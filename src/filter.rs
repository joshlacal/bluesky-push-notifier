use anyhow::Result;
use sqlx::{Pool, Postgres};
use tokio::sync::mpsc;
use tracing::{debug, error, info};
use std::sync::Arc;
use std::collections::HashMap;

use crate::{
    db,
    models::{BlueskyEvent, NotificationPayload, NotificationType},
};

pub async fn run_event_filter(
    mut event_receiver: mpsc::Receiver<BlueskyEvent>,
    notification_sender: mpsc::Sender<NotificationPayload>,
    db_pool: Pool<Postgres>,
    did_resolver: Arc<crate::did_resolver::DidResolver>,
) -> Result<()> {
    info!("Starting event filter");

    // Cache of registered users to avoid frequent DB lookups
    let mut registered_users = db::get_registered_users(&db_pool).await?;
    let mut last_cache_refresh = std::time::Instant::now();

    while let Some(event) = event_receiver.recv().await {
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
            
            for did in relevant_dids {
                // Get user devices
                match db::get_user_devices(&db_pool, &did).await {
                    Ok(devices) => {
                        for device in devices {
                            // Get user notification preferences
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
                                        // Create notification content with handle map
                                        let (title, body) = create_notification_content(
                                            &handle_map,
                                            &notification_type, 
                                            &event
                                        );

                                        // Prepare notification payload
                                        let payload = NotificationPayload {
                                            user_did: did.clone(),
                                            device_token: device.device_token.clone(),
                                            notification_type: notification_type.clone(),
                                            title,
                                            body,
                                            data: Default::default(), // Add relevant data as needed
                                        };

                                        // Send to notification queue
                                        if let Err(e) = notification_sender.send(payload).await {
                                            error!("Failed to send notification to queue: {}", e);
                                        }
                                    }
                                }
                                Err(e) => {
                                    error!("Failed to get notification preferences: {}", e);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to get user devices: {}", e);
                    }
                }
            }
        }
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

fn create_notification_content(
    handle_map: &HashMap<String, String>,
    notification_type: &NotificationType,
    event: &BlueskyEvent,
) -> (String, String) {
    // Use resolved handle if available, fallback to DID
    let username = handle_map.get(&event.author)
        .cloned()
        .unwrap_or_else(|| event.author.split(':').last().unwrap_or(&event.author).to_string());

    let (title, body) = match notification_type {
        NotificationType::Mention => (
            "New mention".to_string(),
            format!("@{} mentioned you in a post", username),
        ),
        NotificationType::Reply => (
            "New reply".to_string(),
            format!("@{} replied to your post", username),
        ),
        NotificationType::Like => (
            "New like".to_string(),
            format!("@{} liked your post", username),
        ),
        NotificationType::Follow => (
            "New follower".to_string(),
            format!("@{} followed you", username),
        ),
        NotificationType::Repost => (
            "New repost".to_string(),
            format!("@{} reposted your post", username),
        ),
        NotificationType::Quote => (
            "New quote".to_string(),
            format!("@{} quoted your post", username),
        ),
    };

    tracing::debug!(
        notification_type = ?notification_type,
        username = %username,
        title = %title,
        body = %body,
        "Created notification content"
    );

    (title, body)
}
