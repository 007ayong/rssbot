use std::time::Duration;
use std::collections::HashMap;

use telebot;
use telebot::functions::*;
use tokio_core::reactor::{Interval, Handle, Timeout};
use futures::{self, Future, Stream, IntoFuture};
use tokio_curl::Session;
use regex::Regex;

use data;
use feed;
use utlis::{Escape, EscapeUrl, send_multiple_messages, format_and_split_msgs,
            to_chinese_error_msg, truncate_message, chat_is_unavailable, TELEGRAM_MAX_MSG_LEN};

// 5 minute
const FREQUENCY_SECOND: u64 = 300;

lazy_static!{
    // it's different from `feed::HOST`, so maybe need a better name?
    static ref HOST: Regex = Regex::new(r"^(?:https?://)?([^/]+)").unwrap();
}

pub fn spawn_fetcher(bot: telebot::RcBot, db: data::Database, handle: Handle) {
    handle.clone().spawn(
        Interval::new(Duration::from_secs(FREQUENCY_SECOND), &handle)
            .expect("failed to start feed loop")
            .map_err(|e| error!("feed loop error: {}", e))
            .for_each(move |_| {
                let feeds = db.get_all_feeds();
                let grouped_feeds = grouping_by_host(feeds);
                let handle2 = handle.clone();
                let bot = bot.clone();
                let db = db.clone();
                let fetcher = futures::stream::iter(grouped_feeds.into_iter().map(Ok))
                    .for_each(move |group| {
                        let session = Session::new(handle2.clone());
                        let bot = bot.clone();
                        let db = db.clone();
                        let group_fetcher = futures::stream::iter(group.into_iter().map(Ok))
                            .for_each(move |feed| {
                                fetch_feed_updates(bot.clone(), db.clone(), &session, feed)
                                    .then(|_| Ok(()))
                            });
                        handle2.spawn(group_fetcher);
                        Timeout::new(Duration::from_secs(1), &handle2)
                            .expect("failed to start sleep")
                            .map_err(|e| error!("feed loop sleep error: {}", e))
                    });
                handle.spawn(fetcher);
                Ok(())
            }),
    )
}

fn grouping_by_host(feeds: Vec<data::Feed>) -> Vec<Vec<data::Feed>> {
    let mut result = HashMap::new();
    for feed in feeds {
        let host = get_host(&feed.link).to_owned();
        let group = result.entry(host).or_insert_with(Vec::new);
        group.push(feed);
    }
    result.into_iter().map(|(_, v)| v).collect()
}

fn get_host(url: &str) -> &str {
    HOST.captures(url).map_or(
        url,
        |r| r.get(0).unwrap().as_str(),
    )
}

fn fetch_feed_updates<'a>(
    bot: telebot::RcBot,
    db: data::Database,
    session: &Session,
    feed: data::Feed,
) -> impl Future<Item = (), Error = ()> + 'a {
    let bot_ = bot.clone();
    let db_ = db.clone();
    let feed_ = feed.clone();
    feed::fetch_feed(session, feed.link.to_owned())
        .map(move |rss| (bot_, db_, rss, feed_))
        .or_else(move |e| {
            // 1440 * 5 minute = 5 days
            if db.inc_error_count(&feed.link) > 1440 {
                Err((bot, db, feed))
            } else {
                Ok(())
            }.into_future()
                .or_else(|(bot, db, feed)| {
                    db.reset_error_count(&feed.link);
                    let err_msg = to_chinese_error_msg(e);
                    let mut msgs = Vec::with_capacity(feed.subscribers.len());
                    for &subscriber in &feed.subscribers {
                        let m = bot.message(
                            subscriber,
                            format!(
                                "《<a href=\"{}\">{}</a>》\
                                                     已经连续 5 天拉取出错 ({}),\
                                                     可能已经关闭, 请取消订阅",
                                EscapeUrl(&feed.link),
                                Escape(&feed.title),
                                Escape(&err_msg)
                            ),
                        ).parse_mode("HTML")
                            .disable_web_page_preview(true)
                            .send();
                        let db = db.clone();
                        let r = m.map_err(move |e| {
                            match e {
                                telebot::error::Error::Telegram(ref s)
                                    if chat_is_unavailable(s) => {
                                    db.delete_subscriber(subscriber);
                                }
                                _ => {
                                    warn!("failed to send error to {}, {:?}", subscriber, e);
                                }
                            };
                        });
                        // if not use Box, rustc will panic
                        msgs.push(Box::new(r) as Box<Future<Item = _, Error = _>>);
                    }
                    futures::future::join_all(msgs).then(|_| Err(()))
                })
                .and_then(|_| Err(()))
        })
        .and_then(|(bot, db, rss, feed)| {
            if rss.title != feed.title {
                db.update_title(&feed.link, &rss.title);
            }
            let updates = db.update(&feed.link, rss.items);
            if updates.is_empty() {
                futures::future::err(())
            } else {
                futures::future::ok((bot, db, feed, rss.title, rss.link, updates))
            }
        })
        .and_then(|(bot, db, feed, rss_title, rss_link, updates)| {
            let msgs =
                format_and_split_msgs(format!("<b>{}</b>", Escape(&rss_title)), &updates, |item| {
                    let title = item.title.as_ref().map(|s| s.as_str()).unwrap_or_else(
                        || &rss_title,
                    );
                    let link = item.link.as_ref().map(|s| s.as_str()).unwrap_or_else(
                        || &rss_link,
                    );
                    format!(
                        "<a href=\"{}\">{}</a>",
                        EscapeUrl(link),
                        Escape(&truncate_message(title, TELEGRAM_MAX_MSG_LEN - 500))
                    )
                });

            let mut msg_futures = Vec::with_capacity(feed.subscribers.len());
            for &subscriber in &feed.subscribers {
                let db = db.clone();
                let bot = bot.clone();
                let r = send_multiple_messages(&bot, subscriber, msgs.clone()).map_err(move |e| {
                    match e {
                        telebot::error::Error::Telegram(ref s) if chat_is_unavailable(s) => {
                            db.delete_subscriber(subscriber);
                        }
                        _ => {
                            warn!("failed to send updates to {}, {:?}", subscriber, e);
                        }
                    };
                });
                msg_futures.push(Box::new(r) as Box<Future<Item = _, Error = _>>);
            }
            futures::future::join_all(msg_futures).then(|_| Ok(()))
        })
}
