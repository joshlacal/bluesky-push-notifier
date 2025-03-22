use axum::{
    extract::{Json, Query},
    routing::{get, post, put},
    Router,
};
use serde::{Deserialize, Serialize};
use sqlx::{Pool, Postgres};
use std::sync::Arc;
use tower_http::cors::CorsLayer;

use crate::models::{NotificationPreference, UserDevice};

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

// API state
pub struct ApiState {
    pub db_pool: Pool<Postgres>,
}

// Set up API router
pub fn create_api_router(state: Arc<ApiState>) -> Router {
    Router::new()
        .route("/register", post(register_device))
        .route("/preferences", get(get_preferences))
        .route("/preferences", put(update_preferences))
        .with_state(state)
        .layer(CorsLayer::permissive()) // For development - restrict in production
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
                    .await {
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
    // Find user device
    let device = sqlx::query_as!(
        UserDevice,
        r#"
        SELECT id, did, device_token, created_at, updated_at
        FROM user_devices
        WHERE did = $1
        LIMIT 1
        "#,
        req.did,
    )
    .fetch_optional(&state.db_pool)
    .await;

    match device {
        Ok(Some(device)) => {
            // Update preferences
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

            match result {
                Ok(_) => axum::http::StatusCode::OK,
                Err(_) => axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            }
        }
        Ok(None) => axum::http::StatusCode::NOT_FOUND,
        Err(_) => axum::http::StatusCode::INTERNAL_SERVER_ERROR,
    }
}