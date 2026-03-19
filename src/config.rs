use anyhow::{Context, Result};
use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub bsky_service_url: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            database_url: env::var("DATABASE_URL").context("DATABASE_URL must be set")?,
            bsky_service_url: env::var("BSKY_SERVICE_URL")
                .unwrap_or_else(|_| "https://bsky.network".to_string()),
        })
    }
}
