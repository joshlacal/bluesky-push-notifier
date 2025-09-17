use std::time::Duration;

use anyhow::Result;
use moka::future::Cache;
use sqlx::{Pool, Postgres};
use tracing::{debug, error};

use crate::{db, models::ActivitySubscription};

pub struct ActivitySubscriptionManager {
    db_pool: Pool<Postgres>,
    by_subscriber: Cache<String, Vec<ActivitySubscription>>,
    by_subject: Cache<String, Vec<ActivitySubscription>>,
}

impl ActivitySubscriptionManager {
    pub fn new(db_pool: Pool<Postgres>) -> Self {
        let by_subscriber = Cache::builder()
            .max_capacity(50_000)
            .time_to_live(Duration::from_secs(300))
            .build();

        let by_subject = Cache::builder()
            .max_capacity(50_000)
            .time_to_live(Duration::from_secs(300))
            .build();

        Self {
            db_pool,
            by_subscriber,
            by_subject,
        }
    }

    pub async fn list_for_subscriber(
        &self,
        subscriber_did: &str,
    ) -> Result<Vec<ActivitySubscription>> {
        if let Some(cached) = self.by_subscriber.get(subscriber_did) {
            return Ok(cached);
        }

        let records =
            db::list_activity_subscriptions_for_subscriber(&self.db_pool, subscriber_did).await?;

        self.by_subscriber
            .insert(subscriber_did.to_string(), records.clone())
            .await;

        Ok(records)
    }

    pub async fn list_subscribers_for_subject(
        &self,
        subject_did: &str,
    ) -> Result<Vec<ActivitySubscription>> {
        if let Some(cached) = self.by_subject.get(subject_did) {
            return Ok(cached);
        }

        let records = db::list_activity_subscribers_for_subject(&self.db_pool, subject_did).await?;

        self.by_subject
            .insert(subject_did.to_string(), records.clone())
            .await;

        Ok(records)
    }

    pub async fn upsert_subscription(
        &self,
        subscriber_did: &str,
        subject_did: &str,
        include_posts: bool,
        include_replies: bool,
    ) -> Result<()> {
        debug!(
            subscriber = subscriber_did,
            subject = subject_did,
            include_posts,
            include_replies,
            "Upserting activity subscription",
        );

        db::upsert_activity_subscription(
            &self.db_pool,
            subscriber_did,
            subject_did,
            include_posts,
            include_replies,
        )
        .await?;

        self.invalidate(subscriber_did, subject_did).await;

        Ok(())
    }

    pub async fn delete_subscription(&self, subscriber_did: &str, subject_did: &str) -> Result<()> {
        debug!(
            subscriber = subscriber_did,
            subject = subject_did,
            "Deleting activity subscription",
        );

        if let Err(err) =
            db::delete_activity_subscription(&self.db_pool, subscriber_did, subject_did).await
        {
            error!(
                "Failed to delete activity subscription for {} -> {}: {}",
                subscriber_did, subject_did, err
            );
            return Err(err);
        }

        self.invalidate(subscriber_did, subject_did).await;

        Ok(())
    }

    pub async fn invalidate(&self, subscriber_did: &str, subject_did: &str) {
        self.by_subscriber.invalidate(subscriber_did).await;
        self.by_subject.invalidate(subject_did).await;
    }
}
