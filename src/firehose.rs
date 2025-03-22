use anyhow::{anyhow, Result};
use atrium_api::app::bsky::feed::like::Record as FeedLike;
use atrium_api::app::bsky::feed::post::Record as FeedPost;
use atrium_api::app::bsky::feed::repost::Record as FeedRepost;
use atrium_api::app::bsky::graph::follow::Record as GraphFollow;
use atrium_api::com::atproto::sync::subscribe_repos::{Commit, NSID};
use atrium_repo::blockstore::{AsyncBlockStoreRead, CarStore};
use futures::StreamExt;
use ipld_core::cid::Cid; // Import Cid from ipld_core
use sqlx::{Pool, Postgres};
use std::io::Cursor;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tracing::{debug, error, info, warn};

use crate::stream::frames::Frame;
use crate::subscription::{CommitHandler, Subscription};
use crate::{db, models::BlueskyEvent};

// WebSocket connection wrapper (no changes here)
struct RepoSubscription {
    stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl RepoSubscription {
    async fn new(bgs: &str, _cursor: Option<String>) -> Result<Self> {
        let ws_url = format!("wss://{}/xrpc/{}", bgs, NSID);
        info!("Connecting to firehose at: {}", ws_url);

        let (stream, _) = connect_async(ws_url).await?;
        info!("WebSocket connection established");

        Ok(RepoSubscription { stream })
    }
}

impl Subscription for RepoSubscription {
    async fn next(&mut self) -> Option<anyhow::Result<Frame>> {
        match self.stream.next().await {
            Some(Ok(Message::Binary(data))) => Some(Frame::try_from(&data[..])),
            Some(Ok(_)) => None,
            None => None,
            Some(Err(e)) => Some(Err(anyhow::Error::new(e))),
        }
    }
}

// Helper function with improved error handling for different record types
fn deserialize_record(collection: &str, record_block: &[u8]) -> Result<serde_json::Value> {
    let cursor = Cursor::new(record_block);
    match collection {
        "app.bsky.feed.post" => {
            let post: FeedPost = serde_ipld_dagcbor::from_reader(cursor)?;
            Ok(serde_json::to_value(post)?)
        }
        "app.bsky.feed.like" => {
            let like: FeedLike = serde_ipld_dagcbor::from_reader(cursor)?;
            Ok(serde_json::to_value(like)?)
        }
        "app.bsky.graph.follow" => {
            let follow: GraphFollow = serde_ipld_dagcbor::from_reader(cursor)?;
            // Add debug info to understand structure
            let follow_value = serde_json::to_value(follow)?;
            debug!("Follow record structure: {:?}", follow_value);
            Ok(follow_value)
        }
        "app.bsky.feed.repost" => {
            let repost: FeedRepost = serde_ipld_dagcbor::from_reader(cursor)?;
            Ok(serde_json::to_value(repost)?)
        }
        _ => Err(anyhow!("Unsupported collection type: {}", collection)),
    }
}

// Handler for Commit events (the fix is here)
struct FirehoseHandler {
    event_sender: mpsc::Sender<BlueskyEvent>,
    db_pool: Pool<Postgres>,
}

impl CommitHandler for FirehoseHandler {
    async fn handle_commit(&self, commit: &Commit) -> Result<()> {
        // Only log every 1000 commits - this will show progress without flooding logs
        if commit.seq % 1000 == 0 {
            info!(
                "Processed commit batch: repo={:?} seq={}",
                commit.repo, commit.seq
            );
        }

        // Create a CarStore from the blocks.
        let mut car_store = CarStore::open(Cursor::new(&commit.blocks[..]))
            .await
            .map_err(|e| anyhow!("Failed to create CarStore: {}", e))?;

        for op in &commit.ops {
            if op.action != "create" && op.action != "update" {
                continue;
            }

            let parts: Vec<&str> = op.path.split('/').collect();
            if parts.len() < 2 {
                continue;
            }

            let collection = parts[0];
            let _rkey_str = parts[1];

            let notification_type = match collection {
                "app.bsky.feed.post" => "post",
                "app.bsky.feed.like" => "like",
                "app.bsky.graph.follow" => "follow",
                "app.bsky.feed.repost" => "repost",
                _ => {
                    continue; // Skip unhandled types silently
                }
            };

            // Get the record CID (if present).
            if let Some(cid_link) = &op.cid {
                // Correctly convert CidLink to ipld_core::cid::Cid
                let cid_bytes = cid_link.0.to_bytes();
                let cid = match Cid::try_from(cid_bytes.as_slice()) {
                    Ok(cid) => cid,
                    Err(e) => {
                        error!("Invalid CID format: {}", e);
                        continue;
                    }
                };

                let mut record_block = Vec::new();
                match car_store.read_block_into(cid, &mut record_block).await {
                    Ok(()) => {
                        // Deserialize the record with better error handling
                        let record_data = match deserialize_record(collection, &record_block) {
                            Ok(data) => {
                                // Log record structure only at debug level to understand format
                                if collection == "app.bsky.graph.follow" {
                                    debug!("Follow record structure: {:?}", data);
                                }
                                data
                            }
                            Err(e) => {
                                // Only log deserialization errors at debug level
                                debug!("Failed to deserialize {}: {}", notification_type, e);
                                continue;
                            }
                        };

                        // Create event.
                        let event = BlueskyEvent {
                            op: op.action.clone(),
                            path: op.path.clone(),
                            cid: format!("{:?}", cid_link.0),
                            author: commit.repo.to_string(),
                            record: record_data,
                            timestamp: chrono::Utc::now().timestamp(),
                        };

                        // Send the event without logging success
                        if let Err(e) = self.event_sender.send(event).await {
                            error!("Failed to queue {} event: {}", notification_type, e);
                        }
                    }
                    Err(e) => {
                        debug!(
                            "Record block not found for CID: {:?}, error: {}",
                            cid_link, e
                        );
                    }
                }
            }
        }

        // Update cursor without logging every time
        if let Err(e) = db::update_cursor(&self.db_pool, &commit.seq.to_string()).await {
            error!("Failed to update cursor: {}", e);
        }

        Ok(())
    }
}

pub async fn run_firehose_consumer(
    bsky_service_url: String,
    event_sender: mpsc::Sender<BlueskyEvent>,
    db_pool: Pool<Postgres>,
    mut shutdown: oneshot::Receiver<()>,
) -> Result<()> {
    info!("Starting firehose consumer");

    // Maximum reconnection attempts
    const MAX_RECONNECTS: u32 = 10;
    // Base delay between reconnection attempts (will be exponentially increased)
    let mut reconnect_delay = 1;
    let mut reconnect_attempts = 0;

    'outer: loop {
        // Get last cursor from database for resuming
        let last_cursor = match db::get_last_cursor(&db_pool).await {
            Ok(cursor) => cursor,
            Err(e) => {
                error!("Failed to get last cursor: {}", e);
                None
            }
        };

        info!(
            "Connecting to firehose, starting from cursor: {:?}",
            last_cursor
        );

        // Create subscription with retry logic
        let subscription_result =
            RepoSubscription::new(&bsky_service_url, last_cursor.clone()).await;

        let mut subscription = match subscription_result {
            Ok(sub) => sub,
            Err(e) => {
                error!("Failed to connect to firehose: {}", e);

                // Check if we've reached max reconnect attempts
                reconnect_attempts += 1;
                if reconnect_attempts >= MAX_RECONNECTS {
                    return Err(anyhow!("Max reconnection attempts reached"));
                }

                // Exponential backoff
                let delay = Duration::from_secs(reconnect_delay);
                reconnect_delay = std::cmp::min(reconnect_delay * 2, 60); // Cap at 60 seconds

                info!(
                    "Retrying in {} seconds (attempt {}/{})",
                    delay.as_secs(),
                    reconnect_attempts,
                    MAX_RECONNECTS
                );

                // Wait before retrying, but also check for shutdown signal
                tokio::select! {
                    _ = tokio::time::sleep(delay) => continue 'outer,
                    _ = &mut shutdown => {
                        info!("Received shutdown signal while waiting to reconnect");
                        break 'outer;
                    }
                }
            }
        };

        // Create handler
        let handler = FirehoseHandler {
            event_sender: event_sender.clone(),
            db_pool: db_pool.clone(),
        };

        // Process incoming frames
        'inner: loop {
            tokio::select! {
                Some(frame_result) = subscription.next() => {
                    match frame_result {
                        Ok(Frame::Message(Some(t), message)) => {
                            if t.as_str() == "#commit" {
                                // Parse commit from message
                                match serde_ipld_dagcbor::from_reader::<Commit, _>(&message.body[..]) {
                                    Ok(commit) => {
                                        // Only log occasional commits for processing stats
                                        if commit.seq % 5000 == 0 {
                                            info!("Processing commit at sequence: {}", commit.seq);
                                        }

                                        // Handle commit without flooding logs
                                        if let Err(e) = handler.handle_commit(&commit).await {
                                            error!("Error handling commit: {}", e);
                                        }

                                        // Reset reconnect counter on successful processing
                                        reconnect_attempts = 0;
                                        reconnect_delay = 1;
                                    },
                                    Err(e) => {
                                        error!("Failed to parse commit: {}", e);
                                    }
                                }
                            } else {
                                // Only log non-commit messages
                                debug!("Received message of type: {}", t);
                            }
                        },
                        Ok(Frame::Message(None, _)) => {
                            // Ignore message with no type
                        },
                        Ok(Frame::Error(_)) => {
                            error!("Received error frame from firehose");
                            break 'inner; // Break inner loop to reconnect
                        },
                        Err(e) => {
                            error!("Error parsing frame: {}", e);
                            break 'inner; // Break inner loop to reconnect
                        }
                    }
                },
                _ = &mut shutdown => {
                    info!("Received shutdown signal, stopping firehose consumer");
                    break 'outer; // Break outer loop to exit
                }
            }
        }

        // If we reach here, the inner loop has broken, attempt to reconnect
        warn!("Connection interrupted, attempting to reconnect");
    }

    info!("Firehose consumer stopped");
    Ok(())
}
