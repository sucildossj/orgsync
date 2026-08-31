//! Enrolment and administration over plain HTTP JSON.
//!
//! Deliberately not part of the libp2p protocol: a phone that has not joined
//! yet has no certificate, so it has nothing to authenticate a p2p connection
//! with. Keeping enrolment on HTTP also means the mobile app can do the call
//! with its own `fetch` and the Rust binary carries no TLS stack.
//!
//! Run this behind a TLS-terminating reverse proxy in production.

use std::sync::Arc;

use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use p2p_core::enroll::{EnrollRequest, EnrollResponse, OrgInfo};
use p2p_core::identity::Role;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tower_http::cors::CorsLayer;

use crate::store::Store;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    /// Dialable multiaddrs of this server, each ending in `/p2p/<peer id>`.
    /// Seeded from `--announce` and extended as the node learns which
    /// addresses it actually ended up listening on.
    pub bootstrap: Arc<std::sync::RwLock<Vec<String>>>,
    pub cert_ttl_ms: u64,
}

impl AppState {
    fn bootstrap_addrs(&self) -> Vec<String> {
        self.bootstrap.read().map(|b| b.clone()).unwrap_or_default()
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/org", get(org_info))
        .route("/v1/enroll", post(enroll))
        .route("/v1/crl", get(crl))
        .route("/v1/admin/invites", post(create_invite).get(list_invites))
        .route("/v1/admin/devices", get(list_devices))
        .route("/v1/admin/revoke", post(revoke))
        // The admin routes are bearer-token gated, so a permissive policy here
        // lets an internal dashboard call them without weakening that gate.
        .layer(CorsLayer::permissive())
        .with_state(state)
}

// ------------------------------------------------------------------- errors

pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self { status: StatusCode::BAD_REQUEST, message: message.into() }
    }
    fn unauthorized() -> Self {
        Self { status: StatusCode::UNAUTHORIZED, message: "admin token required".into() }
    }
    fn internal(message: impl Into<String>) -> Self {
        Self { status: StatusCode::INTERNAL_SERVER_ERROR, message: message.into() }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::internal(e.to_string())
    }
}

type ApiResult<T> = std::result::Result<T, ApiError>;

/// Compares in constant time so a wrong token leaks nothing through timing.
fn token_matches(given: &str, expected: &str) -> bool {
    let (a, b) = (given.as_bytes(), expected.as_bytes());
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(1);
    }
    diff == 0
}

fn require_admin(state: &AppState, headers: &HeaderMap) -> ApiResult<()> {
    let expected = state.store.admin_token().map_err(ApiError::from)?;
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if token_matches(given, &expected) {
        Ok(())
    } else {
        Err(ApiError::unauthorized())
    }
}

// ----------------------------------------------------------------- handlers

async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

async fn org_info(State(state): State<AppState>) -> ApiResult<Json<OrgInfo>> {
    let org = state.store.org_keypair()?;
    Ok(Json(OrgInfo {
        org_id: org.org_id(),
        org_pub: hex::encode(org.public_bytes()),
        name: state.store.org_name()?,
        bootstrap: state.bootstrap_addrs(),
    }))
}

async fn crl(State(state): State<AppState>) -> ApiResult<Json<p2p_core::RevocationList>> {
    Ok(Json(state.store.crl()?))
}

async fn enroll(
    State(state): State<AppState>,
    Json(req): Json<EnrollRequest>,
) -> ApiResult<Json<EnrollResponse>> {
    let cert = state
        .store
        .redeem_invite(
            &req.invite_token,
            &req.device_pub,
            &req.device_name,
            &req.platform,
            req.at_ms,
            &req.proof,
            state.cert_ttl_ms,
        )
        // These are all things the caller can fix, so say what went wrong.
        .map_err(|e| ApiError::bad_request(e.to_string()))?;

    let org = state.store.org_keypair()?;
    tracing::info!(
        peer = %cert.peer_id().map(|p| p.to_string()).unwrap_or_default(),
        user = %cert.claims.user_id,
        serial = cert.claims.serial,
        "enrolled a device"
    );

    Ok(Json(EnrollResponse {
        cert,
        crl: state.store.crl()?,
        org: OrgInfo {
            org_id: org.org_id(),
            org_pub: hex::encode(org.public_bytes()),
            name: state.store.org_name()?,
            bootstrap: state.bootstrap_addrs(),
        },
    }))
}

#[derive(Debug, Deserialize)]
struct CreateInvite {
    user_id: String,
    #[serde(default)]
    display_name: String,
    #[serde(default = "default_role")]
    role: String,
    #[serde(default = "default_ttl_hours")]
    ttl_hours: u64,
}

fn default_role() -> String {
    "member".into()
}
fn default_ttl_hours() -> u64 {
    24
}

#[derive(Debug, Serialize)]
struct InviteCreated {
    invite_code: String,
    user_id: String,
    role: String,
    expires_at_ms: u64,
}

async fn create_invite(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateInvite>,
) -> ApiResult<Json<InviteCreated>> {
    require_admin(&state, &headers)?;
    if body.user_id.trim().is_empty() {
        return Err(ApiError::bad_request("user_id is required"));
    }
    let display = if body.display_name.is_empty() { body.user_id.clone() } else { body.display_name };
    let row = state.store.create_invite(
        &body.user_id,
        &display,
        Role::from_str_lossy(&body.role),
        body.ttl_hours.saturating_mul(3_600_000),
    )?;
    Ok(Json(InviteCreated {
        invite_code: row.token,
        user_id: row.user_id,
        role: row.role,
        expires_at_ms: row.expires_at_ms,
    }))
}

async fn list_invites(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&state, &headers)?;
    Ok(Json(json!({ "invites": state.store.list_invites()? })))
}

async fn list_devices(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&state, &headers)?;
    Ok(Json(json!({ "devices": state.store.list_devices()? })))
}

#[derive(Debug, Deserialize)]
struct RevokeBody {
    serial: u64,
}

async fn revoke(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<RevokeBody>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&state, &headers)?;
    state.store.revoke(body.serial).map_err(|e| ApiError::bad_request(e.to_string()))?;
    let crl = state.store.crl()?;
    tracing::warn!(serial = body.serial, "device revoked");
    Ok(Json(json!({ "revoked": body.serial, "crl": crl })))
}
