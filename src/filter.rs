use anyhow::Result;
use sqlx::{Pool, Postgres};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::{
    activity_subscription_manager::ActivitySubscriptionManager,
    db,
    models::{ActivitySubscriptionKind, BlueskyEvent, NotificationPayload, NotificationType},
};

use crate::post_resolver::PostResolver;

pub async fn run_event_filter(
    mut event_receiver: mpsc::Receiver<BlueskyEvent>,
    notification_sender: mpsc::Sender<NotificationPayload>,
    db_pool: Pool<Postgres>,
    did_resolver: Arc<crate::did_resolver::DidResolver>,
    post_resolver: Arc<crate::post_resolver::PostResolver>,
    relationship_manager: Arc<crate::relationship_manager::RelationshipManager>,
    activity_subscription_manager: Arc<ActivitySubscriptionManager>,
) -> Result<()> {
    info!("Starting event filter");

    // Cache of registered users to avoid frequent DB lookups
    let mut registered_users = db::get_registered_users(&db_pool).await?;
    let mut last_cache_refresh = std::time::Instant::now();

    while let Some(event) = event_receiver.recv().await {
        // Create timer to measure event processing time
        let timer = std::time::Instant::now();
        crate::metrics::EVENTS_PROCESSED.inc();

        // Refresh user cache every 5 minutes
        if last_cache_refresh.elapsed().as_secs() > 300 {
            match db::get_registered_users(&db_pool).await {
                Ok(users) => {
                    registered_users = users;
                    last_cache_refresh = std::time::Instant::now();
                    debug!(
                        "Refreshed registered users cache, count: {}",
                        registered_users.len()
                    );
                }
                Err(e) => error!("Failed to refresh user cache: {}", e),
            }
        }

        let classification = classify_event(&event, &registered_users);

        let is_post_event = event.path.contains("app.bsky.feed.post");
        let is_reply_post = is_post_event && event.record.get("reply").is_some();

        let mut subscription_targets = Vec::new();
        if is_post_event {
            match activity_subscription_manager
                .list_subscribers_for_subject(&event.author)
                .await
            {
                Ok(subscribers) => {
                    for sub in subscribers {
                        if sub.subscriber_did == event.author {
                            continue;
                        }

                        if !registered_users.contains(&sub.subscriber_did) {
                            continue;
                        }

                        let include = if is_reply_post {
                            sub.include_replies
                        } else {
                            sub.include_posts
                        };

                        if include {
                            subscription_targets.push(sub.subscriber_did);
                        }
                    }
                }
                Err(e) => {
                    error!(
                        author = %event.author,
                        error = %e,
                        "Failed to fetch activity subscription targets"
                    );
                }
            }
        }

        let mut notification_batches: Vec<(NotificationType, Vec<String>)> = Vec::new();

        if let Some((notification_type, relevant_dids)) = classification {
            if !relevant_dids.is_empty() {
                notification_batches.push((notification_type, relevant_dids));
            }
        }

        if !subscription_targets.is_empty() {
            subscription_targets.sort();
            subscription_targets.dedup();

            let kind = if is_reply_post {
                ActivitySubscriptionKind::Reply
            } else {
                ActivitySubscriptionKind::Post
            };

            notification_batches.push((
                NotificationType::ActivitySubscription(kind),
                subscription_targets,
            ));
        }

        if notification_batches.is_empty() {
            continue;
        }

        for (notification_type, relevant_dids) in notification_batches {
            if relevant_dids.is_empty() {
                continue;
            }

            let mut dids_to_resolve = Vec::new();
            dids_to_resolve.push(event.author.clone());
            dids_to_resolve.extend(relevant_dids.clone());

            let handle_map = did_resolver.get_handles_bulk(&dids_to_resolve).await;

            let devices_map = match db::get_user_devices_batch(&db_pool, &relevant_dids).await {
                Ok(map) => map,
                Err(e) => {
                    error!("Failed to batch fetch user devices: {}", e);
                    continue;
                }
            };

            let mut notification_futures = Vec::new();

            for did in &relevant_dids {
                if did == &event.author {
                    debug!(recipient = %did, "Skipping self-notification");
                    continue;
                }

                if relationship_manager.is_muted(did, &event.author).await {
                    debug!(
                        recipient = %did,
                        author = %event.author,
                        "Skipping notification - author is muted by recipient"
                    );
                    continue;
                }

                if relationship_manager.is_blocked(did, &event.author).await {
                    debug!(
                        recipient = %did,
                        author = %event.author,
                        "Skipping notification - author is blocked by recipient"
                    );
                    continue;
                }

                if let Some(devices) = devices_map.get(did) {
                    for device in devices {
                        let db_pool = db_pool.clone();
                        let device = device.clone();
                        let notification_type = notification_type.clone();
                        let event = event.clone();
                        let handle_map = handle_map.clone();
                        let post_resolver = post_resolver.clone();
                        let notification_sender = notification_sender.clone();
                        let did = did.clone();

                        notification_futures.push(async move {
                            match db::get_notification_preferences(&db_pool, device.id).await {
                                Ok(prefs) => {
                                    let should_notify = match &notification_type {
                                        NotificationType::Mention => prefs.mentions,
                                        NotificationType::Reply => prefs.replies,
                                        NotificationType::Like => prefs.likes,
                                        NotificationType::Follow => prefs.follows,
                                        NotificationType::Repost => prefs.reposts,
                                        NotificationType::Quote => prefs.quotes,
                                        NotificationType::ViaLike => prefs.via_likes,
                                        NotificationType::ViaRepost => prefs.via_reposts,
                                        NotificationType::ActivitySubscription(_) => {
                                            prefs.activity_subscriptions
                                        }
                                    };

                                    if should_notify {
                                        match create_notification_content(
                                            &handle_map,
                                            &notification_type,
                                            &event,
                                            &post_resolver,
                                        )
                                        .await
                                        {
                                            Ok((title, body, uri)) => {
                                                let mut data = HashMap::new();
                                                data.insert("did".to_string(), did.clone());
                                                data.insert(
                                                    "author".to_string(),
                                                    event.author.clone(),
                                                );
                                                data.insert("cid".to_string(), event.cid.clone());
                                                data.insert(
                                                    "type".to_string(),
                                                    format!("{:?}", notification_type),
                                                );

                                                if let Some(uri_str) = &uri {
                                                    data.insert("uri".to_string(), uri_str.clone());
                                                }

                                                if let NotificationType::ActivitySubscription(kind) = &notification_type {
                                                    data.insert(
                                                        "subscriptionType".to_string(),
                                                        kind.as_reason().to_string(),
                                                    );
                                                }

                                                let payload = NotificationPayload {
                                                    user_did: did.clone(),
                                                    device_token: device.device_token.clone(),
                                                    notification_type: notification_type.clone(),
                                                    title,
                                                    body,
                                                    data,
                                                };

                                                let remaining_capacity = notification_sender.capacity();
                                                if remaining_capacity == 0 {
                                                    warn!(
                                                        "Notification channel at capacity, applying backpressure for {} notification",
                                                        format!("{:?}", notification_type).to_lowercase()
                                                    );

                                                    if !matches!(
                                                        notification_type,
                                                        NotificationType::Follow
                                                            | NotificationType::Reply
                                                            | NotificationType::Mention
                                                            | NotificationType::ActivitySubscription(_)
                                                    ) {
                                                        warn!("Skipping low-priority notification due to system load");
                                                        return;
                                                    }

                                                    tokio::time::sleep(
                                                        tokio::time::Duration::from_millis(100),
                                                    )
                                                    .await;
                                                }

                                                match tokio::time::timeout(
                                                    tokio::time::Duration::from_secs(3),
                                                    notification_sender.send(payload),
                                                )
                                                .await
                                                {
                                                    Ok(Ok(_)) => {
                                                        crate::metrics::NOTIFICATIONS_SENT.inc();
                                                    }
                                                    Ok(Err(e)) => {
                                                        error!("Failed to send notification to queue: {}", e);
                                                    }
                                                    Err(_) => {
                                                        error!(
                                                            "Timeout when sending notification to queue - system overloaded"
                                                        );
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                error!("Failed to create notification content: {}", e);
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    error!("Failed to get notification preferences: {}", e);
                                }
                            }
                        });
                    }
                }
            }

            futures::future::join_all(notification_futures).await;
        }

        // Record event processing time
        let elapsed = timer.elapsed().as_secs_f64();
        crate::metrics::EVENT_PROCESSING_TIME.observe(elapsed);
    }

    info!("Event filter stopped");
    Ok(())
}

// Helper function to check if an embedded record quotes any of the users
fn is_quote_of_users(record_obj: &serde_json::Value, users: &[String]) -> bool {
    if let Some(record_uri) = record_obj
        .get("record")
        .and_then(|r| r.get("uri").and_then(|u| u.as_str()))
    {
        for user in users {
            if record_uri.contains(user) {
                info!(
                    user = %user,
                    "Found quote post referencing user's content"
                );
                return true;
            }
        }
    }

    // Alternative structure
    if let Some(uri) = record_obj.get("uri").and_then(|u| u.as_str()) {
        for user in users {
            if uri.contains(user) {
                info!(
                    user = %user,
                    "Found quote post referencing user's content"
                );
                return true;
            }
        }
    }

    false
}

fn classify_event(
    event: &BlueskyEvent,
    registered_users: &[String],
) -> Option<(NotificationType, Vec<String>)> {
    // Add debug logging to understand record structure for each event type
    debug!(
        path = %event.path,
        "Processing event record structure: {:?}",
        event.record
    );

    // Determine the notification type based on the event path and record
    let (notification_type, relevant_dids) = match event.path.as_str() {
        path if path.contains("app.bsky.feed.post") => {
            // Check for quote posts first (new addition)
            if has_quote_embed(&event.record) {
                let quoted_dids = find_quoted_users(event, registered_users);
                if !quoted_dids.is_empty() {
                    (NotificationType::Quote, quoted_dids)
                } else if event.record.get("reply").is_some() {
                    // Then check if it's a reply
                    let relevant_dids = extract_target_dids(event, registered_users);
                    if !relevant_dids.is_empty() {
                        (NotificationType::Reply, relevant_dids)
                    } else {
                        // Check if it might be a mention
                        let mentioned_dids = extract_mention_dids(event, registered_users);
                        if !mentioned_dids.is_empty() {
                            (NotificationType::Mention, mentioned_dids)
                        } else {
                            return None;
                        }
                    }
                } else {
                    // Regular post - check for mentions in facets
                    let mentioned_dids = extract_mention_dids(event, registered_users);
                    if !mentioned_dids.is_empty() {
                        (NotificationType::Mention, mentioned_dids)
                    } else {
                        return None;
                    }
                }
            } else if event.record.get("reply").is_some() {
                // If not a quote, check if it's a reply
                let relevant_dids = extract_target_dids(event, registered_users);
                if !relevant_dids.is_empty() {
                    (NotificationType::Reply, relevant_dids)
                } else {
                    // Check if it might be a mention
                    let mentioned_dids = extract_mention_dids(event, registered_users);
                    if !mentioned_dids.is_empty() {
                        (NotificationType::Mention, mentioned_dids)
                    } else {
                        return None;
                    }
                }
            } else {
                // Regular post - check for mentions in facets
                let mentioned_dids = extract_mention_dids(event, registered_users);
                if !mentioned_dids.is_empty() {
                    (NotificationType::Mention, mentioned_dids)
                } else {
                    return None;
                }
            }
        }
        path if path.contains("app.bsky.feed.like") => {
            // Check if this is a via like (someone liked a post via someone's repost)
            if let Some(via_dids) = extract_via_dids(event, registered_users) {
                (NotificationType::ViaLike, via_dids)
            } else {
                // Regular like notification
                let relevant_dids = extract_target_dids(event, registered_users);
                (NotificationType::Like, relevant_dids)
            }
        }
        path if path.contains("app.bsky.graph.follow") => {
            // Extract relevant DIDs for follows
            let relevant_dids = extract_target_dids(event, registered_users);
            (NotificationType::Follow, relevant_dids)
        }
        path if path.contains("app.bsky.feed.repost") => {
            // Check if this is a via repost (someone reposted a post via someone's repost)
            if let Some(via_dids) = extract_via_dids(event, registered_users) {
                (NotificationType::ViaRepost, via_dids)
            } else {
                // Regular repost notification
                let relevant_dids = extract_target_dids(event, registered_users);
                (NotificationType::Repost, relevant_dids)
            }
        }
        _ => return None, // Not a notification-worthy event
    };

    if relevant_dids.is_empty() {
        None
    } else {
        // Only log when we found relevant DIDs
        info!(
            notification_type = ?notification_type,
            relevant_dids_count = relevant_dids.len(),
            "Preparing notification"
        );
        Some((notification_type, relevant_dids))
    }
}

// Helper function to check if a post has any quote embeds
fn has_quote_embed(record: &serde_json::Value) -> bool {
    if let Some(embed) = record.get("embed") {
        // Check for direct record embedding
        if embed.get("record").is_some() {
            return true;
        }

        // Check for embed with $type
        if let Some(embed_type) = embed.get("$type").and_then(|t| t.as_str()) {
            return embed_type == "app.bsky.embed.record"
                || embed_type == "app.bsky.embed.recordWithMedia";
        }
    }
    false
}

// Extract DIDs of users whose content is quoted
fn find_quoted_users(event: &BlueskyEvent, registered_users: &[String]) -> Vec<String> {
    let mut quoted_dids = Vec::new();

    if let Some(embed) = event.record.get("embed") {
        // Direct record embedding
        if let Some(record_obj) = embed.get("record") {
            extract_quoted_dids(record_obj, registered_users, &mut quoted_dids);
        }

        // Record with media
        if embed.get("$type").and_then(|t| t.as_str()) == Some("app.bsky.embed.recordWithMedia") {
            if let Some(record_obj) = embed.get("record") {
                extract_quoted_dids(record_obj, registered_users, &mut quoted_dids);
            }
        }
    }

    quoted_dids
}

// Helper to extract DIDs from a quoted record
fn extract_quoted_dids(
    record_obj: &serde_json::Value,
    registered_users: &[String],
    result: &mut Vec<String>,
) {
    // Check standard structure
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

    // Alternative structure
    if let Some(uri) = record_obj.get("uri").and_then(|u| u.as_str()) {
        for user in registered_users {
            if uri.contains(user) && !result.contains(user) {
                result.push(user.to_string());
            }
        }
    }
}

// Separate function to extract mention DIDs from facets
fn extract_mention_dids(event: &BlueskyEvent, registered_users: &[String]) -> Vec<String> {
    let mut mentioned_dids = Vec::new();

    if let Some(facets) = event.record.get("facets").and_then(|f| f.as_array()) {
        for facet in facets {
            if let Some(features) = facet.get("features").and_then(|f| f.as_array()) {
                for feature in features {
                    if let Some(feature_type) = feature.get("$type").and_then(|t| t.as_str()) {
                        if feature_type == "app.bsky.richtext.facet#mention" {
                            if let Some(did) = feature.get("did").and_then(|d| d.as_str()) {
                                if registered_users.contains(&did.to_string())
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
    // Different extraction based on record type
    if event.path.contains("app.bsky.graph.follow") {
        // For follows, the subject is a direct DID string
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
        // For likes and reposts, the subject is an object with a URI
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
        // For posts with reply field, find the parent author
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

async fn create_notification_content(
    handle_map: &HashMap<String, String>,
    notification_type: &NotificationType,
    event: &BlueskyEvent,
    post_resolver: &PostResolver,
) -> Result<(String, String, Option<String>)> {
    // Use resolved handle if available, fallback to DID
    let username = handle_map.get(&event.author).cloned().unwrap_or_else(|| {
        event
            .author
            .split(':')
            .last()
            .unwrap_or(&event.author)
            .to_string()
    });

    // Extract URI and appropriate content based on notification type
    let (title, body, uri) = match notification_type {
        NotificationType::Like => {
            // For likes, we need to fetch the content of the post that was liked
            if let Some(subject) = event.record.get("subject").and_then(|s| s.as_object()) {
                if let Some(uri) = subject.get("uri").and_then(|u| u.as_str()) {
                    // Fetch the original post content that was liked
                    match post_resolver.get_post_content(uri).await {
                        Ok(content) => (
                            format!("@{} liked your post", username),
                            content,
                            Some(uri.to_string()),
                        ),
                        Err(e) => {
                            warn!(error = %e, "Failed to get original post content for like");
                            (
                                format!("@{} liked your post", username),
                                "".to_string(),
                                Some(uri.to_string()),
                            )
                        }
                    }
                } else {
                    (
                        format!("@{} liked your post", username),
                        "".to_string(),
                        None,
                    )
                }
            } else {
                (
                    format!("@{} liked your post", username),
                    "".to_string(),
                    None,
                )
            }
        }
        NotificationType::Repost => {
            // For reposts, we need to fetch the content of the post that was reposted
            if let Some(subject) = event.record.get("subject").and_then(|s| s.as_object()) {
                if let Some(uri) = subject.get("uri").and_then(|u| u.as_str()) {
                    // Fetch the original post content that was reposted
                    match post_resolver.get_post_content(uri).await {
                        Ok(content) => (
                            format!("@{} reposted your post", username),
                            content,
                            Some(uri.to_string()),
                        ),
                        Err(e) => {
                            warn!(error = %e, "Failed to get original post content for repost");
                            (
                                format!("@{} reposted your post", username),
                                "".to_string(),
                                Some(uri.to_string()),
                            )
                        }
                    }
                } else {
                    (
                        format!("@{} reposted your post", username),
                        "".to_string(),
                        None,
                    )
                }
            } else {
                (
                    format!("@{} reposted your post", username),
                    "".to_string(),
                    None,
                )
            }
        }
        NotificationType::Reply => {
            // For replies, use the text of the reply itself
            let post_text = event
                .record
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or("");
            let uri = format!(
                "at://{}/app.bsky.feed.post/{}",
                event.author,
                event.path.split('/').last().unwrap_or("")
            );

            (
                format!("@{} replied to you", username),
                post_text.to_string(),
                Some(uri),
            )
        }
        NotificationType::Mention => {
            // For mentions, use the text of the mentioning post
            let post_text = event
                .record
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or("");
            let uri = format!(
                "at://{}/app.bsky.feed.post/{}",
                event.author,
                event.path.split('/').last().unwrap_or("")
            );

            (
                format!("@{} mentioned you", username),
                post_text.to_string(),
                Some(uri),
            )
        }
        NotificationType::Quote => {
            // For quotes, use the text of the quoting post
            let post_text = event
                .record
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or("");
            let uri = format!(
                "at://{}/app.bsky.feed.post/{}",
                event.author,
                event.path.split('/').last().unwrap_or("")
            );

            (
                format!("@{} quoted your post", username),
                post_text.to_string(),
                Some(uri),
            )
        }
        NotificationType::Follow => {
            // For follows, create a profile URI for the follower
            let profile_uri = format!("at://{}", event.author);

            (
                "New follower".to_string(),
                format!("@{} followed you", username),
                Some(profile_uri), // Now includes URI for deep linking
            )
        }
        NotificationType::ViaLike => {
            // For via likes, get the original post content that was liked
            if let Some(subject) = event.record.get("subject").and_then(|s| s.as_object()) {
                if let Some(uri) = subject.get("uri").and_then(|u| u.as_str()) {
                    match post_resolver.get_post_content(uri).await {
                        Ok(content) => (
                            format!("@{} liked a post via your repost", username),
                            content,
                            Some(uri.to_string()),
                        ),
                        Err(e) => {
                            warn!(error = %e, "Failed to get original post content for via like");
                            (
                                format!("@{} liked a post via your repost", username),
                                "".to_string(),
                                Some(uri.to_string()),
                            )
                        }
                    }
                } else {
                    (
                        format!("@{} liked a post via your repost", username),
                        "".to_string(),
                        None,
                    )
                }
            } else {
                (
                    format!("@{} liked a post via your repost", username),
                    "".to_string(),
                    None,
                )
            }
        }
        NotificationType::ViaRepost => {
            // For via reposts, get the original post content that was reposted
            if let Some(subject) = event.record.get("subject").and_then(|s| s.as_object()) {
                if let Some(uri) = subject.get("uri").and_then(|u| u.as_str()) {
                    match post_resolver.get_post_content(uri).await {
                        Ok(content) => (
                            format!("@{} reposted a post via your repost", username),
                            content,
                            Some(uri.to_string()),
                        ),
                        Err(e) => {
                            warn!(error = %e, "Failed to get original post content for via repost");
                            (
                                format!("@{} reposted a post via your repost", username),
                                "".to_string(),
                                Some(uri.to_string()),
                            )
                        }
                    }
                } else {
                    (
                        format!("@{} reposted a post via your repost", username),
                        "".to_string(),
                        None,
                    )
                }
            } else {
                (
                    format!("@{} reposted a post via your repost", username),
                    "".to_string(),
                    None,
                )
            }
        }
        NotificationType::ActivitySubscription(kind) => {
            let post_text = event
                .record
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or("");

            let uri = format!(
                "at://{}/app.bsky.feed.post/{}",
                event.author,
                event.path.split('/').last().unwrap_or("")
            );

            match kind {
                ActivitySubscriptionKind::Post => (
                    format!("@{} posted a new update", username),
                    post_text.to_string(),
                    Some(uri),
                ),
                ActivitySubscriptionKind::Reply => (
                    format!("@{} replied to a thread", username),
                    post_text.to_string(),
                    Some(uri),
                ),
            }
        }
    };

    tracing::debug!(
        notification_type = ?notification_type,
        username = %username,
        title = %title,
        body = %body,
        uri = ?uri,
        "Created notification content"
    );

    Ok((title, body, uri))
}

// Extract DIDs from via field for via notifications
// The via field points to a repost, and we want to notify the author of that repost
fn extract_via_dids(event: &BlueskyEvent, registered_users: &[String]) -> Option<Vec<String>> {
    // Check if the record has a via field
    if let Some(via) = event.record.get("via") {
        // The via field should be an object with a uri field pointing to a repost
        if let Some(via_uri) = via.get("uri").and_then(|u| u.as_str()) {
            // Extract the DID from the via URI (format: at://did:example/app.bsky.feed.repost/rkey)
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
