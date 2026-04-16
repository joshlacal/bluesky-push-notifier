# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Development Commands

- `cargo build` - Build the project
- `cargo run` - Run the application (requires environment configuration)
- `cargo test` - Run tests
- `cargo check` - Check code for errors without building
- `cargo clippy` - Run linter for code quality checks
- `cargo fmt` - Format code according to Rust standards

## Database Management

- Migrations are located in `migrations/` directory with SQLx format
- The application uses PostgreSQL with SQLx for database operations
- Run `sqlx migrate run` to apply migrations (requires DATABASE_URL)
- The `.sqlx/` directory contains compiled query metadata for offline builds

## Architecture Overview

This is a Bluesky push notification service built in Rust with the following core components:

### Core Pipeline
- **Firehose Consumer** (`firehose.rs`): Connects to Bluesky's WebSocket firehose to receive real-time events
- **Event Filter** (`filter.rs`): Processes events and determines which users should receive notifications
- **APNS Sender** (`apns.rs`): Sends push notifications to iOS devices via Apple Push Notification Service

### Key Modules
- **API Server** (`api.rs`): HTTP REST API for device registration and user preferences using Axum
- **Database Layer** (`db.rs`): PostgreSQL connection pool and database operations using SQLx
- **Models** (`models.rs`): Core data structures (UserDevice, NotificationPreference, etc.)
- **Configuration** (`config.rs`): Environment-based configuration management
- **Relationship Manager** (`relationship_manager.rs`): Manages encrypted user relationships with caching
- **DID Resolver** (`did_resolver.rs`): Resolves Bluesky DIDs with caching
- **Post Resolver** (`post_resolver.rs`): Resolves post content and metadata

### Data Flow
1. Firehose events are received and parsed in `firehose.rs`
2. Events flow through a filtered pipeline in `filter.rs` 
3. The filter checks user preferences, relationships, and generates notifications
4. Notifications are sent via APNS in `apns.rs`
5. The API server handles device registration and preference management

### External Dependencies
- **Bluesky SDK**: Uses `bsky-sdk` and `atrium-*` crates for AT Protocol integration
- **APNS**: Uses `a2` crate for Apple Push Notification Service
- **Database**: SQLx with PostgreSQL and pgcrypto extension for encrypted relationships
- **WebSockets**: `tokio-tungstenite` for firehose connection
- **HTTP**: Axum for REST API with CORS and rate limiting

### Environment Configuration
The application requires these environment variables:
- `DATABASE_URL`: PostgreSQL connection string
- `APNS_KEY_PATH`, `APNS_KEY_ID`, `APNS_TEAM_ID`, `APNS_TOPIC`: Apple Push Notification credentials
- `APNS_PRODUCTION`: Boolean for production vs sandbox APNS
- `BSKY_SERVICE_URL`: Bluesky service URL (defaults to https://bsky.network)
- `API_BIND_ADDRESS`: API server bind address (defaults to 0.0.0.0:8080)
- Optional: `TOKIO_WORKER_THREADS` for runtime thread configuration

### Testing and Debugging
- Debug script available at `tools/debug-firehose.sh` for WebSocket connection testing
- Uses `tracing` for structured logging with configurable levels
- Includes Prometheus metrics collection in `metrics.rs`

## Deployment

Two systemd services: `bluesky-push-notifier-stg` (port 8080) and `bluesky-push-notifier-dev` (port 8081).

```bash
cargo build --release

# Deploy DEV first, then STG if stable
sudo systemctl restart bluesky-push-notifier-dev
sudo journalctl -u bluesky-push-notifier-dev -f

sudo systemctl restart bluesky-push-notifier-stg
sudo journalctl -u bluesky-push-notifier-stg -f
```

Secrets managed via Doppler (configs: `dev`, `stg`, `prd`). Never hardcode credentials.

**Note:** This is the predecessor to `catbird-firehose`, which refactored the monolith into a modular workspace.