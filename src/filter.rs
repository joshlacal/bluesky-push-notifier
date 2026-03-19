use anyhow::Result;
use sqlx::{Pool, Postgres};
use std::collections::HashSet;
use tokio::sync::mpsc;
use tracing::{debug, error, info};

use crate::{
    db,
    models::{ActivitySubscriptionKind, BlueskyEvent, NotificationType},
};

pub async fn run_event_filter(
    mut event_receiver: mpsc::Receiver<BlueskyEvent>,
    db_pool: Pool<Postgres>,
) -> Result<()> {
    info!("Starting event filter (queue enqueuer mode)");

    // Cache of registered users to avoid frequent DB lookups
    let mut registered_users_vec = match db::get_registered_users(&db_pool).await {
        Ok(users) => users,
        Err(e) => {
            error!(
                "Failed to load initial registered users, starting with empty cache: {}",
                e
            );
            Vec::new()
        }
    };
    let mut registered_users: HashSet<String> = registered_users_vec.iter().cloned().collect();
    let mut last_cache_refresh = std::time::Instant::now();

    while let Some(event) = event_receiver.recv().await {
        let timer = std::time::Instant::now();
        crate::metrics::EVENTS_PROCESSED.inc();

        // Refresh user cache every 5 minutes
        if last_cache_refresh.elapsed().as_secs() > 300 {
            match db::get_registered_users(&db_pool).await {
                Ok(users) => {
                    registered_users_vec = users;
                    registered_users = registered_users_vec.iter().cloned().collect();
                    last_cache_refresh = std::time::Instant::now();
                    debug!(
                        "Refreshed registered users cache, count: {}",
                        registered_users.len()
                    );
                }
                Err(e) => error!("Failed to refresh user cache: {}", e),
            }
        }

        if registered_users.is_empty() {
            continue;
        }

        if !is_notification_relevant_event(&event.path) {
            continue;
        }

        // Classify the event and determine recipients
        let classification = classify_event(&event, &registered_users_vec, &registered_users);

        let is_post_event = event.path.contains("app.bsky.feed.post");
        let is_reply_post = is_post_event && event.record.get("reply").is_some();

        let mut enqueue_batches: Vec<(NotificationType, Vec<String>)> = Vec::new();

        if let Some((notification_type, relevant_dids)) = classification {
            if !relevant_dids.is_empty() {
                enqueue_batches.push((notification_type, relevant_dids));
            }
        }

        // Check activity subscriptions for post events
        if is_post_event {
            match db::get_activity_subscribers(&db_pool, &event.author).await {
                Ok(subscribers) => {
                    let mut targets = Vec::new();
                    for (subscriber_did, include_posts, include_replies) in subscribers {
                        if subscriber_did == event.author {
                            continue;
                        }
                        if !registered_users.contains(&subscriber_did) {
                            continue;
                        }
                        let include = if is_reply_post {
                            include_replies
                        } else {
                            include_posts
                        };
                        if include {
                            targets.push(subscriber_did);
                        }
                    }
                    if !targets.is_empty() {
                        targets.sort();
                        targets.dedup();
                        let kind = if is_reply_post {
                            ActivitySubscriptionKind::Reply
                        } else {
                            ActivitySubscriptionKind::Post
                        };
                        enqueue_batches
                            .push((NotificationType::ActivitySubscription(kind), targets));
                    }
                }
                Err(e) => {
                    error!(
                        author = %event.author,
                        error = %e,
                        "Failed to fetch activity subscribers"
                    );
                }
            }
        }

        if enqueue_batches.is_empty() {
            continue;
        }

        // Extract URIs for the queue row
        let subject_uri = extract_subject_uri(&event);
        let thread_root_uri = extract_thread_root_uri(&event);

        // Enqueue events into nest's push_event_queue
        for (notification_type, dids) in &enqueue_batches {
            let type_str = notification_type.as_queue_str();

            for did in dids {
                if did == &event.author {
                    continue;
                }

                let dedupe_key = format!("{}:{}:{}", event.cid, did, type_str);

                match db::enqueue_push_event(
                    &db_pool,
                    did,
                    &event.author,
                    type_str,
                    &event.cid,
                    &event.path,
                    subject_uri.as_deref(),
                    thread_root_uri.as_deref(),
                    &event.record,
                    event.timestamp,
                    &dedupe_key,
                )
                .await
                {
                    Ok(_) => {
                        crate::metrics::EVENTS_ENQUEUED.inc();
                    }
                    Err(e) => {
                        error!(
                            recipient = %did,
                            notification_type = %type_str,
                            error = %e,
                            "Failed to enqueue push event"
                        );
                    }
                }
            }
        }

        let elapsed = timer.elapsed().as_secs_f64();
        crate::metrics::EVENT_PROCESSING_TIME.observe(elapsed);
    }

    info!("Event filter stopped");
    Ok(())
}

/// Extract subject URI from event record.
fn extract_subject_uri(event: &BlueskyEvent) -> Option<String> {
    // Likes and reposts: subject.uri
    if event.path.contains("app.bsky.feed.like") || event.path.contains("app.bsky.feed.repost") {
        return event
            .record
            .get("subject")
            .and_then(|s| s.get("uri"))
            .and_then(|u| u.as_str())
            .map(String::from);
    }

    // Follows: subject DID as at:// URI
    if event.path.contains("app.bsky.graph.follow") {
        return event
            .record
            .get("subject")
            .and_then(|s| s.as_str())
            .map(|did| format!("at://{}", did));
    }

    if event.path.contains("app.bsky.feed.post") {
        // Quote posts: embed.record.uri
        if let Some(embed) = event.record.get("embed") {
            if let Some(uri) = embed
                .get("record")
                .and_then(|r| r.get("uri"))
                .and_then(|u| u.as_str())
            {
                return Some(uri.to_string());
            }
        }

        // Replies: parent URI
        if let Some(parent_uri) = event
            .record
            .get("reply")
            .and_then(|r| r.get("parent"))
            .and_then(|p| p.get("uri"))
            .and_then(|u| u.as_str())
        {
            return Some(parent_uri.to_string());
        }
    }

    None
}

/// Extract thread root URI from event record (for replies).
fn extract_thread_root_uri(event: &BlueskyEvent) -> Option<String> {
    event
        .record
        .get("reply")
        .and_then(|r| r.get("root"))
        .and_then(|p| p.get("uri"))
        .and_then(|u| u.as_str())
        .map(String::from)
}

// Quick check for notification-relevant events to avoid processing irrelevant ones
fn is_notification_relevant_event(path: &str) -> bool {
    path.contains("app.bsky.feed.post")
        || path.contains("app.bsky.feed.like")
        || path.contains("app.bsky.graph.follow")
        || path.contains("app.bsky.feed.repost")
}

fn classify_event(
    event: &BlueskyEvent,
    registered_users: &[String],
    registered_users_set: &HashSet<String>,
) -> Option<(NotificationType, Vec<String>)> {
    if registered_users.is_empty() {
        return None;
    }

    let (notification_type, relevant_dids) = match event.path.as_str() {
        path if path.contains("app.bsky.feed.post") => {
            if has_quote_embed(&event.record) {
                let quoted_dids = find_quoted_users(event, registered_users);
                if !quoted_dids.is_empty() {
                    (NotificationType::Quote, quoted_dids)
                } else if event.record.get("reply").is_some() {
                    let relevant_dids = extract_target_dids(event, registered_users);
                    if !relevant_dids.is_empty() {
                        (NotificationType::Reply, relevant_dids)
                    } else {
                        let mentioned_dids =
                            extract_mention_dids(event, registered_users, registered_users_set);
                        if !mentioned_dids.is_empty() {
                            (NotificationType::Mention, mentioned_dids)
                        } else {
                            return None;
                        }
                    }
                } else {
                    let mentioned_dids =
                        extract_mention_dids(event, registered_users, registered_users_set);
                    if !mentioned_dids.is_empty() {
                        (NotificationType::Mention, mentioned_dids)
                    } else {
                        return None;
                    }
                }
            } else if event.record.get("reply").is_some() {
                let relevant_dids = extract_target_dids(event, registered_users);
                if !relevant_dids.is_empty() {
                    (NotificationType::Reply, relevant_dids)
                } else {
                    let mentioned_dids =
                        extract_mention_dids(event, registered_users, registered_users_set);
                    if !mentioned_dids.is_empty() {
                        (NotificationType::Mention, mentioned_dids)
                    } else {
                        return None;
                    }
                }
            } else {
                let mentioned_dids =
                    extract_mention_dids(event, registered_users, registered_users_set);
                if !mentioned_dids.is_empty() {
                    (NotificationType::Mention, mentioned_dids)
                } else {
                    return None;
                }
            }
        }
        path if path.contains("app.bsky.feed.like") => {
            if let Some(via_dids) = extract_via_dids(event, registered_users) {
                (NotificationType::ViaLike, via_dids)
            } else {
                let relevant_dids = extract_target_dids(event, registered_users);
                (NotificationType::Like, relevant_dids)
            }
        }
        path if path.contains("app.bsky.graph.follow") => {
            let relevant_dids = extract_target_dids(event, registered_users);
            (NotificationType::Follow, relevant_dids)
        }
        path if path.contains("app.bsky.feed.repost") => {
            if let Some(via_dids) = extract_via_dids(event, registered_users) {
                (NotificationType::ViaRepost, via_dids)
            } else {
                let relevant_dids = extract_target_dids(event, registered_users);
                (NotificationType::Repost, relevant_dids)
            }
        }
        _ => return None,
    };

    if relevant_dids.is_empty() {
        None
    } else {
        info!(
            notification_type = ?notification_type,
            relevant_dids_count = relevant_dids.len(),
            "Preparing notification"
        );
        Some((notification_type, relevant_dids))
    }
}

fn has_quote_embed(record: &serde_json::Value) -> bool {
    if let Some(embed) = record.get("embed") {
        if embed.get("record").is_some() {
            return true;
        }
        if let Some(embed_type) = embed.get("$type").and_then(|t| t.as_str()) {
            return embed_type == "app.bsky.embed.record"
                || embed_type == "app.bsky.embed.recordWithMedia";
        }
    }
    false
}

fn find_quoted_users(event: &BlueskyEvent, registered_users: &[String]) -> Vec<String> {
    let mut quoted_dids = Vec::new();

    if let Some(embed) = event.record.get("embed") {
        if let Some(record_obj) = embed.get("record") {
            extract_quoted_dids(record_obj, registered_users, &mut quoted_dids);
        }

        if embed.get("$type").and_then(|t| t.as_str()) == Some("app.bsky.embed.recordWithMedia") {
            if let Some(record_obj) = embed.get("record") {
                extract_quoted_dids(record_obj, registered_users, &mut quoted_dids);
            }
        }
    }

    quoted_dids
}

fn extract_quoted_dids(
    record_obj: &serde_json::Value,
    registered_users: &[String],
    result: &mut Vec<String>,
) {
    if let Some(uri) = record_obj
        .get("record")
        .and_then(|r| r.get("uri").and_then(|u| u.as_str()))
    {
        for user in registered_users {
            if uri.contains(user) && !result.contains(user) {
                result.push(user.to_string());
            }
        }
    }

    if let Some(uri) = record_obj.get("uri").and_then(|u| u.as_str()) {
        for user in registered_users {
            if uri.contains(user) && !result.contains(user) {
                result.push(user.to_string());
            }
        }
    }
}

fn extract_mention_dids(
    event: &BlueskyEvent,
    _registered_users: &[String],
    registered_users_set: &HashSet<String>,
) -> Vec<String> {
    let mut mentioned_dids = Vec::new();

    if let Some(facets) = event.record.get("facets").and_then(|f| f.as_array()) {
        for facet in facets {
            if let Some(features) = facet.get("features").and_then(|f| f.as_array()) {
                for feature in features {
                    if let Some(feature_type) = feature.get("$type").and_then(|t| t.as_str()) {
                        if feature_type == "app.bsky.richtext.facet#mention" {
                            if let Some(did) = feature.get("did").and_then(|d| d.as_str()) {
                                if registered_users_set.contains(did)
                                    && !mentioned_dids.contains(&did.to_string())
                                {
                                    mentioned_dids.push(did.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    mentioned_dids
}

fn extract_target_dids(event: &BlueskyEvent, registered_users: &[String]) -> Vec<String> {
    if event.path.contains("app.bsky.graph.follow") {
        if let Some(subject) = event.record.get("subject").and_then(|s| s.as_str()) {
            return registered_users
                .iter()
                .filter(|did| subject == *did)
                .cloned()
                .collect();
        }
    } else if event.path.contains("app.bsky.feed.like")
        || event.path.contains("app.bsky.feed.repost")
    {
        if let Some(subject) = event.record.get("subject").and_then(|s| s.as_object()) {
            if let Some(uri) = subject.get("uri").and_then(|u| u.as_str()) {
                return registered_users
                    .iter()
                    .filter(|did| uri.contains(did.as_str()))
                    .cloned()
                    .collect();
            }
        }
    } else if event.path.contains("app.bsky.feed.post") {
        if let Some(reply) = event.record.get("reply").and_then(|r| r.as_object()) {
            if let Some(parent) = reply.get("parent").and_then(|p| p.as_object()) {
                if let Some(uri) = parent.get("uri").and_then(|u| u.as_str()) {
                    let reply_targets = registered_users
                        .iter()
                        .filter(|did| uri.contains(did.as_str()))
                        .cloned()
                        .collect::<Vec<String>>();

                    if !reply_targets.is_empty() {
                        return reply_targets;
                    }
                }
            }
        }
    }

    Vec::new()
}

fn extract_via_dids(event: &BlueskyEvent, registered_users: &[String]) -> Option<Vec<String>> {
    if let Some(via) = event.record.get("via") {
        if let Some(via_uri) = via.get("uri").and_then(|u| u.as_str()) {
            let via_dids: Vec<String> = registered_users
                .iter()
                .filter(|did| via_uri.contains(did.as_str()))
                .cloned()
                .collect();

            if !via_dids.is_empty() {
                debug!(
                    via_uri = %via_uri,
                    via_dids_count = via_dids.len(),
                    "Found via notification recipients"
                );
                return Some(via_dids);
            }
        }
    }

    None
}
