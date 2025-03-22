use serde::{Deserialize, Serialize};
use sqlx::types::{time::OffsetDateTime, uuid::Uuid};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserDevice {
    pub id: Uuid,
    pub did: String,
    pub device_token: String,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationPreference {
    pub user_id: Uuid,
    pub mentions: bool,
    pub replies: bool,
    pub likes: bool,
    pub follows: bool,
    pub reposts: bool,
    pub quotes: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NotificationType {
    Mention,
    Reply,
    Like,
    Follow,
    Repost,
    Quote,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlueskyEvent {
    pub op: String,
    pub path: String,
    pub cid: String,
    pub author: String,
    pub record: serde_json::Value,
    pub timestamp: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationPayload {
    pub user_did: String,
    pub device_token: String,
    pub notification_type: NotificationType,
    pub title: String,
    pub body: String,
    pub data: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FirehoseCursor {
    pub id: i32,
    pub cursor: String,
    pub updated_at: OffsetDateTime,
}
