use axum::extract::State;
use axum::http::header::AUTHORIZATION;
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::error::{AppError, AppResult};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/status", get(status))
        .route("/api/v1/broker", get(broker_probe))
        .route("/api/v1/tenants", get(tenants))
        .route("/api/v1/audit", get(audit))
}

async fn healthz() -> &'static str {
    "ok"
}

async fn status(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Json<Value>> {
    require_token(&state, &headers)?;
    Ok(Json(json!({
        "product": "HoneNode",
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": state.uptime_seconds(),
        "registry": state.config.registry_path.display().to_string(),
        "broker_socket": state.broker.socket().display().to_string(),
        "tenants": state.config.tenants,
        "surfaces": {
            "operator": "loopback only, reached by SSH tunnel",
            "tenant_portal": "public, behind edge -> WAF -> app",
        },
    })))
}

async fn broker_probe(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Json<Value>> {
    require_token(&state, &headers)?;
    Ok(
        match state.broker.call(&crate::broker::Request::Status).await {
            Ok(response) => Json(json!({ "ok": response.ok, "broker": response })),
            Err(_) => Json(json!({ "ok": false, "error": "broker unavailable" })),
        },
    )
}

async fn tenants(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Json<Value>> {
    require_token(&state, &headers)?;
    let tenants = crate::registry::tenants(&state.db).await?;
    let services = crate::registry::all_services(&state.db).await?;
    Ok(Json(json!({ "tenants": tenants, "services": services })))
}

async fn audit(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Json<Value>> {
    require_token(&state, &headers)?;
    let events = crate::registry::recent_audit(&state.db, 500).await?;
    Ok(Json(json!({ "events": events })))
}

fn require_token(state: &AppState, headers: &HeaderMap) -> AppResult<()> {
    if state.config.api_tokens.is_empty() {
        return Err(AppError::Forbidden);
    }

    let presented = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .ok_or(AppError::Unauthorized)?;

    let accepted = state
        .config
        .api_tokens
        .iter()
        .any(|token| crate::auth::constant_time_eq(token.as_bytes(), presented.as_bytes()));

    if accepted {
        Ok(())
    } else {
        Err(AppError::Unauthorized)
    }
}
