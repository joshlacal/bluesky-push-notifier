use anyhow::Result;
use sqlx::{Pool, Postgres};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use std::sync::Arc;
use std::collections::HashMap;

use crate::{
    db,
    models::{BlueskyEvent, NotificationPayload, NotificationType},
};

use crate::post_resolver::PostResolver;

pub async fn run_event_filter(
    mut event_receiver: mpsc::Receiver<BlueskyEvent>,
    notification_sender: mpsc::Sender<NotificationPayload>,
    db_pool: Pool<Postgres>,
    did_resolver: Arc<crate::did_resolver::DidResolver>,
    post_resolver: Arc<crate::post_resolver::PostResolver>,
    relationship_manager: Arc<crate::relationship_manager::RelationshipManager>,
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

        // Skip event if author is not registered
        if !registered_users.contains(&event.author) {
            // Check if the event is relevant to any registered user
            if !is_event_relevant_to_users(&event, &registered_users) {
                continue;
            }
        }

        // Determine notification type and extract relevant user DIDs
        if let Some((notification_type, relevant_dids)) = classify_event(&event, &registered_users)
        {
            // Get all DIDs we need to resolve: author + all relevant recipients
            let mut dids_to_resolve = Vec::new();
            dids_to_resolve.push(event.author.clone());
            dids_to_resolve.extend(relevant_dids.clone());
            
            // Resolve all handles at once
            let handle_map = did_resolver.get_handles_bulk(&dids_to_resolve).await;
            
            // Fetch devices for all relevant DIDs in one batch operation
            let devices_map = match db::get_user_devices_batch(&db_pool, &relevant_dids).await {
                Ok(map) => map,
                Err(e) => {
                    error!("Failed to batch fetch user devices: {}", e);
                    continue; // Skip to next event
                }
            };

            // Process each relevant DID
            let mut notification_futures = Vec::new();
            for did in &relevant_dids {
                // Check if the target has muted or blocked the author
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
                    // Process devices for this DID
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
                            // Get user preferences
                            match db::get_notification_preferences(&db_pool, device.id).await {
                                Ok(prefs) => {
                                    // Check if user wants this notification type
                                    let should_notify = match &notification_type {
                                        NotificationType::Mention => prefs.mentions,
                                        NotificationType::Reply => prefs.replies,
                                        NotificationType::Like => prefs.likes,
                                        NotificationType::Follow => prefs.follows,
                                        NotificationType::Repost => prefs.reposts,
                                        NotificationType::Quote => prefs.quotes,
                                    };

                                    if should_notify {
                                        // Create notification content with handle map and post resolver
                                        match create_notification_content(
                                            &handle_map,
                                            &notification_type, 
                                            &event,
                                            &post_resolver
                                        ).await {
                                            Ok((title, body, uri)) => {
                                                // Prepare notification payload with additional data
                                                let mut data = HashMap::new();
                                                
                                                // Add URI to data for deep linking
                                                if let Some(uri_str) = &uri {
                                                    data.insert("uri".to_string(), uri_str.clone());
                                                    data.insert("type".to_string(), format!("{:?}", notification_type));
                                                }

                                                let payload = NotificationPayload {
                                                    user_did: did.clone(),
                                                    device_token: device.device_token.clone(),
                                                    notification_type: notification_type.clone(),
                                                    title,
                                                    body,
                                                    data, // Now contains URI and type for deep linking
                                                };

                                                // Add backpressure detection
                                                let remaining_capacity = notification_sender.capacity();
                                                if remaining_capacity == 0 {
                                                    warn!(
                                                        "Notification channel at capacity, applying backpressure for {} notification",
                                                        format!("{:?}", notification_type).to_lowercase()
                                                    );
                                                    
                                                    // Prioritize important notifications
                                                    if !matches!(notification_type, NotificationType::Follow | NotificationType::Reply | NotificationType::Mention) {
                                                        warn!("Skipping low-priority notification due to system load");
                                                        return;
                                                    }
                                                    
                                                    // Brief delay to allow system to catch up
                                                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                                                }

                                                // Send with timeout to avoid blocking indefinitely
                                                match tokio::time::timeout(
                                                    tokio::time::Duration::from_secs(3),
                                                    notification_sender.send(payload)
                                                ).await {
                                                    Ok(Ok(_)) => {
                                                        crate::metrics::NOTIFICATIONS_SENT.inc();
                                                    },
                                                    Ok(Err(e)) => {
                                                        error!("Failed to send notification to queue: {}", e);
                                                    },
                                                    Err(_) => {
                                                        error!("Timeout when sending notification to queue - system overloaded");
                                                    }
                                                }
                                            },
                                            Err(e) => {
                                                error!("Failed to create notification content: {}", e);
                                            }
                                        }
                                    }
                                },
                                Err(e) => {
                                    error!("Failed to get notification preferences: {}", e);
                                }
                            }
                        });
                    }
                }
            }
            
            // Execute all notification processing in parallel
            futures::future::join_all(notification_futures).await;
        }
        
        // Record event processing time
        let elapsed = timer.elapsed().as_secs_f64();
        crate::metrics::EVENT_PROCESSING_TIME.observe(elapsed);
    }

    info!("Event filter stopped");
    Ok(())
}

fn is_event_relevant_to_users(event: &BlueskyEvent, users: &[String]) -> bool {
    // Only debug log for specific types
    let event_type = if event.path.contains("app.bsky.feed.post") {
        "post"
    } else if event.path.contains("app.bsky.feed.like") {
        "like"
    } else if event.path.contains("app.bsky.feed.repost") {
        "repost"
    } else if event.path.contains("app.bsky.graph.follow") {
        "follow"
    } else {
        "other"
    };

    // Handle follows differently - subject is a direct DID string
    if event.path.contains("app.bsky.graph.follow") {
        if let Some(subject) = event.record.get("subject").and_then(|s| s.as_str()) {
            for user in users {
                if subject == user {
                    info!(
                        type = %event_type,
                        user = %user,
                        "Found relevant follow for user"
                    );
                    return true;
                }
            }
        }
        return false;
    }

    // Handle likes and reposts - subject is an object with a URI
    if event.path.contains("app.bsky.feed.like") || event.path.contains("app.bsky.feed.repost") {
        if let Some(subject) = event.record.get("subject").and_then(|s| s.as_object()) {
            if let Some(uri) = subject.get("uri").and_then(|u| u.as_str()) {
                for user in users {
                    if uri.contains(user) {
                        info!(
                            type = %event_type,
                            user = %user,
                            "Found relevant {} for user in URI",
                            event_type
                        );
                        return true;
                    }
                }
            }
        }
    }

    // For posts, check: 1) facets for mentions, 2) reply chains, 3) quote posts
    if event.path.contains("app.bsky.feed.post") {
        // 1. Check facets for mentions (proper method)
        if let Some(facets) = event.record.get("facets").and_then(|f| f.as_array()) {
            for facet in facets {
                if let Some(features) = facet.get("features").and_then(|f| f.as_array()) {
                    for feature in features {
                        // Look for mention features
                        if let Some(feature_type) = feature.get("$type").and_then(|t| t.as_str()) {
                            if feature_type == "app.bsky.richtext.facet#mention" {
                                if let Some(did) = feature.get("did").and_then(|d| d.as_str()) {
                                    for user in users {
                                        if did == user {
                                            info!(
                                                user = %user,
                                                "Found mention of user in post facets"
                                            );
                                            return true;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // 2. Check for replies to registered users
        if let Some(reply) = event.record.get("reply").and_then(|r| r.as_object()) {
            if let Some(parent) = reply.get("parent").and_then(|p| p.as_object()) {
                if let Some(uri) = parent.get("uri").and_then(|u| u.as_str()) {
                    for user in users {
                        if uri.contains(user) {
                            info!(
                                user = %user,
                                "Found reply to user's post"
                            );
                            return true;
                        }
                    }
                }
            }
        }

        // 3. NEW: Check for quote posts (app.bsky.embed.record)
        if let Some(embed) = event.record.get("embed") {
            // Direct record embedding
            if let Some(embed_obj) = embed.get("record") {
                if is_quote_of_users(embed_obj, users) {
                    return true;
                }
            }
            
            // Check for record with media
            if let Some(_media_obj) = embed.get("media") {
                // For recordWithMedia, the record is in a separate field
                if let Some(record_obj) = embed.get("record") {
                    if is_quote_of_users(record_obj, users) {
                        return true;
                    }
                }
            }
            
            // Check for $type-based embeds (alternative structure)
            if let Some(embed_type) = embed.get("$type").and_then(|t| t.as_str()) {
                match embed_type {
                    "app.bsky.embed.record" => {
                        if let Some(record) = embed.get("record") {
                            if is_quote_of_users(record, users) {
                                return true;
                            }
                        }
                    }
                    "app.bsky.embed.recordWithMedia" => {
                        if let Some(record) = embed.get("record") {
                            if is_quote_of_users(record, users) {
                                return true;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        // 4. Fallback: Check text for @mentions (less accurate but catches some edge cases)
        if let Some(text) = event.record.get("text").and_then(|t| t.as_str()) {
            for user in users {
                // Extract handle from DID
                let handle = user.split('.').next().unwrap_or(user);
                if text.contains(&format!("@{}", handle)) {
                    debug!(
                        user = %user,
                        handle = %handle,
                        "Found potential mention of user in post text (fallback detection)"
                    );
                    return true;
                }
            }
        }
    }

    false
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
            // Extract relevant DIDs for likes
            let relevant_dids = extract_target_dids(event, registered_users);
            (NotificationType::Like, relevant_dids)
        }
        path if path.contains("app.bsky.graph.follow") => {
            // Extract relevant DIDs for follows
            let relevant_dids = extract_target_dids(event, registered_users);
            (NotificationType::Follow, relevant_dids)
        }
        path if path.contains("app.bsky.feed.repost") => {
            // Extract relevant DIDs for reposts
            let relevant_dids = extract_target_dids(event, registered_users);
            (NotificationType::Repost, relevant_dids)
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
            return embed_type == "app.bsky.embed.record" || 
                   embed_type == "app.bsky.embed.recordWithMedia";
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
fn extract_quoted_dids(record_obj: &serde_json::Value, registered_users: &[String], result: &mut Vec<String>) {
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
                                if registered_users.contains(&did.to_string()) && 
                                   !mentioned_dids.contains(&did.to_string()) {
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
    let username = handle_map.get(&event.author)
        .cloned()
        .unwrap_or_else(|| event.author.split(':').last().unwrap_or(&event.author).to_string());
    
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
                            Some(uri.to_string())
                        ),
                        Err(e) => {
                            warn!(error = %e, "Failed to get original post content for like");
                            (
                                format!("@{} liked your post", username),
                                "".to_string(),
                                Some(uri.to_string())
                            )
                        }
                    }
                } else {
                    (
                        format!("@{} liked your post", username),
                        "".to_string(),
                        None
                    )
                }
            } else {
                (
                    format!("@{} liked your post", username),
                    "".to_string(),
                    None
                )
            }
        },
        NotificationType::Repost => {
            // For reposts, we need to fetch the content of the post that was reposted
            if let Some(subject) = event.record.get("subject").and_then(|s| s.as_object()) {
                if let Some(uri) = subject.get("uri").and_then(|u| u.as_str()) {
                    // Fetch the original post content that was reposted
                    match post_resolver.get_post_content(uri).await {
                        Ok(content) => (
                            format!("@{} reposted your post", username),
                            content,
                            Some(uri.to_string())
                        ),
                        Err(e) => {
                            warn!(error = %e, "Failed to get original post content for repost");
                            (
                                format!("@{} reposted your post", username),
                                "".to_string(),
                                Some(uri.to_string())
                            )
                        }
                    }
                } else {
                    (
                        format!("@{} reposted your post", username),
                        "".to_string(),
                        None
                    )
                }
            } else {
                (
                    format!("@{} reposted your post", username),
                    "".to_string(),
                    None
                )
            }
        },
        NotificationType::Reply => {
            // For replies, use the text of the reply itself
            let post_text = event.record.get("text").and_then(|t| t.as_str()).unwrap_or("");
            let uri = format!("at://{}/app.bsky.feed.post/{}", 
                event.author, 
                event.path.split('/').last().unwrap_or(""));
                
            (
                format!("@{} replied to you", username),
                post_text.to_string(),
                Some(uri)
            )
        },
        NotificationType::Mention => {
            // For mentions, use the text of the mentioning post
            let post_text = event.record.get("text").and_then(|t| t.as_str()).unwrap_or("");
            let uri = format!("at://{}/app.bsky.feed.post/{}", 
                event.author, 
                event.path.split('/').last().unwrap_or(""));
                
            (
                format!("@{} mentioned you", username),
                post_text.to_string(),
                Some(uri)
            )
        },
        NotificationType::Quote => {
            // For quotes, use the text of the quoting post
            let post_text = event.record.get("text").and_then(|t| t.as_str()).unwrap_or("");
            let uri = format!("at://{}/app.bsky.feed.post/{}", 
                event.author, 
                event.path.split('/').last().unwrap_or(""));
                
            (
                format!("@{} quoted your post", username),
                post_text.to_string(),
                Some(uri)
            )
        },
        NotificationType::Follow => {
            // For follows, create a profile URI for the follower
            let profile_uri = format!("at://{}", event.author);
            
            (
                "New follower".to_string(),
                format!("@{} followed you", username),
                Some(profile_uri)  // Now includes URI for deep linking
            )
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