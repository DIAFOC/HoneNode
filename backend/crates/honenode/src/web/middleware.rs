use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{FromRequestParts, Request, State};
use axum::http::header::{self, HeaderValue};
use axum::http::request::Parts;
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;

use crate::error::{AppError, AppResult};
use crate::models::Principal;
use crate::state::AppState;

pub const SESSION_COOKIE: &str = "hone_session";

const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
                   img-src 'self' data:; font-src 'self'; connect-src 'self'; \
                   form-action 'self'; frame-ancestors 'none'; base-uri 'none'; object-src 'none'";

pub async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
    headers.insert(
        header::HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static("geolocation=(), microphone=(), camera=()"),
    );
    response
}

pub async fn load_principal(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    if let Some(token) = session_token(request.headers(), state.config.secure_cookies) {
        if let Ok(Some(session)) = crate::auth::load_session(&state.db, &token).await {
            if let Ok(Some(account)) =
                crate::registry::account_by_id(&state.db, session.account_id).await
            {
                if account.is_enabled() {
                    let tenant = crate::registry::tenant_by_account(&state.db, account.id)
                        .await
                        .ok()
                        .flatten();
                    let _ = crate::auth::touch_session(&state.db, &session.id).await;
                    request.extensions_mut().insert(Principal {
                        account,
                        session,
                        tenant,
                    });
                }
            }
        }
    }
    next.run(request).await
}

pub struct Current(pub Option<Principal>);

impl<S> FromRequestParts<S> for Current
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Current(parts.extensions.get::<Principal>().cloned()))
    }
}

impl Current {
    pub fn require(self) -> AppResult<Principal> {
        self.0.ok_or(AppError::Unauthorized)
    }

    pub fn require_operator(self) -> AppResult<Principal> {
        let principal = self.require()?;
        principal.require_operator()?;
        Ok(principal)
    }
}

fn session_cookie_name(secure: bool) -> &'static str {
    if secure {
        "__Host-hone_session"
    } else {
        SESSION_COOKIE
    }
}

pub fn session_token(headers: &HeaderMap, secure: bool) -> Option<String> {
    cookie_token(headers, session_cookie_name(secure))
}

pub fn cookie_token(headers: &HeaderMap, name: &str) -> Option<String> {
    let mut found = None;
    for raw in headers.get_all(header::COOKIE) {
        for part in raw.to_str().ok()?.split(';') {
            let Some((key, value)) = part.trim().split_once('=') else {
                continue;
            };
            if key != name {
                continue;
            }
            if found.is_some() || value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return None;
            }
            found = Some(value.to_string());
        }
    }
    found
}

pub fn session_cookie(token: &str, ttl: Duration, secure: bool) -> String {
    let mut value = format!(
        "{}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        session_cookie_name(secure),
        ttl.as_secs()
    );
    if secure {
        value.push_str("; Secure");
    }
    value
}

pub fn cleared_cookie(secure: bool) -> String {
    let mut value = format!(
        "{}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0",
        session_cookie_name(secure)
    );
    if secure {
        value.push_str("; Secure");
    }
    value
}

pub fn check_csrf(principal: &Principal, presented: &str) -> AppResult<()> {
    if crate::auth::csrf_matches(&principal.session, presented) {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

pub fn client_ip(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-forwarded-for")
        .or_else(|| headers.get("x-real-ip"))
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(',').next().unwrap_or("").trim().to_string())
        .filter(|value| !value.is_empty())
}

pub fn user_agent(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}
