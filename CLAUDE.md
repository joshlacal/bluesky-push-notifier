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

## Current Deployment Status

### Running Services
You currently have **2** healthy `bluesky-push-notifier` services running:

#### Service 1 - Staging (STG)
- **PID**: 38023 (main process), 38010 (doppler wrapper)
- **Port**: 8080
- **Config**: `stg` (Doppler)
- **Health**: ✅ Healthy
- **Command**: `doppler run --config stg -- /home/ubuntu/bluesky-push-notifier/target/release/bluesky-push-notifier`
- **Domain**: notifications.catbird.blue (production domain)

#### Service 2 - Development (DEV)
- **PID**: 45136 (main process), 45109 (doppler wrapper)  
- **Port**: 8081
- **Config**: `dev` (Doppler)
- **Health**: ✅ Healthy
- **Command**: `doppler run --config dev -- /home/ubuntu/bluesky-push-notifier/target/release/bluesky-push-notifier`
- **Domain**: dev.notifications.catbird.blue

### Nginx Configurations

#### Production: notifications.catbird.blue
- **Config File**: `/etc/nginx/sites-available/notifications.catbird.blue`
- **Proxy Target**: `http://localhost:8080` (STG service)

#### Development: dev.notifications.catbird.blue  
- **Config File**: `/etc/nginx/sites-available/dev.notifications.catbird.blue`
- **Backup Config**: `/etc/nginx/sites-available/dev.notifications.catbird.blue.backup`
- **Proxy Target**: `http://localhost:8081` (DEV service)

### Health Status
- **Port 8080** (STG): ✅ Healthy
- **Port 8081** (DEV): ✅ Healthy

### Doppler Configurations

Available configs in `bluesky-push-notifier` project:
- **dev**: Development environment (actively used)
- **dev_personal**: Personal development environment (unused)
- **stg**: Staging environment (actively used)
- **prd**: Production environment (available but not currently deployed)

### Summary
- ✅ 2 healthy services running on ports 8080 (STG) and 8081 (DEV)
- ✅ Both services have proper nginx reverse proxy configurations
- ✅ Both services are using Doppler for environment configuration
- 📝 Production config exists but not currently deployed