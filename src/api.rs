use axum::{
    error_handling::HandleErrorLayer, // Add HandleErrorLayer
    extract::{Json, Query, State},
    http::{header, StatusCode},
    response::IntoResponse,
    routing::{get, post, put},
    BoxError, // Add BoxError for error handler
    Router,
};
use serde::{Deserialize, Serialize};
use sqlx::{Pool, Postgres};
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::CorsLayer;
// Remove unused import: tower_http::limit::RequestBodyLimitLayer
use tower::timeout::TimeoutLayer;
use tower::ServiceBuilder;
use tracing::{error, info, warn};

use crate::models::{NotificationPreference, UserDevice};
use crate::relationship_manager::RelationshipManager;

// Request and response models
#[derive(Deserialize)]
struct RegisterRequest {
    did: String,
    device_token: String,
}

#[derive(Deserialize)]
struct PreferencesQuery {
    did: String,
}

#[derive(Deserialize, Serialize)]
struct PreferencesRequest {
    did: String,
    mentions: bool,
    replies: bool,
    likes: bool,
    follows: bool,
    reposts: bool,
    quotes: bool,
}

// New model for relationship updates with authentication
#[derive(Deserialize)]
struct RelationshipsRequest {
    did: String,
    device_token: String, // Required for authentication
    mutes: Vec<String>,
    blocks: Vec<String>,
}

// API state
pub struct ApiState {
    pub db_pool: Pool<Postgres>,
    pub relationship_manager: Arc<RelationshipManager>,
}

// Add error handler function for timeouts
async fn handle_timeout_error(error: BoxError) -> (StatusCode, String) {
    if error.is::<tower::timeout::error::Elapsed>() {
        (
            StatusCode::REQUEST_TIMEOUT,
            "Request took too long".to_string(),
        )
    } else {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Unhandled internal error: {}", error),
        )
    }
}

pub fn create_api_router(state: Arc<ApiState>) -> Router {
    Router::new()
        .route("/register", post(register_device))
        .route("/preferences", get(get_preferences))
        .route("/preferences", put(update_preferences))
        .route("/health", get(health_check))
        .route("/metrics", get(metrics_endpoint))
        .route("/relationships", put(update_relationships))
        .with_state(state)
        // Properly structure middleware stack
        .layer(
            ServiceBuilder::new()
                // Handle errors from TimeoutLayer
                .layer(HandleErrorLayer::new(handle_timeout_error))
                // Apply the timeout
                .layer(TimeoutLayer::new(Duration::from_secs(30)))
                // Apply CORS
                // .layer(CorsLayer::permissive()),
        )
}

// Handler for the new relationships endpoint
async fn update_relationships(
    State(state): State<Arc<ApiState>>,
    Json(req): Json<RelationshipsRequest>,
) -> impl IntoResponse {
    info!(
        "Processing relationship update request for DID: {}",
        req.did
    );

    // Verify request size limits to prevent abuse
    if req.mutes.len() > 1000 || req.blocks.len() > 1000 {
        warn!(
            "Excessive relationship data: mutes={}, blocks={}",
            req.mutes.len(),
            req.blocks.len()
        );
        return (
            StatusCode::BAD_REQUEST,
            "Request exceeds maximum allowable size",
        )
            .into_response();
    }

    match state
        .relationship_manager
        .update_relationships_batch(&req.did, &req.device_token, req.mutes, req.blocks)
        .await
    {
        Ok(_) => {
            info!("Successfully updated relationships for DID: {}", req.did);
            StatusCode::OK.into_response()
        }
        Err(e) => {
            if e.to_string().contains("Invalid device token") {
                // Authentication error
                warn!(
                    "Unauthorized relationship update attempt for DID: {}",
                    req.did
                );
                StatusCode::UNAUTHORIZED.into_response()
            } else {
                // Other errors - provide more information in the response
                error!("Error updating relationships: {}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Internal server error: {}", e),
                )
                    .into_response()
            }
        }
    }
}

// API handlers
async fn register_device(
    axum::extract::State(state): axum::extract::State<Arc<ApiState>>,
    Json(req): Json<RegisterRequest>,
) -> axum::response::Response {
    tracing::info!("Registering device for DID: {}", req.did);

    // Start a transaction to prevent race conditions
    let mut tx = match state.db_pool.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            tracing::error!("Error starting transaction: {}", e);
            return axum::response::Response::builder()
                .status(500)
                .body(axum::body::Body::from(format!("Database error: {}", e)))
                .unwrap();
        }
    };

    // Check for existing token within the transaction
    let existing_token = sqlx::query_as!(
        UserDevice,
        r#"
        SELECT id, did, device_token, created_at, updated_at
        FROM user_devices
        WHERE device_token = $1
        FOR UPDATE
        "#,
        req.device_token
    )
    .fetch_optional(&mut *tx)
    .await;

    match existing_token {
        Ok(Some(device)) => {
            if device.did == req.did {
                // Device already registered with this DID - return success
                let _ = tx.commit().await;
                tracing::info!("Device already registered with same DID");
                return axum::response::Response::builder()
                    .status(200)
                    .body(axum::body::Body::empty())
                    .unwrap();
            } else {
                // Update the DID
                tracing::info!("Updating device from DID {} to {}", device.did, req.did);
                let result = sqlx::query!(
                    r#"
                    UPDATE user_devices
                    SET did = $1, updated_at = NOW()
                    WHERE device_token = $2
                    "#,
                    req.did,
                    req.device_token
                )
                .execute(&mut *tx)
                .await;

                match result {
                    Ok(_) => {
                        // Commit transaction
                        if let Err(e) = tx.commit().await {
                            tracing::error!("Error committing transaction: {}", e);
                            return axum::response::Response::builder()
                                .status(500)
                                .body(axum::body::Body::from(format!("Database error: {}", e)))
                                .unwrap();
                        }

                        tracing::info!("Device token updated successfully");
                        return axum::response::Response::builder()
                            .status(200)
                            .body(axum::body::Body::empty())
                            .unwrap();
                    }
                    Err(e) => {
                        let _ = tx.rollback().await;
                        tracing::error!("Error updating device: {}", e);
                        return axum::response::Response::builder()
                            .status(500)
                            .body(axum::body::Body::from(format!("Database error: {}", e)))
                            .unwrap();
                    }
                }
            }
        }
        Ok(None) => {
            // Create new device record
            tracing::info!("Creating new device registration");
            let result = sqlx::query!(
                r#"
                INSERT INTO user_devices (did, device_token)
                VALUES ($1, $2)
                RETURNING id
                "#,
                req.did,
                req.device_token
            )
            .fetch_one(&mut *tx)
            .await;

            match result {
                Ok(row) => {
                    // Create default preferences
                    match sqlx::query!(
                        r#"
                        INSERT INTO notification_preferences (user_id)
                        VALUES ($1)
                        "#,
                        row.id
                    )
                    .execute(&mut *tx)
                    .await
                    {
                        Ok(_) => {
                            // Commit transaction
                            if let Err(e) = tx.commit().await {
                                tracing::error!("Error committing transaction: {}", e);
                                return axum::response::Response::builder()
                                    .status(500)
                                    .body(axum::body::Body::from(format!("Database error: {}", e)))
                                    .unwrap();
                            }

                            tracing::info!("Device registered successfully");
                            return axum::response::Response::builder()
                                .status(201)
                                .body(axum::body::Body::empty())
                                .unwrap();
                        }
                        Err(e) => {
                            let _ = tx.rollback().await;
                            tracing::error!("Error creating preferences: {}", e);
                            return axum::response::Response::builder()
                                .status(500)
                                .body(axum::body::Body::from(format!("Database error: {}", e)))
                                .unwrap();
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.rollback().await;
                    tracing::error!("Error registering device: {}", e);
                    return axum::response::Response::builder()
                        .status(500)
                        .body(axum::body::Body::from(format!("Database error: {}", e)))
                        .unwrap();
                }
            }
        }
        Err(e) => {
            let _ = tx.rollback().await;
            tracing::error!("Database error: {}", e);
            return axum::response::Response::builder()
                .status(500)
                .body(axum::body::Body::from(format!("Database error: {}", e)))
                .unwrap();
        }
    }
}
async fn get_preferences(
    axum::extract::State(state): axum::extract::State<Arc<ApiState>>,
    Query(query): Query<PreferencesQuery>,
) -> Result<Json<PreferencesRequest>, axum::http::StatusCode> {
    // Find user devices
    let device = sqlx::query_as!(
        UserDevice,
        r#"
        SELECT id, did, device_token, created_at, updated_at
        FROM user_devices
        WHERE did = $1
        LIMIT 1
        "#,
        query.did,
    )
    .fetch_optional(&state.db_pool)
    .await
    .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

    let device = device.ok_or(axum::http::StatusCode::NOT_FOUND)?;

    // Get preferences
    let prefs = sqlx::query_as!(
        NotificationPreference,
        r#"
        SELECT user_id, mentions, replies, likes, follows, reposts, quotes
        FROM notification_preferences
        WHERE user_id = $1
        "#,
        device.id
    )
    .fetch_one(&state.db_pool)
    .await
    .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(PreferencesRequest {
        did: query.did,
        mentions: prefs.mentions,
        replies: prefs.replies,
        likes: prefs.likes,
        follows: prefs.follows,
        reposts: prefs.reposts,
        quotes: prefs.quotes,
    }))
}

async fn update_preferences(
    axum::extract::State(state): axum::extract::State<Arc<ApiState>>,
    Json(req): Json<PreferencesRequest>,
) -> axum::http::StatusCode {
    // Find ALL user devices for this DID (remove the LIMIT 1)
    let devices = sqlx::query_as!(
        UserDevice,
        r#"
        SELECT id, did, device_token, created_at, updated_at
        FROM user_devices
        WHERE did = $1
        "#,
        req.did,
    )
    .fetch_all(&state.db_pool)
    .await;

    match devices {
        Ok(devices) if !devices.is_empty() => {
            // Update preferences for ALL devices associated with this DID
            let mut success = true;
            for device in devices {
                let result = sqlx::query!(
                    r#"
                    UPDATE notification_preferences
                    SET mentions = $1, replies = $2, likes = $3, follows = $4, reposts = $5, quotes = $6
                    WHERE user_id = $7
                    "#,
                    req.mentions,
                    req.replies,
                    req.likes,
                    req.follows,
                    req.reposts,
                    req.quotes,
                    device.id
                )
                .execute(&state.db_pool)
                .await;
                
                if result.is_err() {
                    success = false;
                }
            }
            
            if success {
                axum::http::StatusCode::OK
            } else {
                axum::http::StatusCode::INTERNAL_SERVER_ERROR
            }
        },
        Ok(_) => axum::http::StatusCode::NOT_FOUND,
        Err(_) => axum::http::StatusCode::INTERNAL_SERVER_ERROR,
    }
}

// Add health check handler
async fn health_check(State(state): State<Arc<ApiState>>) -> impl IntoResponse {
    // Check DB connection
    match sqlx::query("SELECT 1").fetch_one(&state.db_pool).await {
        Ok(_) => (StatusCode::OK, "Healthy"),
        Err(e) => {
            error!("Health check failed: {}", e);
            (StatusCode::SERVICE_UNAVAILABLE, "Unhealthy: Database issue")
        }
    }
}

// Add metrics endpoint handler
async fn metrics_endpoint() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain")],
        crate::metrics::metrics_handler(),
    )
}
