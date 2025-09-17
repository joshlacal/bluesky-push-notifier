use serde::{Deserialize, Serialize};
use sqlx::{
    types::{time::OffsetDateTime, uuid::Uuid},
    FromRow,
};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct UserDevice {
    pub id: Uuid,
    pub did: String,
    pub device_token: String,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub app_attest_key_id: Option<String>,
    pub app_attest_public_key: Option<Vec<u8>>,
    pub app_attest_receipt: Option<Vec<u8>>,
    pub app_attest_counter: i64,
    pub app_attest_challenge: Option<String>,
    pub app_attest_challenge_expires_at: Option<OffsetDateTime>,
    pub app_attest_last_verified_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
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

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct FirehoseCursor {
    pub id: i32,
    pub cursor: String,
    pub updated_at: OffsetDateTime,
}
