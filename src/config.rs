use anyhow::{Context, Result};
use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub bsky_service_url: String,
    pub bsky_api_url: String,
    pub apns_key_path: String,
    pub apns_key_id: String,
    pub apns_team_id: String,
    pub apns_topic: String,
    pub apns_production: bool,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            database_url: env::var("DATABASE_URL").context("DATABASE_URL must be set")?,
            bsky_service_url: env::var("BSKY_SERVICE_URL")
                .unwrap_or_else(|_| "https://bsky.network".to_string()),
            bsky_api_url: env::var("BSKY_API_URL")
                .unwrap_or_else(|_| "https://public.api.bsky.app".to_string()),
            apns_key_path: env::var("APNS_KEY_PATH").context("APNS_KEY_PATH must be set")?,
            apns_key_id: env::var("APNS_KEY_ID").context("APNS_KEY_ID must be set")?,
            apns_team_id: env::var("APNS_TEAM_ID").context("APNS_TEAM_ID must be set")?,
            apns_topic: env::var("APNS_TOPIC").context("APNS_TOPIC must be set")?,
            apns_production: env::var("APNS_PRODUCTION")
                .map(|v| v == "true")
                .unwrap_or(false),
        })
    }
}