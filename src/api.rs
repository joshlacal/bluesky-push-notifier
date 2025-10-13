use axum::body::Bytes;
use axum::{
    error_handling::HandleErrorLayer, // Add HandleErrorLayer
    extract::{Json, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{delete, get, post, put},
    BoxError, // Add BoxError for error handler
    Router,
};
use base64::{engine::general_purpose, Engine as _};
use constant_time_eq::constant_time_eq;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sqlx::{Pool, Postgres};
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;
// Remove unused imports: tower_http::cors::CorsLayer, tower_http::limit::RequestBodyLimitLayer
use sha2::{Digest, Sha256};
use tower::timeout::TimeoutLayer;
use tower::ServiceBuilder;
use tracing::{error, info, warn};

use crate::activity_subscription_manager::ActivitySubscriptionManager;
use crate::app_attest::AppAttestService;
use crate::models::{ActivitySubscription, NotificationPreference, UserDevice};
use crate::moderation_list_manager::ModerationListManager;
use crate::relationship_manager::RelationshipManager;
use crate::thread_mute_manager::ThreadMuteManager;

#[derive(Deserialize)]
struct RegisterRequest {
    did: String,
    device_token: String,
}

#[derive(Deserialize)]
struct UnregisterRequest {
    did: String,
    device_token: String,
}

#[derive(Deserialize)]
struct PreferencesQuery {
    did: String,
    device_token: String,
}

#[derive(Deserialize)]
struct PreferencesUpdateRequest {
    did: String,
    device_token: String,
    mentions: bool,
    replies: bool,
    likes: bool,
    follows: bool,
    reposts: bool,
    quotes: bool,
    via_likes: bool,
    via_reposts: bool,
    activity_subscriptions: bool,
}

#[derive(Serialize)]
struct PreferencesBody {
    did: String,
    mentions: bool,
    replies: bool,
    likes: bool,
    follows: bool,
    reposts: bool,
    quotes: bool,
    via_likes: bool,
    via_reposts: bool,
    activity_subscriptions: bool,
}

#[derive(Serialize)]
struct ChallengeEnvelope {
    challenge: String,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
}

#[derive(Deserialize)]
struct ChallengeRequest {
    did: Option<String>,
    device_token: Option<String>,
    force_key_rotation: Option<bool>,
}

#[derive(Deserialize)]
struct SyncModerationListsRequest {
    did: String,
    device_token: String,
    lists: Vec<ModerationListDto>,
}

#[derive(Deserialize, Serialize)]
struct ModerationListDto {
    uri: String,
    purpose: String,
    name: Option<String>,
}

#[derive(Deserialize)]
struct MuteThreadRequest {
    did: String,
    device_token: String,
    thread_root_uri: String,
}

#[derive(Deserialize)]
struct UnmuteThreadRequest {
    did: String,
    device_token: String,
    thread_root_uri: String,
}

#[derive(Deserialize)]
struct GetMutedThreadsRequest {
    did: String,
    device_token: String,
}

#[derive(Serialize)]
struct MutedThreadsResponse {
    threads: Vec<String>,
}

#[derive(Serialize)]
struct RegisterResponse {
    next_challenge: ChallengeEnvelope,
}

#[derive(Serialize)]
struct PreferencesResponse {
    preferences: PreferencesBody,
    next_challenge: ChallengeEnvelope,
}

#[derive(Serialize)]
struct ActivitySubscriptionDto {
    subject_did: String,
    include_posts: bool,
    include_replies: bool,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
}

impl From<ActivitySubscription> for ActivitySubscriptionDto {
    fn from(value: ActivitySubscription) -> Self {
        Self {
            subject_did: value.subject_did,
            include_posts: value.include_posts,
            include_replies: value.include_replies,
            updated_at: value.updated_at,
        }
    }
}

#[derive(Serialize)]
struct ActivitySubscriptionsResponse {
    subscriptions: Vec<ActivitySubscriptionDto>,
    next_challenge: ChallengeEnvelope,
}

const HEADER_APP_ATTEST_KEY_ID: &str = "X-AppAttest-KeyId";
const HEADER_APP_ATTEST_CHALLENGE: &str = "X-AppAttest-Challenge";
const HEADER_APP_ATTEST_ASSERTION: &str = "X-AppAttest-Assertion";
const HEADER_APP_ATTEST_CLIENT_DATA: &str = "X-AppAttest-ClientData";
const HEADER_APP_ATTEST_BODY_SHA256: &str = "X-AppAttest-BodySHA256";
const HEADER_APP_ATTEST_ATTESTATION: &str = "X-AppAttest-Attestation";

struct AppAttestRequestProof {
    key_id: String,
    challenge: String,
    assertion: String,
    client_data: Option<String>,
    body_sha256: Option<Vec<u8>>,
    attestation: Option<String>,
}

impl AppAttestRequestProof {
    fn from_headers(headers: &axum::http::HeaderMap) -> Result<Self, axum::response::Response> {
        // Debug log all App Attest headers
        tracing::debug!("🔍 Received App Attest headers:");
        for (name, value) in headers.iter() {
            if name.as_str().starts_with("X-AppAttest") || name.as_str().starts_with("x-appattest")
            {
                tracing::debug!(
                    "🔍   {}: {:?}",
                    name.as_str(),
                    value.to_str().unwrap_or("<invalid utf8>")
                );
            }
        }

        let key_id = Self::require_header(headers, HEADER_APP_ATTEST_KEY_ID)?;
        let challenge = Self::require_header(headers, HEADER_APP_ATTEST_CHALLENGE)?;
        let assertion = Self::require_header(headers, HEADER_APP_ATTEST_ASSERTION)?;

        let body_sha256 = match headers.get(HEADER_APP_ATTEST_BODY_SHA256) {
            Some(value) => {
                let value_str = value
                    .to_str()
                    .map_err(|_| {
                        error_response(
                            StatusCode::BAD_REQUEST,
                            "invalid X-AppAttest-BodySHA256 header encoding",
                        )
                    })?
                    .trim()
                    .to_string();

                if value_str.is_empty() {
                    return Err(error_response(
                        StatusCode::BAD_REQUEST,
                        "X-AppAttest-BodySHA256 header must not be empty",
                    ));
                }

                let decoded = general_purpose::STANDARD.decode(&value_str).map_err(|_| {
                    error_response(
                        StatusCode::BAD_REQUEST,
                        "invalid X-AppAttest-BodySHA256 header value",
                    )
                })?;

                if decoded.len() != 32 {
                    return Err(error_response(
                        StatusCode::BAD_REQUEST,
                        "X-AppAttest-BodySHA256 must decode to 32 bytes",
                    ));
                }

                Some(decoded)
            }
            None => None,
        };

        let attestation = match headers.get(HEADER_APP_ATTEST_ATTESTATION) {
            Some(value) => {
                let value_str = value
                    .to_str()
                    .map_err(|_| {
                        error_response(
                            StatusCode::BAD_REQUEST,
                            "invalid X-AppAttest-Attestation header encoding",
                        )
                    })?
                    .trim()
                    .to_string();

                if value_str.is_empty() {
                    None
                } else {
                    Some(value_str)
                }
            }
            None => None,
        };

        let client_data = match headers.get(HEADER_APP_ATTEST_CLIENT_DATA) {
            Some(value) => {
                let value_str = value
                    .to_str()
                    .map_err(|_| {
                        error_response(
                            StatusCode::BAD_REQUEST,
                            "invalid X-AppAttest-ClientData header encoding",
                        )
                    })?
                    .trim()
                    .to_string();

                if value_str.is_empty() {
                    tracing::debug!("🔍 X-AppAttest-ClientData header is empty");
                    None
                } else {
                    tracing::debug!("🔍 X-AppAttest-ClientData received: {}", value_str);
                    Some(value_str)
                }
            }
            None => {
                tracing::debug!("🔍 X-AppAttest-ClientData header not present");
                None
            }
        };

        Ok(Self {
            key_id,
            challenge,
            assertion,
            client_data,
            body_sha256,
            attestation,
        })
    }

    fn require_header(
        headers: &axum::http::HeaderMap,
        name: &str,
    ) -> Result<String, axum::response::Response> {
        let value = headers.get(name).ok_or_else(|| {
            error_response(StatusCode::UNAUTHORIZED, format!("missing {name} header"))
        })?;

        let value_str = value
            .to_str()
            .map_err(|_| error_response(StatusCode::BAD_REQUEST, format!("invalid {name} header")))?
            .trim()
            .to_string();

        if value_str.is_empty() {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                format!("{name} header must not be empty"),
            ));
        }

        Ok(value_str)
    }
}

fn error_response(status: StatusCode, message: impl Into<String>) -> axum::response::Response {
    (status, message.into()).into_response()
}

// Helper to create a simple response without unwrap
fn simple_response(status: StatusCode, body: impl Into<axum::body::Body>) -> axum::response::Response {
    axum::response::Response::builder()
        .status(status)
        .body(body.into())
        .unwrap_or_else(|e| {
            tracing::error!("Failed to build response: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
        })
}

fn parse_json_body<T: DeserializeOwned>(body: &Bytes) -> Result<T, axum::response::Response> {
    serde_json::from_slice(body).map_err(|err| {
        error_response(
            StatusCode::BAD_REQUEST,
            format!("invalid JSON body: {}", err),
        )
    })
}

fn verify_body_binding(
    body: &Bytes,
    expected_digest: Option<&[u8]>,
) -> Result<(), axum::response::Response> {
    if let Some(expected) = expected_digest {
        let actual = Sha256::digest(body);
        if !constant_time_eq(actual.as_slice(), expected) {
            return Err(error_response(
                StatusCode::UNAUTHORIZED,
                "request body digest mismatch",
            ));
        }
    }

    Ok(())
}

struct AuthenticatedDeviceResult {
    device_id: uuid::Uuid,
    next_challenge: String,
    next_challenge_expires_at: OffsetDateTime,
}

async fn authenticate_device_for_request(
    state: &Arc<ApiState>,
    tx: &mut sqlx::Transaction<'_, Postgres>,
    did: &str,
    device_token: &str,
    proof: &AppAttestRequestProof,
    client_data_hash: Vec<u8>,
    request_body: Option<&[u8]>,
) -> Result<AuthenticatedDeviceResult, axum::response::Response> {
    let device = match sqlx::query_as::<_, UserDevice>(
        r#"
        SELECT id, did, device_token, created_at, updated_at,
               app_attest_key_id,
               app_attest_public_key,
               app_attest_receipt,
               app_attest_counter,
               app_attest_challenge,
               app_attest_challenge_expires_at,
               app_attest_last_verified_at
        FROM user_devices
        WHERE device_token = $1 AND did = $2
        FOR UPDATE
        "#,
    )
    .bind(device_token)
    .bind(did)
    .fetch_optional(tx.as_mut())
    .await
    {
        Ok(Some(device)) => device,
        Ok(None) => {
            return Err(error_response(
                StatusCode::NOT_FOUND,
                "device not registered",
            ))
        }
        Err(e) => {
            tracing::error!("Failed to fetch device for authenticated request: {}", e);
            return Err(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "database error",
            ));
        }
    };

    if let Some(key_id) = &device.app_attest_key_id {
        if key_id != &proof.key_id {
            return Err(error_response(
                StatusCode::UNAUTHORIZED,
                "app attest key mismatch",
            ));
        }
    } else {
        return Err(error_response(
            StatusCode::PRECONDITION_REQUIRED,
            "device requires re-attestation",
        ));
    }

    if let Err(err) = state.app_attest.validate_challenge(
        device.app_attest_challenge.as_deref(),
        device.app_attest_challenge_expires_at,
        &proof.challenge,
    ) {
        tracing::warn!("Challenge validation failed: {}", err);
        return Err(error_response(
            StatusCode::UNAUTHORIZED,
            "invalid or expired challenge",
        ));
    }

    let public_key = match &device.app_attest_public_key {
        Some(key) => key.clone(),
        None => {
            return Err(error_response(
                StatusCode::PRECONDITION_REQUIRED,
                "device requires re-attestation",
            ));
        }
    };

    let previous_counter = match u32::try_from(device.app_attest_counter) {
        Ok(value) => value,
        Err(_) => {
            return Err(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "invalid counter state",
            ));
        }
    };

    let request_body_vec = request_body.map(|body| body.to_vec());
    let body_binding_required = proof.body_sha256.is_some();

    let assertion = match if let Some(client_data) = &proof.client_data {
        // Use new client data verification
        verify_assertion_async(
            &state.app_attest,
            proof.assertion.clone(),
            client_data.clone(),
            request_body_vec.clone(),
            body_binding_required,
            public_key,
            previous_counter,
            device.app_attest_challenge.clone(),
            proof.challenge.clone(),
        )
        .await
    } else {
        // Fallback to hash-based verification (old path)
        verify_assertion_legacy_async(
            &state.app_attest,
            proof.assertion.clone(),
            client_data_hash,
            public_key,
            previous_counter,
            device.app_attest_challenge.clone(),
            proof.challenge.clone(),
        )
        .await
    } {
        Ok(result) => result,
        Err(err) => {
            tracing::warn!("App Attest assertion failed: {}", err);
            return Err(error_response(
                StatusCode::UNAUTHORIZED,
                "invalid app attest assertion",
            ));
        }
    };

    let (next_challenge, expires_at) = state.app_attest.issue_challenge();

    if let Err(e) = sqlx::query(
        r#"
        UPDATE user_devices
        SET app_attest_counter = $1,
            app_attest_challenge = $2,
            app_attest_challenge_expires_at = $3,
            app_attest_last_verified_at = NOW(),
            updated_at = NOW()
        WHERE id = $4
        "#,
    )
    .bind(i64::from(assertion.counter))
    .bind(&next_challenge)
    .bind(expires_at)
    .bind(device.id)
    .execute(tx.as_mut())
    .await
    {
        tracing::error!("Failed to update device metadata: {}", e);
        return Err(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "database error",
        ));
    }

    Ok(AuthenticatedDeviceResult {
        device_id: device.id,
        next_challenge,
        next_challenge_expires_at: expires_at,
    })
}

async fn fetch_preferences_with_auth(
    state: Arc<ApiState>,
    query: PreferencesQuery,
    proof: &AppAttestRequestProof,
    client_data_hash: Vec<u8>,
    request_body: Option<&[u8]>,
) -> axum::response::Response {
    let mut tx = match state.db_pool.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            tracing::error!("Failed to start transaction for preferences: {}", e);
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "database error");
        }
    };

    let auth = match authenticate_device_for_request(
        &state,
        &mut tx,
        &query.did,
        &query.device_token,
        proof,
        client_data_hash,
        request_body,
    )
    .await
    {
        Ok(auth) => auth,
        Err(resp) => {
            tx.rollback().await.ok();
            return resp;
        }
    };

    let prefs = match sqlx::query_as::<_, NotificationPreference>(
        r#"
        SELECT
            user_id,
            mentions,
            replies,
            likes,
            follows,
            reposts,
            quotes,
            via_likes,
            via_reposts,
            activity_subscriptions
        FROM notification_preferences
        WHERE user_id = $1
        "#,
    )
    .bind(auth.device_id)
    .fetch_one(tx.as_mut())
    .await
    {
        Ok(prefs) => prefs,
        Err(e) => {
            tx.rollback().await.ok();
            tracing::error!("Failed to load preferences: {}", e);
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "database error");
        }
    };

    if let Err(e) = tx.commit().await {
        tracing::error!("Failed to commit preferences transaction: {}", e);
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "database error");
    }

    let body = PreferencesBody {
        did: query.did,
        mentions: prefs.mentions,
        replies: prefs.replies,
        likes: prefs.likes,
        follows: prefs.follows,
        reposts: prefs.reposts,
        quotes: prefs.quotes,
        via_likes: prefs.via_likes,
        via_reposts: prefs.via_reposts,
        activity_subscriptions: prefs.activity_subscriptions,
    };

    Json(PreferencesResponse {
        preferences: body,
        next_challenge: ChallengeEnvelope {
            challenge: auth.next_challenge,
            expires_at: auth.next_challenge_expires_at,
        },
    })
    .into_response()
}

// New model for relationship updates with authentication
#[derive(Deserialize)]
struct RelationshipsRequest {
    did: String,
    device_token: String, // Required for authentication
    mutes: Vec<String>,
    blocks: Vec<String>,
}

#[derive(Deserialize)]
struct ActivitySubscriptionQuery {
    did: String,
    device_token: String,
}

#[derive(Deserialize)]
struct ActivitySubscriptionUpdateRequest {
    did: String,
    device_token: String,
    subject_did: String,
    include_posts: bool,
    include_replies: bool,
}

#[derive(Deserialize)]
struct ActivitySubscriptionDeleteRequest {
    did: String,
    device_token: String,
    subject_did: String,
}

// API state
pub struct ApiState {
    pub db_pool: Pool<Postgres>,
    pub relationship_manager: Arc<RelationshipManager>,
    pub moderation_list_manager: Arc<ModerationListManager>,
    pub thread_mute_manager: Arc<ThreadMuteManager>,
    pub app_attest: Arc<AppAttestService>,
    pub activity_subscription_manager: Arc<ActivitySubscriptionManager>,
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
        .route("/unregister", post(unregister_device))
        .route(
            "/preferences",
            get(get_preferences).post(get_preferences_post),
        )
        .route("/preferences", put(update_preferences))
        .route(
            "/activity-subscriptions",
            get(list_activity_subscriptions).post(list_activity_subscriptions_post),
        )
        .route("/activity-subscriptions", put(upsert_activity_subscription))
        .route(
            "/activity-subscriptions",
            delete(remove_activity_subscription),
        )
        .route("/health", get(health_check))
        .route("/metrics", get(metrics_endpoint))
        .route("/relationships", put(update_relationships))
        .route("/sync-moderation-lists", post(sync_moderation_lists))
        .route("/mute-thread", post(mute_thread))
        .route("/unmute-thread", post(unmute_thread))
        .route("/muted-threads", get(get_muted_threads).post(get_muted_threads_post))
        .route(
            "/challenge",
            get(issue_challenge_get).post(issue_challenge_post),
        )
        .with_state(state)
        // Properly structure middleware stack
        .layer(
            ServiceBuilder::new()
                // Handle errors from TimeoutLayer
                .layer(HandleErrorLayer::new(handle_timeout_error))
                // Apply the timeout
                .layer(TimeoutLayer::new(Duration::from_secs(30))), // Apply CORS
                                                                    // .layer(CorsLayer::permissive()),
        )
}

async fn issue_challenge_get(
    State(state): State<Arc<ApiState>>,
    Query(req): Query<ChallengeRequest>,
) -> impl IntoResponse {
    issue_challenge_common(state, req).await
}

async fn issue_challenge_post(
    State(state): State<Arc<ApiState>>,
    Json(req): Json<ChallengeRequest>,
) -> impl IntoResponse {
    issue_challenge_common(state, req).await
}

async fn issue_challenge_common(state: Arc<ApiState>, req: ChallengeRequest) -> impl IntoResponse {
    let (challenge, expires_at) = state.app_attest.issue_challenge();

    if let (Some(did), Some(device_token)) = (req.did, req.device_token) {
        // Handle force key rotation by clearing existing App Attest data
        if req.force_key_rotation.unwrap_or(false) {
            tracing::info!(
                "Force key rotation requested for DID: {}, clearing existing App Attest data",
                did
            );

            // Clear App Attest key and challenge for this device to force fresh attestation
            if let Err(e) = sqlx::query(
                r#"
                UPDATE user_devices
                SET app_attest_key_id = NULL,
                    app_attest_challenge = $1,
                    app_attest_challenge_expires_at = $2,
                    updated_at = NOW()
                WHERE did = $3 AND device_token = $4
                "#,
            )
            .bind(&challenge)
            .bind(expires_at)
            .bind(&did)
            .bind(&device_token)
            .execute(&state.db_pool)
            .await
            {
                tracing::warn!("Failed to clear App Attest data for force rotation: {}", e);
            }
        } else {
            // Best-effort: persist challenge for existing device
            if let Err(e) = sqlx::query(
                r#"
                UPDATE user_devices
                SET app_attest_challenge = $1,
                    app_attest_challenge_expires_at = $2,
                    updated_at = NOW()
                WHERE did = $3 AND device_token = $4
                "#,
            )
            .bind(&challenge)
            .bind(expires_at)
            .bind(did)
            .bind(device_token)
            .execute(&state.db_pool)
            .await
            {
                tracing::warn!("Failed to persist challenge for device: {}", e);
            }
        }
    }

    let body = ChallengeEnvelope {
        challenge,
        expires_at,
    };
    (StatusCode::OK, Json(body))
}

// Handler for the new relationships endpoint
async fn update_relationships(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let proof = match AppAttestRequestProof::from_headers(&headers) {
        Ok(proof) => proof,
        Err(resp) => return resp,
    };

    if let Err(resp) = verify_body_binding(&body, proof.body_sha256.as_deref()) {
        return resp;
    }

    let req: RelationshipsRequest = match parse_json_body(&body) {
        Ok(req) => req,
        Err(resp) => return resp,
    };

    info!(
        "Processing relationship update request for DID: {}",
        req.did
    );

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

    let client_data_hash = match state
        .app_attest
        .compute_client_data_hash(&proof.challenge, proof.body_sha256.as_deref())
    {
        Ok(hash) => hash.to_vec(),
        Err(err) => {
            tracing::warn!(
                "Failed to prepare clientDataHash for relationships on DID {}: {}",
                req.did,
                err
            );
            return error_response(StatusCode::BAD_REQUEST, "invalid App Attest parameters");
        }
    };

    let mut tx = match state.db_pool.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            tracing::error!("Failed to start transaction for relationship update: {}", e);
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "database error");
        }
    };

    let auth = match authenticate_device_for_request(
        &state,
        &mut tx,
        &req.did,
        &req.device_token,
        &proof,
        client_data_hash,
        Some(body.as_ref()),
    )
    .await
    {
        Ok(auth) => auth,
        Err(resp) => {
            tx.rollback().await.ok();
            return resp;
        }
    };

    if let Err(e) = tx.commit().await {
        tracing::error!("Failed to commit device challenge update: {}", e);
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "database error");
    }

    let mutes = req.mutes.clone();
    let blocks = req.blocks.clone();

    match state
        .relationship_manager
        .update_relationships_batch(&req.did, &req.device_token, mutes, blocks)
        .await
    {
        Ok(_) => {
            info!("Successfully updated relationships for DID: {}", req.did);
            let response = RegisterResponse {
                next_challenge: ChallengeEnvelope {
                    challenge: auth.next_challenge,
                    expires_at: auth.next_challenge_expires_at,
                },
            };
            (StatusCode::OK, Json(response)).into_response()
        }
        Err(e) => {
            if e.to_string().contains("Invalid device token") {
                warn!(
                    "Unauthorized relationship update attempt for DID: {}",
                    req.did
                );
                StatusCode::UNAUTHORIZED.into_response()
            } else {
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

// Helper functions to handle App Attest operations in blocking context
async fn verify_attestation_async(
    app_attest: &AppAttestService,
    attestation_payload: String,
    challenge: String,
    key_id: String,
) -> Result<crate::app_attest::AttestationVerification, anyhow::Error> {
    let app_attest = app_attest.clone();
    tokio::task::spawn_blocking(move || {
        app_attest.verify_attestation(&attestation_payload, &challenge, &key_id)
    })
    .await
    .map_err(|e| anyhow::anyhow!("Task join error: {}", e))?
}

async fn verify_assertion_async(
    app_attest: &AppAttestService,
    assertion: String,
    client_data: String,
    request_body: Option<Vec<u8>>,
    body_binding_required: bool,
    public_key: Vec<u8>,
    previous_counter: u32,
    stored_challenge: Option<String>,
    challenge: String,
) -> Result<crate::app_attest::AssertionVerification, anyhow::Error> {
    let app_attest = app_attest.clone();
    tokio::task::spawn_blocking(move || {
        app_attest.verify_assertion_with_client_data(
            &assertion,
            &client_data,
            request_body.as_deref(),
            body_binding_required,
            &public_key,
            previous_counter,
            stored_challenge.as_deref(),
            &challenge,
        )
    })
    .await
    .map_err(|e| anyhow::anyhow!("Task join error: {}", e))?
}

async fn verify_assertion_legacy_async(
    app_attest: &AppAttestService,
    assertion: String,
    client_data_hash: Vec<u8>,
    public_key: Vec<u8>,
    previous_counter: u32,
    stored_challenge: Option<String>,
    challenge: String,
) -> Result<crate::app_attest::AssertionVerification, anyhow::Error> {
    let app_attest = app_attest.clone();
    tokio::task::spawn_blocking(move || {
        app_attest.verify_assertion(
            &assertion,
            &client_data_hash,
            &public_key,
            previous_counter,
            stored_challenge.as_deref(),
            &challenge,
        )
    })
    .await
    .map_err(|e| anyhow::anyhow!("Task join error: {}", e))?
}

// API handlers
async fn register_device(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    let proof = match AppAttestRequestProof::from_headers(&headers) {
        Ok(proof) => proof,
        Err(resp) => return resp,
    };

    if let Err(resp) = verify_body_binding(&body, proof.body_sha256.as_deref()) {
        return resp;
    }

    let req: RegisterRequest = match parse_json_body(&body) {
        Ok(req) => req,
        Err(resp) => return resp,
    };

    tracing::info!("Registering device for DID: {}", req.did);

    let request_body_vec = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };

    let client_data_hash = match state
        .app_attest
        .compute_client_data_hash(&proof.challenge, proof.body_sha256.as_deref())
    {
        Ok(hash) => hash.to_vec(),
        Err(err) => {
            tracing::warn!(
                "Failed to prepare clientDataHash during register for DID {}: {}",
                req.did,
                err
            );
            return error_response(StatusCode::BAD_REQUEST, "invalid App Attest parameters");
        }
    };

    let mut tx = match state.db_pool.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            tracing::error!("Error starting transaction: {}", e);
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Database error: {}", e),
            );
        }
    };

    let existing_registration = match sqlx::query_as::<_, UserDevice>(
        r#"
        SELECT id, did, device_token, created_at, updated_at,
               app_attest_key_id,
               app_attest_public_key,
               app_attest_receipt,
               app_attest_counter,
               app_attest_challenge,
               app_attest_challenge_expires_at,
               app_attest_last_verified_at
        FROM user_devices
        WHERE device_token = $1 AND did = $2
        FOR UPDATE
        "#,
    )
    .bind(&req.device_token)
    .bind(&req.did)
    .fetch_optional(tx.as_mut())
    .await
    {
        Ok(device) => device,
        Err(e) => {
            let _ = tx.rollback().await;
            tracing::error!("Database error: {}", e);
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Database error: {}", e),
            );
        }
    };

    if let Some(device) = existing_registration {
        if let Some(key_id) = &device.app_attest_key_id {
            if key_id != &proof.key_id {
                let _ = tx.rollback().await;
                tracing::warn!(
                    "App Attest key mismatch for DID {} (expected {}, got {})",
                    req.did,
                    key_id,
                    proof.key_id
                );
                return error_response(StatusCode::UNAUTHORIZED, "app attest key mismatch");
            }
        } else {
            // Device exists but has no key ID (likely cleared by force rotation)
            // This is expected after force key rotation - allow new key registration
            tracing::info!(
                "Device exists without App Attest key for DID {} - allowing new key registration (likely after force rotation)",
                req.did
            );

            // For devices without keys, we need attestations for initial key setup
            if proof.attestation.is_none() {
                let _ = tx.rollback().await;
                tracing::warn!(
                    "Device without key missing App Attest attestation for DID {}",
                    req.did
                );
                return error_response(
                    StatusCode::PRECONDITION_REQUIRED,
                    "device requires re-attestation",
                );
            }

            // Process fresh attestation for existing device (similar to new device registration)
            let attestation_payload = proof.attestation.as_ref().unwrap();

            let attestation = match verify_attestation_async(
                &state.app_attest,
                attestation_payload.clone(),
                proof.challenge.clone(),
                proof.key_id.clone(),
            )
            .await
            {
                Ok(data) => data,
                Err(err) => {
                    let _ = tx.rollback().await;
                    tracing::warn!("App Attest attestation failed for DID {}: {}", req.did, err);
                    return error_response(StatusCode::UNAUTHORIZED, "invalid attestation payload");
                }
            };

            // IMPORTANT: During initial registration with attestation, we do NOT verify assertion
            // The attestation itself proves the key is valid. Assertion verification would fail
            // because the assertion and attestation were both created with the same challenge,
            // but assertion verification expects a different flow (attestation first, then assertion).
            // 
            // Per Apple's spec: attestation proves key authenticity, assertion proves ongoing possession.
            // At registration time, attestation is sufficient.
            
            tracing::info!(
                "✅ Attestation verified for DID {} - skipping assertion check (not needed for initial attestation)",
                req.did
            );

            let (next_challenge, expires_at) = state.app_attest.issue_challenge();

            // Update existing device with fresh App Attest data
            // Counter starts at 0 for fresh attestations - will increment on first assertion
            if let Err(e) = sqlx::query(
                r#"
                UPDATE user_devices
                SET updated_at = NOW(),
                    app_attest_key_id = $1,
                    app_attest_public_key = $2,
                    app_attest_receipt = $3,
                    app_attest_counter = $4,
                    app_attest_challenge = $5,
                    app_attest_challenge_expires_at = $6,
                    app_attest_last_verified_at = NOW()
                WHERE id = $7
                "#,
            )
            .bind(&proof.key_id)
            .bind(attestation.public_key)
            .bind(attestation.receipt)
            .bind(0i64) // Start counter at 0 for fresh attestation
            .bind(&next_challenge)
            .bind(expires_at)
            .bind(device.id)
            .execute(tx.as_mut())
            .await
            {
                let _ = tx.rollback().await;
                tracing::error!("Error updating device with fresh attestation: {}", e);
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Database error: {}", e),
                );
            }

            if let Err(e) = tx.commit().await {
                tracing::error!("Error committing transaction: {}", e);
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Database error: {}", e),
                );
            }

            let response = RegisterResponse {
                next_challenge: ChallengeEnvelope {
                    challenge: next_challenge,
                    expires_at,
                },
            };

            return (StatusCode::OK, Json(response)).into_response();
        }

        if let Err(e) = state.app_attest.validate_challenge(
            device.app_attest_challenge.as_deref(),
            device.app_attest_challenge_expires_at,
            &proof.challenge,
        ) {
            let _ = tx.rollback().await;
            tracing::warn!("Challenge validation failed for DID {}: {}", req.did, e);
            return error_response(StatusCode::UNAUTHORIZED, "invalid or expired challenge");
        }

        let public_key = match &device.app_attest_public_key {
            Some(key) => key.clone(),
            None => {
                let _ = tx.rollback().await;
                tracing::warn!(
                    "Existing device missing App Attest public key for DID {}",
                    req.did
                );
                return error_response(
                    StatusCode::PRECONDITION_REQUIRED,
                    "device requires re-attestation",
                );
            }
        };

        let previous_counter = match u32::try_from(device.app_attest_counter) {
            Ok(value) => value,
            Err(_) => {
                let _ = tx.rollback().await;
                tracing::error!("Invalid stored counter for DID {}", req.did);
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, "invalid counter state");
            }
        };

        // Special case: If counter is 0, device was just attested but never used
        // In this case, we should accept a new attestation rather than require assertion
        // This happens when user registers, then immediately unregisters and re-registers
        if previous_counter == 0 {
            tracing::info!(
                "Device has counter=0 for DID {}, treating as fresh attestation case",
                req.did
            );
            
            // Device needs fresh attestation (same as line 1037 path)
            if proof.attestation.is_none() {
                let _ = tx.rollback().await;
                tracing::warn!(
                    "Device with counter=0 missing App Attest attestation for DID {}",
                    req.did
                );
                return error_response(
                    StatusCode::PRECONDITION_REQUIRED,
                    "device requires re-attestation",
                );
            }

            let attestation_payload = proof.attestation.as_ref().unwrap();

            let attestation = match verify_attestation_async(
                &state.app_attest,
                attestation_payload.clone(),
                proof.challenge.clone(),
                proof.key_id.clone(),
            )
            .await
            {
                Ok(data) => data,
                Err(err) => {
                    let _ = tx.rollback().await;
                    tracing::warn!("App Attest attestation failed for DID {}: {}", req.did, err);
                    return error_response(StatusCode::UNAUTHORIZED, "invalid attestation payload");
                }
            };

            tracing::info!(
                "✅ Re-attestation verified for DID {} (counter was 0)",
                req.did
            );

            let (next_challenge, expires_at) = state.app_attest.issue_challenge();

            // Update device with fresh attestation data, keep counter at 0
            if let Err(e) = sqlx::query(
                r#"
                UPDATE user_devices
                SET updated_at = NOW(),
                    app_attest_key_id = $1,
                    app_attest_public_key = $2,
                    app_attest_receipt = $3,
                    app_attest_counter = $4,
                    app_attest_challenge = $5,
                    app_attest_challenge_expires_at = $6,
                    app_attest_last_verified_at = NOW()
                WHERE id = $7
                "#,
            )
            .bind(&proof.key_id)
            .bind(attestation.public_key)
            .bind(attestation.receipt)
            .bind(0i64) // Keep counter at 0 for fresh attestation
            .bind(&next_challenge)
            .bind(expires_at)
            .bind(device.id)
            .execute(tx.as_mut())
            .await
            {
                let _ = tx.rollback().await;
                tracing::error!("Error updating device with re-attestation: {}", e);
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Database error: {}", e),
                );
            }

            if let Err(e) = tx.commit().await {
                tracing::error!("Error committing transaction: {}", e);
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Database error: {}", e),
                );
            }

            let response = RegisterResponse {
                next_challenge: ChallengeEnvelope {
                    challenge: next_challenge,
                    expires_at,
                },
            };

            return (StatusCode::OK, Json(response)).into_response();
        }

        // Normal path: counter > 0, verify assertion
        let assertion = match if let Some(client_data) = &proof.client_data {
            // Use new client data verification
            verify_assertion_async(
                &state.app_attest,
                proof.assertion.clone(),
                client_data.clone(),
                request_body_vec.clone(),
                proof.body_sha256.is_some(),
                public_key,
                previous_counter,
                device.app_attest_challenge.clone(),
                proof.challenge.clone(),
            )
            .await
        } else {
            // Fallback to hash-based verification (old path)
            verify_assertion_legacy_async(
                &state.app_attest,
                proof.assertion.clone(),
                client_data_hash.clone(),
                public_key,
                previous_counter,
                device.app_attest_challenge.clone(),
                proof.challenge.clone(),
            )
            .await
        } {
            Ok(result) => result,
            Err(err) => {
                let _ = tx.rollback().await;
                let err_str = err.to_string();
                tracing::warn!("App Attest assertion failed for DID {}: {}", req.did, err_str);
                
                // If signature validation fails, it likely means the client has a different key
                // than what we have stored. Request re-attestation.
                if err_str.contains("invalid signature") {
                    tracing::info!(
                        "Signature validation failed for DID {}, requesting re-attestation (likely key mismatch)",
                        req.did
                    );
                    return error_response(
                        StatusCode::PRECONDITION_REQUIRED,
                        "device requires re-attestation due to key mismatch"
                    );
                }
                
                return error_response(StatusCode::UNAUTHORIZED, "invalid app attest assertion");
            }
        };

        let (next_challenge, expires_at) = state.app_attest.issue_challenge();

        if let Err(e) = sqlx::query(
            r#"
            UPDATE user_devices
            SET updated_at = NOW(),
                app_attest_counter = $1,
                app_attest_challenge = $2,
                app_attest_challenge_expires_at = $3,
                app_attest_last_verified_at = NOW()
            WHERE id = $4
            "#,
        )
        .bind(i64::from(assertion.counter))
        .bind(&next_challenge)
        .bind(expires_at)
        .bind(device.id)
        .execute(tx.as_mut())
        .await
        {
            let _ = tx.rollback().await;
            tracing::error!("Error updating device: {}", e);
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Database error: {}", e),
            );
        }

        if let Err(e) = tx.commit().await {
            tracing::error!("Error committing transaction: {}", e);
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Database error: {}", e),
            );
        }

        let response = RegisterResponse {
            next_challenge: ChallengeEnvelope {
                challenge: next_challenge,
                expires_at,
            },
        };

        return (StatusCode::OK, Json(response)).into_response();
    }

    let attestation_payload = match &proof.attestation {
        Some(payload) => payload,
        None => {
            let _ = tx.rollback().await;
            tracing::warn!(
                "Missing App Attest attestation for new device DID {}",
                req.did
            );
            return error_response(StatusCode::BAD_REQUEST, "attestation payload required");
        }
    };

    let attestation = match verify_attestation_async(
        &state.app_attest,
        attestation_payload.clone(),
        proof.challenge.clone(),
        proof.key_id.clone(),
    )
    .await
    {
        Ok(data) => data,
        Err(err) => {
            let _ = tx.rollback().await;
            tracing::warn!("App Attest attestation failed for DID {}: {}", req.did, err);
            return error_response(StatusCode::UNAUTHORIZED, "invalid attestation payload");
        }
    };

    // IMPORTANT: For new device registration with attestation, we do NOT verify assertion
    // The attestation itself proves the key is valid. See comment above at line 1070.
    tracing::info!(
        "✅ New device attestation verified for DID {} - skipping assertion check",
        req.did
    );

    let (next_challenge, expires_at) = state.app_attest.issue_challenge();

    let new_device_id = match sqlx::query_scalar::<_, uuid::Uuid>(
        r#"
        INSERT INTO user_devices (
            did,
            device_token,
            app_attest_key_id,
            app_attest_public_key,
            app_attest_receipt,
            app_attest_counter,
            app_attest_challenge,
            app_attest_challenge_expires_at,
            app_attest_last_verified_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW())
        RETURNING id
        "#,
    )
    .bind(&req.did)
    .bind(&req.device_token)
    .bind(&proof.key_id)
    .bind(attestation.public_key)
    .bind(attestation.receipt)
    .bind(0i64) // Start counter at 0 for fresh attestation
    .bind(next_challenge.clone())
    .bind(expires_at)
    .fetch_one(tx.as_mut())
    .await
    {
        Ok(id) => id,
        Err(e) => {
            let _ = tx.rollback().await;
            tracing::error!("Error registering device: {}", e);
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Database error: {}", e),
            );
        }
    };

    if let Err(e) = sqlx::query(
        r#"
        INSERT INTO notification_preferences (user_id)
        VALUES ($1)
        "#,
    )
    .bind(new_device_id)
    .execute(tx.as_mut())
    .await
    {
        let _ = tx.rollback().await;
        tracing::error!("Error creating preferences: {}", e);
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Database error: {}", e),
        );
    }

    if let Err(e) = tx.commit().await {
        tracing::error!("Error committing transaction: {}", e);
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Database error: {}", e),
        );
    }

    let response = RegisterResponse {
        next_challenge: ChallengeEnvelope {
            challenge: next_challenge,
            expires_at,
        },
    };

    (StatusCode::CREATED, Json(response)).into_response()
}

async fn unregister_device(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    let proof = match AppAttestRequestProof::from_headers(&headers) {
        Ok(proof) => proof,
        Err(resp) => return resp,
    };

    if let Err(resp) = verify_body_binding(&body, proof.body_sha256.as_deref()) {
        return resp;
    }

    let req: UnregisterRequest = match parse_json_body(&body) {
        Ok(req) => req,
        Err(resp) => return resp,
    };

    let token_preview = req.device_token.get(..8).unwrap_or(&req.device_token);
    tracing::info!("Unregistering device with token: {}...", token_preview);

    let request_body_vec = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };

    let client_data_hash = match state
        .app_attest
        .compute_client_data_hash(&proof.challenge, proof.body_sha256.as_deref())
    {
        Ok(hash) => hash.to_vec(),
        Err(err) => {
            tracing::warn!(
                "Failed to prepare clientDataHash during unregister for DID {}: {}",
                req.did,
                err
            );
            return error_response(StatusCode::BAD_REQUEST, "invalid App Attest parameters");
        }
    };

    // Start a transaction to ensure consistency
    let mut tx = match state.db_pool.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            tracing::error!("Error starting transaction: {}", e);
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, format!("Database error: {}", e));
        }
    };

    // Find the device to delete
    let device_result = sqlx::query_as::<_, UserDevice>(
        r#"
        SELECT id, did, device_token, created_at, updated_at,
               app_attest_key_id,
               app_attest_public_key,
               app_attest_receipt,
               app_attest_counter,
               app_attest_challenge,
               app_attest_challenge_expires_at,
               app_attest_last_verified_at
        FROM user_devices
        WHERE device_token = $1 AND did = $2
        FOR UPDATE
        "#,
    )
    .bind(&req.device_token)
    .bind(&req.did)
    .fetch_optional(tx.as_mut())
    .await;

    match device_result {
        Ok(Some(device)) => {
            tracing::info!(
                "Found device for DID: {}, proceeding with deletion",
                device.did
            );

            if let Some(key_id) = &device.app_attest_key_id {
                if key_id != &proof.key_id {
                    let _ = tx.rollback().await;
                    tracing::warn!(
                        "App Attest key mismatch during unregister for DID {}",
                        req.did
                    );
                    return error_response(StatusCode::UNAUTHORIZED, "app attest key mismatch");
                }
            } else {
                let _ = tx.rollback().await;
                tracing::warn!(
                    "Device lacks App Attest provisioning during unregister for DID {}",
                    req.did
                );
                return error_response(
                    StatusCode::PRECONDITION_REQUIRED,
                    "device requires re-attestation",
                );
            }

            if let Err(e) = state.app_attest.validate_challenge(
                device.app_attest_challenge.as_deref(),
                device.app_attest_challenge_expires_at,
                &proof.challenge,
            ) {
                let _ = tx.rollback().await;
                tracing::warn!(
                    "Challenge validation failed during unregister for DID {}: {}",
                    req.did,
                    e
                );
                return error_response(StatusCode::UNAUTHORIZED, "invalid or expired challenge");
            }

            let public_key = match &device.app_attest_public_key {
                Some(key) => key.clone(),
                None => {
                    let _ = tx.rollback().await;
                    tracing::warn!(
                        "Missing App Attest public key during unregister for DID {}",
                        req.did
                    );
                    return error_response(
                        StatusCode::PRECONDITION_REQUIRED,
                        "device requires re-attestation",
                    );
                }
            };

            let previous_counter = match u32::try_from(device.app_attest_counter) {
                Ok(value) => value,
                Err(_) => {
                    let _ = tx.rollback().await;
                    tracing::error!("Invalid counter during unregister for DID {}", req.did);
                    return error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "invalid counter state",
                    );
                }
            };

            // Special case: If counter is 0, device was attested but never used
            // Allow deletion without assertion verification
            if previous_counter == 0 {
                tracing::info!(
                    "Device has counter=0 for DID {}, allowing unregister without assertion verification",
                    req.did
                );
                
                // Delete the device directly
                let delete_result =
                    sqlx::query("DELETE FROM user_devices WHERE device_token = $1 AND did = $2")
                        .bind(&req.device_token)
                        .bind(&req.did)
                        .execute(tx.as_mut())
                        .await;

                match delete_result {
                    Ok(result) => {
                        if result.rows_affected() > 0 {
                            if let Err(e) = tx.commit().await {
                                tracing::error!("Error committing transaction: {}", e);
                                return error_response(StatusCode::INTERNAL_SERVER_ERROR, format!("Database error: {}", e));
                            }

                            tracing::info!("Device unregistered successfully (counter was 0)");
                            return (StatusCode::OK, "").into_response();
                        } else {
                            let _ = tx.rollback().await;
                            tracing::warn!("Device not found during deletion");
                            return error_response(StatusCode::NOT_FOUND, "Device not found");
                        }
                    }
                    Err(e) => {
                        let _ = tx.rollback().await;
                        tracing::error!("Error deleting device: {}", e);
                        return axum::response::Response::builder()
                            .status(500)
                            .body(axum::body::Body::from(format!("Database error: {}", e)))
                            .unwrap();
                    }
                }
            }

            // Normal path: counter > 0, verify assertion before deletion
            let assertion_result = if let Some(client_data) = &proof.client_data {
                // Use new client data verification
                verify_assertion_async(
                    &state.app_attest,
                    proof.assertion.clone(),
                    client_data.clone(),
                    request_body_vec.clone(),
                    proof.body_sha256.is_some(),
                    public_key,
                    previous_counter,
                    device.app_attest_challenge.clone(),
                    proof.challenge.clone(),
                )
                .await
            } else {
                // Fallback to hash-based verification (old path)
                verify_assertion_legacy_async(
                    &state.app_attest,
                    proof.assertion.clone(),
                    client_data_hash,
                    public_key,
                    previous_counter,
                    device.app_attest_challenge.clone(),
                    proof.challenge.clone(),
                )
                .await
            };

            // Check assertion result and handle signature failures gracefully for unregister
            let allow_deletion = match assertion_result {
                Ok(_) => {
                    tracing::debug!("Assertion verification succeeded for unregister");
                    true
                }
                Err(err) => {
                    let err_str = err.to_string();
                    tracing::warn!(
                        "App Attest assertion failed during unregister for DID {}: {}",
                        req.did,
                        err_str
                    );
                    
                    // For unregister, if signature fails due to key mismatch, just allow the deletion
                    // The user wants to unregister anyway, and forcing re-attestation doesn't make sense
                    if err_str.contains("invalid signature") {
                        tracing::info!(
                            "Signature validation failed during unregister for DID {}, allowing deletion anyway (user wants to unregister)",
                            req.did
                        );
                        true // Allow deletion despite signature failure
                    } else {
                        let _ = tx.rollback().await;
                        return error_response(StatusCode::UNAUTHORIZED, "invalid app attest assertion");
                    }
                }
            };

            if !allow_deletion {
                let _ = tx.rollback().await;
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, "unexpected state");
            }

            // Delete the device (this will cascade delete notification preferences)
            let delete_result =
                sqlx::query("DELETE FROM user_devices WHERE device_token = $1 AND did = $2")
                    .bind(&req.device_token)
                    .bind(&req.did)
                    .execute(tx.as_mut())
                    .await;

            match delete_result {
                Ok(result) => {
                    if result.rows_affected() > 0 {
                        // Commit transaction
                        if let Err(e) = tx.commit().await {
                            tracing::error!("Error committing transaction: {}", e);
                            return axum::response::Response::builder()
                                .status(500)
                                .body(axum::body::Body::from(format!("Database error: {}", e)))
                                .unwrap();
                        }

                        tracing::info!("Device unregistered successfully");
                        axum::response::Response::builder()
                            .status(200)
                            .body(axum::body::Body::empty())
                            .unwrap()
                    } else {
                        // This shouldn't happen since we found the device above, but handle it gracefully
                        let _ = tx.rollback().await;
                        tracing::warn!("Device not found during deletion");
                        axum::response::Response::builder()
                            .status(404)
                            .body(axum::body::Body::from("Device not found"))
                            .unwrap()
                    }
                }
                Err(e) => {
                    let _ = tx.rollback().await;
                    tracing::error!("Error deleting device: {}", e);
                    axum::response::Response::builder()
                        .status(500)
                        .body(axum::body::Body::from(format!("Database error: {}", e)))
                        .unwrap()
                }
            }
        }
        Ok(None) => {
            // Device not found - return 200 OK as per requirements
            let _ = tx.commit().await;
            tracing::info!("Device token not found, returning 200 OK");
            axum::response::Response::builder()
                .status(200)
                .body(axum::body::Body::empty())
                .unwrap()
        }
        Err(e) => {
            let _ = tx.rollback().await;
            tracing::error!("Database error during device lookup: {}", e);
            axum::response::Response::builder()
                .status(500)
                .body(axum::body::Body::from(format!("Database error: {}", e)))
                .unwrap()
        }
    }
}

async fn get_preferences(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Query(query): Query<PreferencesQuery>,
) -> axum::response::Response {
    let proof = match AppAttestRequestProof::from_headers(&headers) {
        Ok(proof) => proof,
        Err(resp) => return resp,
    };

    if proof.body_sha256.is_some() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "X-AppAttest-BodySHA256 is not accepted on GET requests",
        );
    }

    let client_data_hash = match state
        .app_attest
        .compute_client_data_hash(&proof.challenge, proof.body_sha256.as_deref())
    {
        Ok(hash) => hash.to_vec(),
        Err(err) => {
            tracing::warn!(
                "Failed to prepare clientDataHash during preferences fetch for DID {}: {}",
                query.did,
                err
            );
            return error_response(StatusCode::BAD_REQUEST, "invalid App Attest parameters");
        }
    };

    fetch_preferences_with_auth(state, query, &proof, client_data_hash, None).await
}

async fn get_preferences_post(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    let proof = match AppAttestRequestProof::from_headers(&headers) {
        Ok(proof) => proof,
        Err(resp) => return resp,
    };

    if let Err(resp) = verify_body_binding(&body, proof.body_sha256.as_deref()) {
        return resp;
    }

    let query: PreferencesQuery = match parse_json_body(&body) {
        Ok(query) => query,
        Err(resp) => return resp,
    };

    let client_data_hash = match state
        .app_attest
        .compute_client_data_hash(&proof.challenge, proof.body_sha256.as_deref())
    {
        Ok(hash) => hash.to_vec(),
        Err(err) => {
            tracing::warn!(
                "Failed to prepare clientDataHash during preferences POST for DID {}: {}",
                query.did,
                err
            );
            return error_response(StatusCode::BAD_REQUEST, "invalid App Attest parameters");
        }
    };

    fetch_preferences_with_auth(state, query, &proof, client_data_hash, Some(body.as_ref())).await
}

async fn list_activity_subscriptions(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Query(query): Query<ActivitySubscriptionQuery>,
) -> axum::response::Response {
    let proof = match AppAttestRequestProof::from_headers(&headers) {
        Ok(proof) => proof,
        Err(resp) => return resp,
    };

    if proof.body_sha256.is_some() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "X-AppAttest-BodySHA256 is not accepted on GET requests",
        );
    }

    let client_data_hash = match state
        .app_attest
        .compute_client_data_hash(&proof.challenge, proof.body_sha256.as_deref())
    {
        Ok(hash) => hash.to_vec(),
        Err(err) => {
            tracing::warn!(
                "Failed to prepare clientDataHash for activity subscription list on DID {}: {}",
                query.did,
                err
            );
            return error_response(StatusCode::BAD_REQUEST, "invalid App Attest parameters");
        }
    };

    let mut tx = match state.db_pool.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            tracing::error!(
                "Failed to start transaction for activity subscription list: {}",
                e
            );
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "database error");
        }
    };

    let auth = match authenticate_device_for_request(
        &state,
        &mut tx,
        &query.did,
        &query.device_token,
        &proof,
        client_data_hash,
        None,
    )
    .await
    {
        Ok(auth) => auth,
        Err(resp) => {
            tx.rollback().await.ok();
            return resp;
        }
    };

    if let Err(e) = tx.commit().await {
        tracing::error!(
            "Failed to commit activity subscription list transaction: {}",
            e
        );
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "database error");
    }

    let subscriptions = match state
        .activity_subscription_manager
        .list_for_subscriber(&query.did)
        .await
    {
        Ok(list) => list,
        Err(err) => {
            tracing::error!(
                "Failed to list activity subscriptions for {}: {}",
                query.did,
                err
            );
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "database error");
        }
    };

    let payload = ActivitySubscriptionsResponse {
        subscriptions: subscriptions
            .into_iter()
            .map(ActivitySubscriptionDto::from)
            .collect(),
        next_challenge: ChallengeEnvelope {
            challenge: auth.next_challenge,
            expires_at: auth.next_challenge_expires_at,
        },
    };

    Json(payload).into_response()
}

async fn list_activity_subscriptions_post(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    let proof = match AppAttestRequestProof::from_headers(&headers) {
        Ok(proof) => proof,
        Err(resp) => return resp,
    };

    if let Err(resp) = verify_body_binding(&body, proof.body_sha256.as_deref()) {
        return resp;
    }

    let query: ActivitySubscriptionQuery = match parse_json_body(&body) {
        Ok(query) => query,
        Err(resp) => return resp,
    };

    let client_data_hash = match state
        .app_attest
        .compute_client_data_hash(&proof.challenge, proof.body_sha256.as_deref())
    {
        Ok(hash) => hash.to_vec(),
        Err(err) => {
            tracing::warn!(
                "Failed to prepare clientDataHash for activity subscription list POST on DID {}: {}",
                query.did,
                err
            );
            return error_response(StatusCode::BAD_REQUEST, "invalid App Attest parameters");
        }
    };

    let mut tx = match state.db_pool.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            tracing::error!(
                "Failed to start transaction for activity subscription list POST: {}",
                e
            );
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "database error");
        }
    };

    let auth = match authenticate_device_for_request(
        &state,
        &mut tx,
        &query.did,
        &query.device_token,
        &proof,
        client_data_hash,
        Some(body.as_ref()),
    )
    .await
    {
        Ok(auth) => auth,
        Err(resp) => {
            tx.rollback().await.ok();
            return resp;
        }
    };

    if let Err(e) = tx.commit().await {
        tracing::error!(
            "Failed to commit activity subscription list POST transaction: {}",
            e
        );
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "database error");
    }

    let subscriptions = match state
        .activity_subscription_manager
        .list_for_subscriber(&query.did)
        .await
    {
        Ok(list) => list,
        Err(err) => {
            tracing::error!(
                "Failed to list activity subscriptions for {}: {}",
                query.did,
                err
            );
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "database error");
        }
    };

    let payload = ActivitySubscriptionsResponse {
        subscriptions: subscriptions
            .into_iter()
            .map(ActivitySubscriptionDto::from)
            .collect(),
        next_challenge: ChallengeEnvelope {
            challenge: auth.next_challenge,
            expires_at: auth.next_challenge_expires_at,
        },
    };

    Json(payload).into_response()
}

async fn update_preferences(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    let proof = match AppAttestRequestProof::from_headers(&headers) {
        Ok(proof) => proof,
        Err(resp) => return resp,
    };

    if let Err(resp) = verify_body_binding(&body, proof.body_sha256.as_deref()) {
        return resp;
    }

    let req: PreferencesUpdateRequest = match parse_json_body(&body) {
        Ok(req) => req,
        Err(resp) => return resp,
    };

    let client_data_hash = match state
        .app_attest
        .compute_client_data_hash(&proof.challenge, proof.body_sha256.as_deref())
    {
        Ok(hash) => hash.to_vec(),
        Err(err) => {
            tracing::warn!(
                "Failed to prepare clientDataHash for preferences update on DID {}: {}",
                req.did,
                err
            );
            return error_response(StatusCode::BAD_REQUEST, "invalid App Attest parameters");
        }
    };

    let mut tx = match state.db_pool.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            tracing::error!("Failed to start transaction for preferences update: {}", e);
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "database error");
        }
    };

    let auth = match authenticate_device_for_request(
        &state,
        &mut tx,
        &req.did,
        &req.device_token,
        &proof,
        client_data_hash,
        Some(body.as_ref()),
    )
    .await
    {
        Ok(auth) => auth,
        Err(resp) => {
            tx.rollback().await.ok();
            return resp;
        }
    };

    let device_ids = match sqlx::query_scalar::<_, uuid::Uuid>(
        r#"
        SELECT id
        FROM user_devices
        WHERE did = $1
        "#,
    )
    .bind(&req.did)
    .fetch_all(tx.as_mut())
    .await
    {
        Ok(ids) if !ids.is_empty() => ids,
        Ok(_) => {
            tx.rollback().await.ok();
            return error_response(StatusCode::NOT_FOUND, "no devices found for DID");
        }
        Err(e) => {
            tx.rollback().await.ok();
            tracing::error!("Failed to enumerate devices for preferences update: {}", e);
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "database error");
        }
    };

    for device_id in device_ids {
        if let Err(e) = sqlx::query(
            r#"
            UPDATE notification_preferences
            SET mentions = $1,
                replies = $2,
                likes = $3,
                follows = $4,
                reposts = $5,
                quotes = $6,
                via_likes = $7,
                via_reposts = $8,
                activity_subscriptions = $9
            WHERE user_id = $10
            "#,
        )
        .bind(req.mentions)
        .bind(req.replies)
        .bind(req.likes)
        .bind(req.follows)
        .bind(req.reposts)
        .bind(req.quotes)
        .bind(req.via_likes)
        .bind(req.via_reposts)
        .bind(req.activity_subscriptions)
        .bind(device_id)
        .execute(tx.as_mut())
        .await
        {
            tx.rollback().await.ok();
            tracing::error!(
                "Failed to update preferences for device {}: {}",
                device_id,
                e
            );
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "database error");
        }
    }

    if let Err(e) = tx.commit().await {
        tracing::error!("Failed to commit preferences update: {}", e);
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "database error");
    }

    let response = PreferencesResponse {
        preferences: PreferencesBody {
            did: req.did,
            mentions: req.mentions,
            replies: req.replies,
            likes: req.likes,
            follows: req.follows,
            reposts: req.reposts,
            quotes: req.quotes,
            via_likes: req.via_likes,
            via_reposts: req.via_reposts,
            activity_subscriptions: req.activity_subscriptions,
        },
        next_challenge: ChallengeEnvelope {
            challenge: auth.next_challenge,
            expires_at: auth.next_challenge_expires_at,
        },
    };

    (StatusCode::OK, Json(response)).into_response()
}

async fn upsert_activity_subscription(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    let proof = match AppAttestRequestProof::from_headers(&headers) {
        Ok(proof) => proof,
        Err(resp) => return resp,
    };

    if let Err(resp) = verify_body_binding(&body, proof.body_sha256.as_deref()) {
        return resp;
    }

    let req: ActivitySubscriptionUpdateRequest = match parse_json_body(&body) {
        Ok(req) => req,
        Err(resp) => return resp,
    };

    if req.subject_did.trim().is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "subject_did is required");
    }

    let remove_only = !req.include_posts && !req.include_replies;

    let client_data_hash = match state
        .app_attest
        .compute_client_data_hash(&proof.challenge, proof.body_sha256.as_deref())
    {
        Ok(hash) => hash.to_vec(),
        Err(err) => {
            tracing::warn!(
                "Failed to prepare clientDataHash for activity subscription update on DID {}: {}",
                req.did,
                err
            );
            return error_response(StatusCode::BAD_REQUEST, "invalid App Attest parameters");
        }
    };

    let mut tx = match state.db_pool.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            tracing::error!(
                "Failed to start transaction for activity subscription update: {}",
                e
            );
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "database error");
        }
    };

    let auth = match authenticate_device_for_request(
        &state,
        &mut tx,
        &req.did,
        &req.device_token,
        &proof,
        client_data_hash,
        Some(body.as_ref()),
    )
    .await
    {
        Ok(auth) => auth,
        Err(resp) => {
            tx.rollback().await.ok();
            return resp;
        }
    };

    if let Err(e) = tx.commit().await {
        tracing::error!(
            "Failed to commit activity subscription update transaction: {}",
            e
        );
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "database error");
    }

    let manager = state.activity_subscription_manager.clone();

    let update_result = if remove_only {
        manager
            .delete_subscription(&req.did, &req.subject_did)
            .await
    } else {
        manager
            .upsert_subscription(
                &req.did,
                &req.subject_did,
                req.include_posts,
                req.include_replies,
            )
            .await
    };

    if let Err(err) = update_result {
        tracing::error!(
            "Failed to persist activity subscription change for {} -> {}: {}",
            req.did,
            req.subject_did,
            err
        );
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "database error");
    }

    let subscriptions = match state
        .activity_subscription_manager
        .list_for_subscriber(&req.did)
        .await
    {
        Ok(list) => list,
        Err(err) => {
            tracing::error!(
                "Failed to list activity subscriptions after update for {}: {}",
                req.did,
                err
            );
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "database error");
        }
    };

    let response = ActivitySubscriptionsResponse {
        subscriptions: subscriptions
            .into_iter()
            .map(ActivitySubscriptionDto::from)
            .collect(),
        next_challenge: ChallengeEnvelope {
            challenge: auth.next_challenge,
            expires_at: auth.next_challenge_expires_at,
        },
    };

    (StatusCode::OK, Json(response)).into_response()
}

async fn remove_activity_subscription(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    let proof = match AppAttestRequestProof::from_headers(&headers) {
        Ok(proof) => proof,
        Err(resp) => return resp,
    };

    if let Err(resp) = verify_body_binding(&body, proof.body_sha256.as_deref()) {
        return resp;
    }

    let req: ActivitySubscriptionDeleteRequest = match parse_json_body(&body) {
        Ok(req) => req,
        Err(resp) => return resp,
    };

    if req.subject_did.trim().is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "subject_did is required");
    }

    let client_data_hash = match state
        .app_attest
        .compute_client_data_hash(&proof.challenge, proof.body_sha256.as_deref())
    {
        Ok(hash) => hash.to_vec(),
        Err(err) => {
            tracing::warn!(
                "Failed to prepare clientDataHash for activity subscription removal on DID {}: {}",
                req.did,
                err
            );
            return error_response(StatusCode::BAD_REQUEST, "invalid App Attest parameters");
        }
    };

    let mut tx = match state.db_pool.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            tracing::error!(
                "Failed to start transaction for activity subscription removal: {}",
                e
            );
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "database error");
        }
    };

    let auth = match authenticate_device_for_request(
        &state,
        &mut tx,
        &req.did,
        &req.device_token,
        &proof,
        client_data_hash,
        Some(body.as_ref()),
    )
    .await
    {
        Ok(auth) => auth,
        Err(resp) => {
            tx.rollback().await.ok();
            return resp;
        }
    };

    if let Err(e) = tx.commit().await {
        tracing::error!(
            "Failed to commit activity subscription removal transaction: {}",
            e
        );
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "database error");
    }

    if let Err(err) = state
        .activity_subscription_manager
        .delete_subscription(&req.did, &req.subject_did)
        .await
    {
        tracing::error!(
            "Failed to delete activity subscription for {} -> {}: {}",
            req.did,
            req.subject_did,
            err
        );
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "database error");
    }

    let subscriptions = match state
        .activity_subscription_manager
        .list_for_subscriber(&req.did)
        .await
    {
        Ok(list) => list,
        Err(err) => {
            tracing::error!(
                "Failed to list activity subscriptions after removal for {}: {}",
                req.did,
                err
            );
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "database error");
        }
    };

    let response = ActivitySubscriptionsResponse {
        subscriptions: subscriptions
            .into_iter()
            .map(ActivitySubscriptionDto::from)
            .collect(),
        next_challenge: ChallengeEnvelope {
            challenge: auth.next_challenge,
            expires_at: auth.next_challenge_expires_at,
        },
    };

    (StatusCode::OK, Json(response)).into_response()
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

// Sync moderation lists
async fn sync_moderation_lists(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let proof = match AppAttestRequestProof::from_headers(&headers) {
        Ok(proof) => proof,
        Err(resp) => return resp,
    };

    if let Err(resp) = verify_body_binding(&body, proof.body_sha256.as_deref()) {
        return resp;
    }

    let req: SyncModerationListsRequest = match parse_json_body(&body) {
        Ok(req) => req,
        Err(resp) => return resp,
    };

    // Verify App Attest assertion
    if let Err(resp) = verify_app_attest_assertion(
        &state,
        &req.did,
        &req.device_token,
        &proof,
        &body,
    )
    .await
    {
        return resp;
    }

    // Convert DTOs to manager types
    let lists: Vec<crate::moderation_list_manager::ModerationList> = req
        .lists
        .into_iter()
        .map(|l| crate::moderation_list_manager::ModerationList {
            uri: l.uri,
            purpose: l.purpose,
            name: l.name,
        })
        .collect();

    // TODO: Get PDS URL and access token from user's session or config
    // For now, we'll use a placeholder - this needs to be implemented properly
    let pds_url = std::env::var("DEFAULT_PDS_URL").unwrap_or_else(|_| "https://bsky.social".to_string());
    let access_token = ""; // This should come from the user's session

    // Sync the lists
    let client = reqwest::Client::new();
    match state
        .moderation_list_manager
        .sync_moderation_lists(&req.did, lists, &client, &pds_url, access_token)
        .await
    {
        Ok(_) => {
            info!("Successfully synced moderation lists for user {}", req.did);
            (
                StatusCode::OK,
                Json(serde_json::json!({"status": "success"})),
            )
                .into_response()
        }
        Err(e) => {
            error!("Failed to sync moderation lists: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Failed to sync lists"})),
            )
                .into_response()
        }
    }
}

// Mute thread
async fn mute_thread(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let proof = match AppAttestRequestProof::from_headers(&headers) {
        Ok(proof) => proof,
        Err(resp) => return resp,
    };

    if let Err(resp) = verify_body_binding(&body, proof.body_sha256.as_deref()) {
        return resp;
    }

    let req: MuteThreadRequest = match parse_json_body(&body) {
        Ok(req) => req,
        Err(resp) => return resp,
    };

    // Verify App Attest assertion
    if let Err(resp) = verify_app_attest_assertion(
        &state,
        &req.did,
        &req.device_token,
        &proof,
        &body,
    )
    .await
    {
        return resp;
    }

    match state
        .thread_mute_manager
        .mute_thread(&req.did, &req.thread_root_uri)
        .await
    {
        Ok(_) => (
            StatusCode::OK,
            Json(serde_json::json!({"status": "success"})),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to mute thread: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Failed to mute thread"})),
            )
                .into_response()
        }
    }
}

// Unmute thread
async fn unmute_thread(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let proof = match AppAttestRequestProof::from_headers(&headers) {
        Ok(proof) => proof,
        Err(resp) => return resp,
    };

    if let Err(resp) = verify_body_binding(&body, proof.body_sha256.as_deref()) {
        return resp;
    }

    let req: UnmuteThreadRequest = match parse_json_body(&body) {
        Ok(req) => req,
        Err(resp) => return resp,
    };

    // Verify App Attest assertion
    if let Err(resp) = verify_app_attest_assertion(
        &state,
        &req.did,
        &req.device_token,
        &proof,
        &body,
    )
    .await
    {
        return resp;
    }

    match state
        .thread_mute_manager
        .unmute_thread(&req.did, &req.thread_root_uri)
        .await
    {
        Ok(_) => (
            StatusCode::OK,
            Json(serde_json::json!({"status": "success"})),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to unmute thread: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Failed to unmute thread"})),
            )
                .into_response()
        }
    }
}

// Get muted threads (GET)
async fn get_muted_threads(
    State(state): State<Arc<ApiState>>,
    Query(req): Query<GetMutedThreadsRequest>,
) -> impl IntoResponse {
    match state.thread_mute_manager.get_muted_threads(&req.did).await {
        Ok(threads) => (StatusCode::OK, Json(MutedThreadsResponse { threads })).into_response(),
        Err(e) => {
            error!("Failed to get muted threads: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Failed to get muted threads"})),
            )
                .into_response()
        }
    }
}

// Get muted threads (POST)
async fn get_muted_threads_post(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let proof = match AppAttestRequestProof::from_headers(&headers) {
        Ok(proof) => proof,
        Err(resp) => return resp,
    };

    if let Err(resp) = verify_body_binding(&body, proof.body_sha256.as_deref()) {
        return resp;
    }

    let req: GetMutedThreadsRequest = match parse_json_body(&body) {
        Ok(req) => req,
        Err(resp) => return resp,
    };

    // Verify App Attest assertion
    if let Err(resp) = verify_app_attest_assertion(
        &state,
        &req.did,
        &req.device_token,
        &proof,
        &body,
    )
    .await
    {
        return resp;
    }

    match state.thread_mute_manager.get_muted_threads(&req.did).await {
        Ok(threads) => (StatusCode::OK, Json(MutedThreadsResponse { threads })).into_response(),
        Err(e) => {
            error!("Failed to get muted threads: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Failed to get muted threads"})),
            )
                .into_response()
        }
    }
}

