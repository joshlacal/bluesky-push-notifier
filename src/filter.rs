use anyhow::Result;
use sqlx::{Pool, Postgres};
use std::collections::{HashMap, HashSet};
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
    moderation_list_manager: Arc<crate::moderation_list_manager::ModerationListManager>,
    thread_mute_manager: Arc<crate::thread_mute_manager::ThreadMuteManager>,
) -> Result<()> {
    info!("Starting event filter");

    // Cache of registered users to avoid frequent DB lookups
    // IMPORTANT: Handle initial load failure gracefully - don't crash the entire task
    let mut registered_users_vec = match db::get_registered_users(&db_pool).await {
        Ok(users) => users,
        Err(e) => {
            error!("Failed to load initial registered users, starting with empty cache: {}", e);
            Vec::new()
        }
    };
    let mut registered_users: HashSet<String> = registered_users_vec.iter().cloned().collect();
    let mut last_cache_refresh = std::time::Instant::now();

    while let Some(event) = event_receiver.recv().await {
        // Create timer to measure event processing time
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

        // Early exit if no registered users to notify
        if registered_users.is_empty() {
            continue;
        }

        // Quick check - only process notification-relevant events
        if !is_notification_relevant_event(&event.path) {
            continue;
        }

        let classification = classify_event(&event, &registered_users_vec, &registered_users);

        let is_post_event = event.path.contains("app.bsky.feed.post");
        let is_reply_post = is_post_event && event.record.get("reply").is_some();

        let mut subscription_targets = Vec::new();
        if is_post_event && !registered_users.is_empty() {
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
                subscription_targets.clone(), // Clone here to avoid move
            ));
        }

        if notification_batches.is_empty() && subscription_targets.is_empty() {
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

            // Collect all device IDs for batched preference lookup
            let all_device_ids: Vec<uuid::Uuid> = devices_map
                .values()
                .flatten()
                .map(|device| device.id)
                .collect();

            let preferences_map =
                match db::get_notification_preferences_batch(&db_pool, &all_device_ids).await {
                    Ok(map) => map,
                    Err(e) => {
                        error!("Failed to batch fetch notification preferences: {}", e);
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

                // Check moderation lists
                if moderation_list_manager.is_in_block_list(did, &event.author).await {
                    debug!(
                        recipient = %did,
                        author = %event.author,
                        "Skipping notification - author is in recipient's block list"
                    );
                    continue;
                }

                if moderation_list_manager.is_in_mute_list(did, &event.author).await {
                    debug!(
                        recipient = %did,
                        author = %event.author,
                        "Skipping notification - author is in recipient's mute list"
                    );
                    continue;
                }

                // Check thread mutes (if this is a reply)
                if let Some(reply) = event.record.get("reply") {
                    if let Some(root) = reply.get("root").and_then(|r| r.get("uri")).and_then(|u| u.as_str()) {
                        if thread_mute_manager.is_thread_muted(did, root).await {
                            debug!(
                                recipient = %did,
                                thread_root = %root,
                                "Skipping notification - thread is muted by recipient"
                            );
                            continue;
                        }
                    }
                }

                if let Some(devices) = devices_map.get(did) {
                    for device in devices {
                        let device = device.clone();
                        let notification_type = notification_type.clone();
                        let event = event.clone();
                        let handle_map = handle_map.clone();
                        let post_resolver = post_resolver.clone();
                        let notification_sender = notification_sender.clone();
                        let did = did.clone();
                        let preferences_map = preferences_map.clone();

                        notification_futures.push(async move {
                            match preferences_map.get(&device.id) {
                                Some(prefs) => {
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
                                            Ok((title, body, uri, media_urls, thumbnail_url)) => {
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

                                                // Add media information to custom data if available
                                                if let Some(ref urls) = media_urls {
                                                    // Add media URLs as a JSON array string
                                                    if let Ok(media_json) = serde_json::to_string(urls) {
                                                        data.insert("mediaUrls".to_string(), media_json);
                                                    }
                                                }

                                                if let Some(ref thumb_url) = thumbnail_url {
                                                    data.insert("thumbnailUrl".to_string(), thumb_url.clone());
                                                }

                                                let payload = NotificationPayload {
                                                    user_did: did.clone(),
                                                    device_token: device.device_token.clone(),
                                                    notification_type: notification_type.clone(),
                                                    title,
                                                    body,
                                                    data,
                                                    media_urls,
                                                    thumbnail_url,
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
                                None => {
                                    error!("Notification preferences not found for device: {}", device.id);
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

// Quick check for notification-relevant events to avoid processing irrelevant ones
fn is_notification_relevant_event(path: &str) -> bool {
    path.contains("app.bsky.feed.post")
        || path.contains("app.bsky.feed.like")
        || path.contains("app.bsky.graph.follow")
        || path.contains("app.bsky.feed.repost")
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
    registered_users_set: &HashSet<String>,
) -> Option<(NotificationType, Vec<String>)> {
    // Early exit if no registered users to notify
    if registered_users.is_empty() {
        return None;
    }

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
                        let mentioned_dids =
                            extract_mention_dids(event, registered_users, registered_users_set);
                        if !mentioned_dids.is_empty() {
                            (NotificationType::Mention, mentioned_dids)
                        } else {
                            return None;
                        }
                    }
                } else {
                    // Regular post - check for mentions in facets
                    let mentioned_dids =
                        extract_mention_dids(event, registered_users, registered_users_set);
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
                    let mentioned_dids =
                        extract_mention_dids(event, registered_users, registered_users_set);
                    if !mentioned_dids.is_empty() {
                        (NotificationType::Mention, mentioned_dids)
                    } else {
                        return None;
                    }
                }
            } else {
                // Regular post - check for mentions in facets
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
fn extract_mention_dids(
    event: &BlueskyEvent,
    registered_users: &[String],
    registered_users_set: &HashSet<String>,
) -> Vec<String> {
    let mut mentioned_dids = Vec::new();

    // Early exit if no registered users or no facets
    if registered_users.is_empty() {
        return mentioned_dids;
    }

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

/// Extract media URLs from an embed object in a Bluesky post
/// Converts blob CID references to full CDN URLs for display
fn extract_media_from_embed(embed: &serde_json::Value, author_did: &str) -> (Option<Vec<String>>, Option<String>) {
    let mut media_urls = Vec::new();
    let mut thumbnail_url = None;

    // Get the embed type
    let embed_type = embed
        .get("$type")
        .and_then(|t| t.as_str())
        .unwrap_or("");

    match embed_type {
        // Handle images embed
        "app.bsky.embed.images" => {
            if let Some(images) = embed.get("images").and_then(|i| i.as_array()) {
                for image in images {
                    if let Some(img_obj) = image.as_object() {
                        // For images in the record, we have a blob reference
                        if let Some(image_blob) = img_obj.get("image") {
                            if let Some(ref_link) = image_blob.get("ref").and_then(|r| r.get("$link")).and_then(|l| l.as_str()) {
                                // Convert CID to CDN URL
                                let cdn_url = format!(
                                    "https://cdn.bsky.app/img/feed_fullsize/plain/{}/{}@jpeg",
                                    author_did, ref_link
                                );
                                media_urls.push(cdn_url);
                            }
                        }
                    }
                }
                // Use the first image as thumbnail
                if !media_urls.is_empty() {
                    // For thumbnails, use the thumbnail endpoint
                    if let Some(first_img) = embed.get("images")
                        .and_then(|i| i.as_array())
                        .and_then(|arr| arr.first())
                        .and_then(|img| img.get("image"))
                        .and_then(|blob| blob.get("ref"))
                        .and_then(|r| r.get("$link"))
                        .and_then(|l| l.as_str())
                    {
                        thumbnail_url = Some(format!(
                            "https://cdn.bsky.app/img/feed_thumbnail/plain/{}/{}@jpeg",
                            author_did, first_img
                        ));
                    }
                }
            }
        }

        // Handle video embed
        "app.bsky.embed.video" => {
            if let Some(video_blob) = embed.get("video") {
                if let Some(ref_link) = video_blob.get("ref").and_then(|r| r.get("$link")).and_then(|l| l.as_str()) {
                    // For video, we'll use the blob reference as-is since video URLs are different
                    // The client will need to handle video blob access
                    media_urls.push(ref_link.to_string());
                }
            }
            // Get thumbnail if available
            if let Some(thumb_blob) = embed.get("thumbnail") {
                if let Some(ref_link) = thumb_blob.get("ref").and_then(|r| r.get("$link")).and_then(|l| l.as_str()) {
                    // Convert thumbnail CID to CDN URL
                    thumbnail_url = Some(format!(
                        "https://cdn.bsky.app/img/feed_thumbnail/plain/{}/{}@jpeg",
                        author_did, ref_link
                    ));
                }
            }
        }

        // Handle external link embed (website cards)
        "app.bsky.embed.external" => {
            if let Some(external) = embed.get("external") {
                // Get the thumbnail image for the external link
                if let Some(thumb_blob) = external.get("thumb") {
                    if let Some(ref_link) = thumb_blob.get("ref").and_then(|r| r.get("$link")).and_then(|l| l.as_str()) {
                        let cdn_url = format!(
                            "https://cdn.bsky.app/img/feed_thumbnail/plain/{}/{}@jpeg",
                            author_did, ref_link
                        );
                        thumbnail_url = Some(cdn_url.clone());
                        media_urls.push(cdn_url);
                    }
                }
            }
        }

        // Handle record with media (quote post with images/video)
        "app.bsky.embed.recordWithMedia" => {
            // Extract media from the media field
            if let Some(media) = embed.get("media") {
                let (urls, thumb) = extract_media_from_embed(media, author_did);
                if let Some(urls) = urls {
                    media_urls.extend(urls);
                }
                if thumbnail_url.is_none() {
                    thumbnail_url = thumb;
                }
            }
        }

        _ => {
            // For other types or unknown embeds, try to extract any image references
            debug!(embed_type, "Unknown embed type encountered");
        }
    }

    let result_urls = if media_urls.is_empty() {
        None
    } else {
        Some(media_urls)
    };

    (result_urls, thumbnail_url)
}

async fn create_notification_content(
    handle_map: &HashMap<String, String>,
    notification_type: &NotificationType,
    event: &BlueskyEvent,
    post_resolver: &PostResolver,
) -> Result<(String, String, Option<String>, Option<Vec<String>>, Option<String>)> {
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

    // Extract media from the event's embed field
    let (media_urls, thumbnail_url) = if let Some(embed) = event.record.get("embed") {
        extract_media_from_embed(embed, &event.author)
    } else {
        (None, None)
    };

    tracing::debug!(
        notification_type = ?notification_type,
        username = %username,
        title = %title,
        body = %body,
        uri = ?uri,
        media_count = ?media_urls.as_ref().map(|m| m.len()),
        has_thumbnail = thumbnail_url.is_some(),
        "Created notification content"
    );

    Ok((title, body, uri, media_urls, thumbnail_url))
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
