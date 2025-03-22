use a2::{Client, DefaultNotificationBuilder, NotificationBuilder, NotificationOptions, Priority};
use anyhow::{Context, Result};
use sqlx::{Pool, Postgres};
use std::{path::Path, time::Duration};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::models::NotificationPayload;

pub struct ApnsClient {
    client: Client,
    topic: String,
}

impl ApnsClient {
    pub fn new(key_path: &str, key_id: &str, team_id: &str, production: bool) -> Result<Self> {
        let key_path = Path::new(key_path);
        let _key = std::fs::read(key_path).context(format!(
            "Failed to read APNs key file: {}",
            key_path.display()
        ))?;

        let _endpoint = if production {
            a2::Endpoint::Production
        } else {
            a2::Endpoint::Sandbox
        };

        // Use the topic from config
        let topic =
            std::env::var("APNS_TOPIC").context("APNS_TOPIC environment variable not set")?;

        let config = a2::ClientConfig::new(if production {
            a2::Endpoint::Production
        } else {
            a2::Endpoint::Sandbox
        });

        let client = a2::Client::token(std::fs::File::open(key_path)?, key_id, team_id, config)?;

        Ok(Self { client, topic })
    }

    pub async fn send_notification(&self, payload_data: &NotificationPayload) -> Result<()> {
        let builder = DefaultNotificationBuilder::new()
            .set_title(&payload_data.title)
            .set_body(&payload_data.body)
            .set_sound("default");

        let mut payload = builder.build(
            &payload_data.device_token,
            NotificationOptions {
                apns_topic: Some(&self.topic),
                apns_priority: Some(Priority::High),
                apns_collapse_id: None,
                apns_expiration: None,
                apns_push_type: None,
                apns_id: None,
            },
        );

        for (key, value) in &payload_data.data {
            payload.add_custom_data(key, value)?;
        }

        debug!(
            device_token = %payload_data.device_token,
            title = %payload_data.title,
            "Attempting to send APNS notification"
        );

        // Log the details once when attempting to send
        info!(
            notification_type = ?payload_data.notification_type,
            user_did = %payload_data.user_did,
            title = %payload_data.title,
            "Sending notification"
        );

        const MAX_RETRIES: u8 = 3;
        let mut retry_count = 0;
        let mut backoff_ms = 100;

        loop {
            match self.client.send(payload.clone()).await {
                Ok(response) => {
                    if response.code >= 200 && response.code < 300 {
                        info!(
                            notification_type = ?payload_data.notification_type,
                            user_did = %payload_data.user_did,
                            status = response.code,
                            "Notification delivered successfully"
                        );
                    } else {
                        // Non-2xx status is still an "Ok" response from the API but might indicate a problem
                        warn!(
                            notification_type = ?payload_data.notification_type,
                            user_did = %payload_data.user_did,
                            status = response.code,
                            "Notification accepted but with non-success status"
                        );
                    }
                    return Ok(());
                }
                Err(e) => {
                    retry_count += 1;
                    warn!(
                        notification_type = ?payload_data.notification_type,
                        user_did = %payload_data.user_did,
                        error = %e,
                        attempt = retry_count,
                        "Failed to send notification, retrying"
                    );

                    if retry_count >= MAX_RETRIES {
                        error!(
                            notification_type = ?payload_data.notification_type,
                            user_did = %payload_data.user_did,
                            error = %e,
                            "Failed to send notification after maximum retries"
                        );
                        return Err(e.into());
                    }

                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    backoff_ms *= 2;
                }
            }
        }
    }
}

pub async fn run_notification_sender(
    mut notification_receiver: mpsc::Receiver<NotificationPayload>,
    apns_client: ApnsClient,
    db_pool: Pool<Postgres>,
) -> Result<()> {
    info!("Starting notification sender");

    // Add a counter to track notification processing
    let mut notification_count = 0;
    let mut success_count = 0;
    let mut error_count = 0;

    while let Some(notification) = notification_receiver.recv().await {
        notification_count += 1;

        match apns_client.send_notification(&notification).await {
            Ok(_) => {
                success_count += 1;
                // Only log notification stats periodically to reduce log spam
                if notification_count % 10 == 0 {
                    info!(
                        "Notification stats: {} processed ({} succeeded, {} failed)",
                        notification_count, success_count, error_count
                    );
                }
                info!(
                    "Successfully sent {} notification to {}",
                    format!("{:?}", notification.notification_type).to_lowercase(),
                    notification.user_did
                );
            }
            Err(e) => {
                error_count += 1;
                error!(
                    notification_type = ?notification.notification_type,
                    user_did = %notification.user_did,
                    "Failed to send notification: {}",
                    e
                );

                if let Some(a2_err) = e.downcast_ref::<a2::Error>() {
                    if let a2::Error::ResponseError(resp) = a2_err {
                        if resp.code == 410 {
                            match sqlx::query!(
                                "DELETE FROM user_devices WHERE device_token = $1",
                                notification.device_token
                            )
                            .execute(&db_pool)
                            .await
                            {
                                Ok(_) => {
                                    info!(
                                        "Removed invalid token for user {}",
                                        notification.user_did
                                    );
                                }
                                Err(e) => {
                                    error!("Failed to remove invalid token: {}", e);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    info!("Notification sender stopped");
    Ok(())
}
