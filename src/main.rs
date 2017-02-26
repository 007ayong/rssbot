#![feature(conservative_impl_trait)]

#[macro_use]
extern crate log;
extern crate env_logger;
#[macro_use]
extern crate error_chain;
extern crate serde_json;
#[macro_use]
extern crate serde_derive;
extern crate quick_xml;
extern crate rss;
extern crate atom_syndication as atom;
extern crate curl;
extern crate futures;
extern crate tokio_core;
extern crate tokio_curl;
extern crate telebot;

use std::io::prelude::*;
use std::rc::Rc;
use std::cell::RefCell;
use std::time::Duration;

use telebot::functions::*;
use tokio_core::reactor::{Core, Interval};
use futures::Future;
use futures::Stream;
use tokio_curl::Session;

mod errors;
mod feed;
mod data;
mod telebot_missing;
mod utlis;

use telebot_missing::{get_chat_string, edit_message_text};
use utlis::Escape;

const TELEGRAM_MAX_MSG_LEN: usize = 4096;

fn log_error(e: &errors::Error) {
    warn!("error: {}", e);
    for e in e.iter().skip(1) {
        warn!("caused by: {}", e);
    }
    if let Some(backtrace) = e.backtrace() {
        warn!("backtrace: {:?}", backtrace);
    }
}

fn check_channel<'a>(bot: telebot::RcBot, channel: &str, chat_id: i64, user_id: i64) -> impl Future<Item = Option<i64>, Error = telebot::Error> + 'a {
    let channel = channel.parse::<i64>()
        .map(|_| if !channel.starts_with("-100") {
            format!("-100{}", channel)
        } else {
            channel.to_owned()
        })
        .unwrap_or_else(|_| if !channel.starts_with("@") {
            format!("@{}", channel)
        } else {
            channel.to_owned()
        });
    bot.message(chat_id, "正在验证 Channel".to_string())
        .send()
        .map_err(|e| Some(e))
        .and_then(move |(bot, msg)| {
            let msg_id = msg.message_id;
            get_chat_string(&bot, channel)
                .send()
                .or_else(move |e| -> Box<Future<Item = _, Error = Option<telebot::Error>>> {
                    match e {
                        telebot::Error::Telegram(err_msg) => {
                            Box::new(edit_message_text(&bot,
                                                       chat_id,
                                                       msg_id,
                                                       format!("无法找到目标 Channel: {}", err_msg))
                                .send()
                                .then(|result| match result {
                                    Ok(_) => futures::future::err(None),
                                    Err(e) => futures::future::err(Some(e)),
                                }))
                        }
                        _ => Box::new(futures::future::err(Some(e))),
                    }
                })
                .map(move |(bot, channel)| (bot, channel, msg_id))
        })
        .and_then(move |(bot, channel, msg_id)| -> Box<Future<Item = _, Error = Option<_>>> {
            if channel.kind != "channel" {
                Box::new(bot.message(chat_id, "目标需为 Channel".to_string())
                    .send()
                    .then(|result| match result {
                        Ok(_) => Err(None),
                        Err(e) => Err(Some(e)),
                    }))
            } else {
                let channel_id = channel.id;
                Box::new(bot.unban_chat_administrators(channel_id)
                    .send()
                    .or_else(move |e| -> Box<Future<Item = _, Error = Option<_>>> {
                        match e {
                            telebot::Error::Telegram(error_msg) => {
                                Box::new(edit_message_text(&bot,
                                                           chat_id,
                                                           msg_id,
                                                           format!("请先将本 Bot 加入目标 Channel 并设为管理员: {}",
                                                                   error_msg))
                                    .send()
                                    .then(|result| match result {
                                        Ok(_) => futures::future::err(None),
                                        Err(e) => futures::future::err(Some(e)),
                                    }))
                            }
                            _ => Box::new(futures::future::err(Some(e))),
                        }
                    })
                    .map(move |(bot, admins)| {
                        let admin_id_list = admins.iter().map(|member| member.user.id).collect::<Vec<i64>>();
                        (bot, admin_id_list, msg_id, channel_id)
                    }))
            }
        })
        .and_then(move |(bot, admin_id_list, msg_id, channel_id)| {
            bot.get_me()
                .send()
                .map_err(|e| Some(e))
                .map(move |(bot, me)| (bot, me.id, admin_id_list, msg_id, channel_id))
        })
        .and_then(move |(bot, bot_id, admin_id_list, msg_id, channel_id)| -> Box<Future<Item = i64, Error = Option<_>>> {
            if admin_id_list.contains(&bot_id) {
                if admin_id_list.contains(&user_id) {
                    Box::new(futures::future::ok(channel_id))
                } else {
                    Box::new(edit_message_text(&bot,
                                               chat_id,
                                               msg_id,
                                               "该命令只能由 Channel 管理员使用".to_string())
                        .send()
                        .then(|result| match result {
                            Ok(_) => futures::future::err(None),
                            Err(e) => futures::future::err(Some(e)),
                        }))
                }
            } else {
                Box::new(edit_message_text(&bot,
                                           chat_id,
                                           msg_id,
                                           "请将本 Bot 设为管理员".to_string())
                    .send()
                    .then(|result| match result {
                        Ok(_) => futures::future::err(None),
                        Err(e) => futures::future::err(Some(e)),
                    }))
            }
        })
        .then(|result| match result {
            Err(None) => futures::future::ok(None),
            Err(Some(e)) => futures::future::err(e),
            Ok(ok) => futures::future::ok(Some(ok)),
        })
}

fn to_chinese_error_msg(e: errors::Error) -> String {
    match e {
        errors::Error(errors::ErrorKind::Curl(e), _) => format!("网络错误 ({})", e.into_error()),
        errors::Error(errors::ErrorKind::Utf8(e), _) => format!("编码错误 ({})", e),
        errors::Error(errors::ErrorKind::Xml(e), _) => {
            let msg = truncate_message(format!("{}", e), 500);
            format!("解析错误 ({})", msg)
        }
        _ => format!("{}", e),
    }
}

fn shoud_unsubscribe_for_user(tg_err_msg: &str) -> bool {
    tg_err_msg.contains("Forbidden") || tg_err_msg.contains("chat not found") || tg_err_msg.contains("group chat was migrated to a supergroup chat")
}

fn send_multiple_messages<'a>(bot: &telebot::RcBot, target: i64, messages: &[String]) -> impl Future<Item = (), Error = telebot::Error> + 'a {
    let mut future: Box<Future<Item = telebot::RcBot, Error = telebot::Error>> = Box::new(futures::future::ok(bot.clone()));
    for msg in messages {
        let msg = msg.to_owned();
        future = Box::new(future.and_then(move |bot| {
            bot.message(target, msg)
                .parse_mode("HTML")
                .disable_web_page_preview(true)
                .send()
                .map(|(bot, _)| bot)
        }));
    }
    future.map(|_| ())
}

fn truncate_message(mut s: String, max: usize) -> String {
    if s.len() > max {
        s.truncate(max - 3);
        s.push_str("...");
    }
    s
}

fn format_and_split_msgs<T, F>(head: String, data: &[T], line_format_fn: F) -> Vec<String>
    where F: Fn(&T) -> String
{
    let mut msgs = vec![head];
    for item in data {
        let line = line_format_fn(item);
        if msgs.last_mut().unwrap().len() + line.len() > TELEGRAM_MAX_MSG_LEN {
            msgs.push(line);
        } else {
            let msg = msgs.last_mut().unwrap();
            msg.push('\n');
            msg.push_str(&line);
        }
    }
    msgs
}

fn fetch_feed_updates<'a>(bot: telebot::RcBot,
                          db: Rc<RefCell<data::Database>>,
                          session: Session,
                          feed: data::Feed)
                          -> impl Future<Item = (), Error = ()> + 'a {
    info!("fetching: {} {}", feed.title, feed.link);
    let r = {
        let db = db.clone();
        let bot = bot.clone();
        let feed = feed.clone();
        feed::fetch_feed(&session, &feed.link).or_else(move |e| -> Box<Future<Item = rss::Channel, Error = ()>> {
            if db.borrow_mut().inc_error_count(&feed.link) > 1440 {
                // 1440 * 5 minute = 5 days
                let err_msg = to_chinese_error_msg(e);
                let mut msgs = Vec::with_capacity(feed.subscribers.len());
                for &subscriber in &feed.subscribers {
                    let m = bot.message(subscriber,
                                 format!("《<a href=\"{}\">{}</a>》已经连续 5 天拉取出错 ({})，可能已经关闭，请取消订阅",
                                         feed.link,
                                         Escape(&feed.title),
                                         Escape(&err_msg)))
                        .parse_mode("HTML")
                        .disable_web_page_preview(true)
                        .send();
                    let feed_link = feed.link.clone();
                    let db = db.clone();
                    let bot = bot.clone();
                    let r = m.map_err(move |e| -> Box<Future<Item = (), Error = ()>> {
                        match e {
                            telebot::error::Error::Telegram(ref s) if shoud_unsubscribe_for_user(s) => {
                                db.borrow_mut().unsubscribe(subscriber, &feed_link).unwrap();
                                Box::new(bot.message(subscriber,
                                             format!("无法修复的错误 ({})，自动退订", s))
                                    .send()
                                    .then(|_| Ok(())))
                            }
                            _ => {
                                warn!("failed to send error to {}, {:?}", subscriber, e);
                                futures::future::ok(()).boxed()
                            }
                        }
                    });
                    msgs.push(r);
                }
                Box::new(futures::future::join_all(msgs).then(|_| Err(())))
            } else {
                futures::future::err(()).boxed()
            }
        })
    };
    let r = {
        r.and_then(move |rss| {
            let updates = db.borrow_mut().update(&feed.link, rss.items);
            if updates.is_empty() {
                futures::future::err(())
            } else {
                futures::future::ok((bot, db, feed, rss.title, rss.link, updates))
            }
        })
    };
    r.and_then(|(bot, db, feed, rss_title, rss_link, updates)| {
        let msgs = format_and_split_msgs(format!("<b>{}</b>", rss_title), &updates, |item| {
            let title = item.title.as_ref().map(|s| s.as_str()).unwrap_or_else(|| &rss_title);
            let link = item.link.as_ref().map(|s| s.as_str()).unwrap_or_else(|| &rss_link);
            format!("<a href=\"{}\">{}</a>",
                    link,
                    Escape(&truncate_message(title.to_owned(), TELEGRAM_MAX_MSG_LEN - 500)))
        });

        let mut msg_futures = Vec::with_capacity(feed.subscribers.len());
        for &subscriber in &feed.subscribers {
            let feed_link = feed.link.clone();
            let db = db.clone();
            let bot = bot.clone();
            let r = send_multiple_messages(&bot, subscriber, &msgs).map_err(move |e| -> Box<Future<Item = (), Error = ()>> {
                match e {
                    telebot::error::Error::Telegram(ref s) if shoud_unsubscribe_for_user(s) => {
                        if let Err(e) = db.borrow_mut().unsubscribe(subscriber, &feed_link) {
                            log_error(&e);
                        }
                        Box::new(bot.message(subscriber,
                                     format!("无法修复的错误 ({})，自动退订", s))
                            .send()
                            .then(|_| Ok(())))
                    }
                    _ => {
                        warn!("failed to send updates to {}, {:?}", subscriber, e);
                        futures::future::ok(()).boxed()
                    }
                }
            });
            msg_futures.push(r);
        }
        futures::future::join_all(msg_futures).then(|_| Ok(()))
    })
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        writeln!(&mut std::io::stderr(),
                 "Usage: {} DATAFILE TELEGRAM-BOT-TOKEN",
                 args[0])
            .unwrap();
        std::process::exit(1);
    }
    let datafile = &args[1];
    let token = &args[2];

    let db = Rc::new(RefCell::new(data::Database::open(datafile)
        .map_err(|e| {
            writeln!(&mut std::io::stderr(), "error: {}", e).unwrap();
            for e in e.iter().skip(1) {
                writeln!(&mut std::io::stderr(), "caused by: {}", e).unwrap();
            }
            if let Some(backtrace) = e.backtrace() {
                writeln!(&mut std::io::stderr(), "backtrace: {:?}", backtrace).unwrap();
            }
            std::process::exit(1);
        })
        .unwrap()));

    env_logger::init().unwrap();

    let mut lp = Core::new().unwrap();
    let bot = telebot::RcBot::new(lp.handle(), token).update_interval(200);

    {
        let db = db.clone();
        let handle = bot.new_cmd("/rss")
            .map_err(|e| Some(e))
            .and_then(move |(bot, msg)| -> Box<Future<Item = _, Error = Option<_>>> {
                let text = msg.text.unwrap();
                let args: Vec<&str> = text.split_whitespace().collect();
                let raw: bool;
                let subscriber: Box<Future<Item = Option<i64>, Error = telebot::Error>>;
                match args.len() {
                    0 => {
                        raw = false;
                        subscriber = futures::future::ok(Some(msg.chat.id)).boxed();
                    }
                    1 => {
                        if args[0] == "raw" {
                            raw = true;
                            subscriber = futures::future::ok(Some(msg.chat.id)).boxed();
                        } else {
                            raw = false;
                            let channel = args[0];
                            subscriber = Box::new(check_channel(bot.clone(), channel, msg.chat.id, msg.from.unwrap().id));
                        }
                    }
                    2 => {
                        raw = true;
                        let channel = args[0];
                        subscriber = Box::new(check_channel(bot.clone(), channel, msg.chat.id, msg.from.unwrap().id));
                    }
                    _ => {
                        return Box::new(bot.message(msg.chat.id,
                                     "使用方法： /rss <Channel ID> <raw>".to_string())
                            .send()
                            .then(|result| match result {
                                Ok(_) => Err(None),
                                Err(e) => Err(Some(e)),
                            }))
                    }
                }

                let bot = bot.clone();
                let db = db.clone();
                let chat_id = msg.chat.id;
                Box::new(subscriber.then(|result| match result {
                        Ok(Some(ok)) => Ok(ok),
                        Ok(None) => Err(None),
                        Err(err) => Err(Some(err)),
                    })
                    .map(move |subscriber| (bot, db, subscriber, raw, chat_id)))
            })
            .and_then(|(bot, db, subscriber, raw, chat_id)| {
                let r: Box<Future<Item = _, Error = _>> = match db.borrow().get_subscribed_feeds(subscriber) {
                    Some(feeds) => {
                        let text = String::from("订阅列表:");
                        if !raw {
                            let msgs = format_and_split_msgs(text,
                                                             &feeds,
                                                             |feed| format!("<a href=\"{}\">{}</a>", feed.link, Escape(&feed.title)));
                            Box::new(send_multiple_messages(&bot, chat_id, &msgs))
                        } else {
                            let msgs = format_and_split_msgs(text,
                                                             &feeds,
                                                             |feed| format!("{}: {}", Escape(&feed.title), Escape(&feed.link)));
                            Box::new(send_multiple_messages(&bot, chat_id, &msgs))
                        }
                    }
                    None => {
                        Box::new(bot.message(chat_id, "订阅列表为空".to_string())
                            .send()
                            .map(|_| ()))
                    }
                };
                r.map_err(|e| Some(e))
            })
            .then(|result| match result {
                Ok(_) => Ok(()),
                Err(None) => Ok(()),
                Err(Some(err)) => Err(err),
            });

        bot.register(handle);
    }
    {
        let db = db.clone();
        let lphandle = lp.handle();
        let handle = bot.new_cmd("/sub")
            .map_err(|e| Some(e))
            .and_then(move |(bot, msg)| -> Box<Future<Item = _, Error = Option<_>>> {
                let text = msg.text.unwrap();
                let args: Vec<&str> = text.split_whitespace().collect();
                let feed_link: &str;
                let subscriber: Box<Future<Item = Option<i64>, Error = telebot::Error>>;
                match args.len() {
                    1 => {
                        feed_link = args[0];
                        subscriber = futures::future::ok(Some(msg.chat.id)).boxed();
                    }
                    2 => {
                        let channel = args[0];
                        subscriber = Box::new(check_channel(bot.clone(), channel, msg.chat.id, msg.from.unwrap().id));
                        feed_link = args[1];
                    }
                    _ => {
                        return Box::new(bot.message(msg.chat.id,
                                     "使用方法： /sub [Channel ID] <RSS URL>".to_string())
                            .send()
                            .then(|result| match result {
                                Ok(_) => Err(None),
                                Err(e) => Err(Some(e)),
                            }))
                    }
                }

                let bot = bot.clone();
                let db = db.clone();
                let feed_link = feed_link.to_owned();
                let chat_id = msg.chat.id;
                let lphandle = lphandle.clone();
                Box::new(subscriber.then(|result| match result {
                        Ok(Some(ok)) => Ok(ok),
                        Ok(None) => Err(None),
                        Err(err) => Err(Some(err)),
                    })
                    .map(move |subscriber| (bot, db, subscriber, feed_link, chat_id, lphandle)))
            })
            .and_then(|(bot, db, subscriber, feed_link, chat_id, lphandle)| -> Box<Future<Item = _, Error = _>> {
                if db.borrow().is_subscribed(subscriber, &feed_link) {
                    Box::new(bot.message(chat_id, "已订阅过的 RSS".to_string())
                        .send()
                        .then(|result| match result {
                            Ok(_) => Err(None),
                            Err(e) => Err(Some(e)),
                        }))
                } else {
                    Box::new(futures::future::ok((bot, db, subscriber, feed_link, chat_id, lphandle)))
                }
            })
            .and_then(|(bot, db, subscriber, feed_link, chat_id, lphandle)| {
                let session = Session::new(lphandle);
                let bot2 = bot.clone();
                feed::fetch_feed(&session, &feed_link)
                    .map(move |feed| (bot2, db, subscriber, feed_link, chat_id, feed))
                    .or_else(move |e| {
                        bot.message(chat_id,
                                     format!("订阅失败: {}", to_chinese_error_msg(e)))
                            .send()
                            .then(|result| match result {
                                Ok(_) => Err(None),
                                Err(e) => Err(Some(e)),
                            })
                    })
            })
            .and_then(|(bot, db, subscriber, feed_link, chat_id, feed)| {
                let r = match db.borrow_mut().subscribe(subscriber, &feed_link, &feed) {
                    Ok(_) => {
                        bot.message(chat_id,
                                     format!("《<a href=\"{}\">{}</a>》订阅成功",
                                             feed.link,
                                             Escape(&feed.title)))
                            .parse_mode("HTML")
                            .disable_web_page_preview(true)
                            .send()
                    }
                    Err(e) => {
                        log_error(&e);
                        bot.message(chat_id, format!("error: {}", e)).send()
                    }
                };
                r.map_err(|e| Some(e))
            })
            .then(|result| match result {
                Ok(_) => Ok(()),
                Err(None) => Ok(()),
                Err(Some(err)) => Err(err),
            });

        bot.register(handle);
    }
    {
        let db = db.clone();
        let handle = bot.new_cmd("/unsub")
            .map_err(|e| Some(e))
            .and_then(move |(bot, msg)| -> Box<Future<Item = _, Error = Option<_>>> {
                let text = msg.text.unwrap();
                let args: Vec<&str> = text.split_whitespace().collect();
                let feed_link: &str;
                let subscriber: Box<Future<Item = Option<i64>, Error = telebot::Error>>;
                match args.len() {
                    1 => {
                        feed_link = args[0];
                        subscriber = futures::future::ok(Some(msg.chat.id)).boxed();
                    }
                    2 => {
                        let channel = args[0];
                        subscriber = Box::new(check_channel(bot.clone(), channel, msg.chat.id, msg.from.unwrap().id));
                        feed_link = args[1];
                    }
                    _ => {
                        return Box::new(bot.message(msg.chat.id,
                                     "使用方法： /unsub [Channel ID] <RSS URL>".to_string())
                            .send()
                            .then(|result| match result {
                                Ok(_) => Err(None),
                                Err(e) => Err(Some(e)),
                            }))
                    }
                }
                let bot = bot.clone();
                let db = db.clone();
                let feed_link = feed_link.to_owned();
                let chat_id = msg.chat.id;
                Box::new(subscriber.then(|result| match result {
                        Ok(Some(ok)) => Ok(ok),
                        Ok(None) => Err(None),
                        Err(err) => Err(Some(err)),
                    })
                    .map(move |subscriber| (bot, db, subscriber, feed_link, chat_id)))
            })
            .and_then(|(bot, db, subscriber, feed_link, chat_id)| {
                let r = match db.borrow_mut().unsubscribe(subscriber, &feed_link) {
                    Ok(feed) => {
                        bot.message(chat_id,
                                     format!("《<a href=\"{}\">{}</a>》退订成功",
                                             feed.link,
                                             Escape(&feed.title)))
                            .parse_mode("HTML")
                            .disable_web_page_preview(true)
                            .send()
                    }
                    Err(errors::Error(errors::ErrorKind::NotSubscribed, _)) => bot.message(chat_id, "未订阅过的 Feed".to_string()).send(),
                    Err(e) => {
                        log_error(&e);
                        bot.message(chat_id, format!("error: {}", e)).send()
                    }
                };
                r.map_err(|e| Some(e))
            })
            .then(|result| match result {
                Ok(_) => Ok(()),
                Err(None) => Ok(()),
                Err(Some(err)) => Err(err),
            });

        bot.register(handle);
    }

    {
        let session = Session::new(lp.handle());
        let handle = lp.handle();
        let bot = bot.clone();
        lp.handle().spawn(Interval::new(Duration::from_secs(10), &lp.handle())
            .expect("failed to start feed loop")
            .for_each(move |_| {
                let feeds = db.borrow().get_all_feeds();
                for feed in feeds {
                    handle.spawn(fetch_feed_updates(bot.clone(), db.clone(), session.clone(), feed));
                }
                Ok(())
            })
            .map_err(|e| error!("feed loop error: {}", e)))
    }

    loop {
        if let Err(err) = lp.run(bot.get_stream().for_each(|_| Ok(()))) {
            error!("telebot: {:?}", err);
        }
    }
}
