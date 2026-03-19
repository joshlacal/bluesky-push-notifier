use serde::{Deserialize, Serialize};
use sqlx::{types::time::OffsetDateTime, FromRow};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NotificationType {
    Mention,
    Reply,
    Like,
    Follow,
    Repost,
    Quote,
    ViaLike,
    ViaRepost,
    ActivitySubscription(ActivitySubscriptionKind),
}

impl NotificationType {
    /// Convert to the string format expected by nest's push_event_queue
    pub fn as_queue_str(&self) -> &'static str {
        match self {
            NotificationType::Mention => "mention",
            NotificationType::Reply => "reply",
            NotificationType::Like => "like",
            NotificationType::Follow => "follow",
            NotificationType::Repost => "repost",
            NotificationType::Quote => "quote",
            NotificationType::ViaLike => "via_like",
            NotificationType::ViaRepost => "via_repost",
            NotificationType::ActivitySubscription(ActivitySubscriptionKind::Post) => {
                "activity_post"
            }
            NotificationType::ActivitySubscription(ActivitySubscriptionKind::Reply) => {
                "activity_reply"
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ActivitySubscriptionKind {
    Post,
    Reply,
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

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct FirehoseCursor {
    pub id: i32,
    pub cursor: String,
    pub updated_at: OffsetDateTime,
}
