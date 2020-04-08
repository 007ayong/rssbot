use std::cmp;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tbot::{connectors::Https, types::parameters};
use tokio::{
    self, select,
    stream::StreamExt,
    sync::Notify,
    time::{self, delay_queue::DelayQueue, Duration, Instant},
};

use crate::client::pull_feed;
use crate::data::{Database, Feed, FeedUpdate};
use crate::messages::{format_large_msg, Escape};

pub fn start(
    bot: tbot::Bot<tbot::connectors::Https>,
    db: Arc<Mutex<Database>>,
    min_interval: u32,
    max_interval: u32,
) {
    let mut queue = FetchQueue::new();
    // TODO: Don't use interval, it can accumulate ticks
    // replace it with delay_until
    let mut interval = time::interval_at(Instant::now(), Duration::from_secs(min_interval as u64));
    tokio::spawn(async move {
        loop {
            select! {
                _ = interval.tick() => {
                    let feeds = db.lock().unwrap().all_feeds();
                    for feed in feeds {
                        let feed_interval =
                            cmp::min(feed.ttl.map(|ttl| ttl * 60).unwrap_or(min_interval), max_interval) as u64;
                        queue.enqueue(feed, Duration::from_secs(feed_interval));
                    }
                }
                feed = queue.next() => {
                    let feed = feed.expect("unreachable");
                    let bot = bot.clone();
                    let db = db.clone();
                    tokio::spawn(async move {
                        if let Err(e) = fetch_and_push_updates(bot, db, feed).await {
                            dbg!(e);
                        }
                    });
                }
            }
        }
    });
}

async fn fetch_and_push_updates(
    bot: tbot::Bot<tbot::connectors::Https>,
    db: Arc<Mutex<Database>>,
    feed: Feed,
) -> Result<(), tbot::errors::MethodCall> {
    let new_feed = match pull_feed(&feed.link).await {
        Ok(feed) => feed,
        Err(e) => {
            let down_time = db.lock().unwrap().get_or_update_down_time(&feed.link);
            // 5 days
            if down_time.as_secs() > 5 * 24 * 60 * 60 {
                let msg = format!(
                    "《<a href=\"{}\">{}</a>》\
                     已经连续 5 天拉取出错 ({}),\
                     可能已经关闭, 请取消订阅",
                    Escape(&feed.link),
                    Escape(&feed.title),
                    Escape(&e.to_string())
                );
                push_updates(&bot, &db, feed.subscribers, parameters::Text::html(&msg)).await?;
            }
            return Ok(());
        }
    };

    let updates = db.lock().unwrap().update(&feed.link, new_feed);
    for update in updates {
        match update {
            FeedUpdate::Items(items) => {
                let msgs =
                    format_large_msg(format!("<b>{}</b>", Escape(&feed.title)), &items, |item| {
                        let title = item
                            .title
                            .as_ref()
                            .map(|s| s.as_str())
                            .unwrap_or_else(|| &feed.title);
                        let link = item
                            .link
                            .as_ref()
                            .map(|s| s.as_str())
                            .unwrap_or_else(|| &feed.link);
                        format!("<a href=\"{}\">{}</a>", Escape(link), Escape(title))
                    });
                for msg in msgs {
                    push_updates(
                        &bot,
                        &db,
                        feed.subscribers.iter().copied(),
                        parameters::Text::html(&msg),
                    )
                    .await?;
                }
            }
            FeedUpdate::Title(new_title) => {
                let msg = format!(
                    "<a href=\"{}\">{}</a> 已更名为 {}",
                    Escape(&feed.link),
                    Escape(&feed.title),
                    Escape(&new_title)
                );
                push_updates(
                    &bot,
                    &db,
                    feed.subscribers.iter().copied(),
                    parameters::Text::html(&msg),
                )
                .await?;
            }
        }
    }
    Ok(())
}

async fn push_updates<I: IntoIterator<Item = i64>>(
    bot: &tbot::Bot<Https>,
    db: &Arc<Mutex<Database>>,
    subscribers: I,
    msg: parameters::Text<'_>,
) -> Result<(), tbot::errors::MethodCall> {
    use tbot::errors::MethodCall;
    for mut subscriber in subscribers {
        'retry: for _ in 0..3 {
            match bot
                .send_message(tbot::types::chat::Id(subscriber), msg)
                .call()
                .await
            {
                Err(MethodCall::RequestError { description, .. })
                    if chat_is_unavailable(&description) =>
                {
                    db.lock().unwrap().delete_subscriber(subscriber);
                }
                Err(MethodCall::RequestError {
                    migrate_to_chat_id: Some(new_chat_id),
                    ..
                }) => {
                    db.lock()
                        .unwrap()
                        .update_subscriber(subscriber, new_chat_id.0);
                    subscriber = new_chat_id.0;
                    continue 'retry;
                }
                Err(MethodCall::RequestError {
                    retry_after: Some(delay),
                    ..
                }) => {
                    time::delay_for(Duration::from_secs(delay)).await;
                    continue 'retry;
                }
                other => {
                    other?;
                }
            }
            break 'retry;
        }
    }
    Ok(())
}

pub fn chat_is_unavailable(s: &str) -> bool {
    s.contains("Forbidden") || s.contains("chat not found")
}

#[derive(Default)]
struct FetchQueue {
    feeds: HashMap<String, Feed>,
    notifies: DelayQueue<String>,
    wakeup: Notify,
}

impl FetchQueue {
    fn new() -> Self {
        Self::default()
    }

    fn enqueue(&mut self, feed: Feed, delay: Duration) -> bool {
        let exists = self.feeds.contains_key(&feed.link);
        if !exists {
            self.notifies.insert(feed.link.clone(), delay);
            self.feeds.insert(feed.link.clone(), feed);
            self.wakeup.notify();
        }
        !exists
    }

    async fn next(&mut self) -> Result<Feed, time::Error> {
        loop {
            if let Some(feed_id) = self.notifies.next().await {
                let feed = self.feeds.remove(feed_id?.get_ref()).unwrap();
                break Ok(feed);
            } else {
                self.wakeup.notified().await;
            }
        }
    }
}
