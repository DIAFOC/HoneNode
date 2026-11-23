use axum::http::HeaderMap;
use sqlx::SqlitePool;

use crate::error::AppResult;
use crate::registry::{self, NewAuditEvent};

#[derive(Debug, Clone)]
pub struct Context {
    pub request_id: String,
    pub src_ip: Option<String>,
    pub user_agent: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct Event<'a> {
    pub tenant_id: Option<i64>,
    pub actor_type: &'a str,
    pub actor_id: Option<&'a str>,
    pub action: &'a str,
    pub resource_type: Option<&'a str>,
    pub resource_id: Option<&'a str>,
    pub outcome: &'a str,
    pub reason: Option<&'a str>,
}

impl Context {
    pub fn from_headers(headers: &HeaderMap) -> Self {
        Self {
            request_id: crate::auth::generate_token().unwrap_or_else(|_| "unavailable".to_string()),
            src_ip: header(headers, "x-forwarded-for")
                .or_else(|| header(headers, "x-real-ip"))
                .map(|value| value.split(',').next().unwrap_or("").trim().to_string())
                .filter(|value| !value.is_empty()),
            user_agent: header(headers, "user-agent"),
        }
    }

    pub fn broker_context(
        &self,
        principal: &crate::models::Principal,
    ) -> AppResult<crate::broker::CallContext> {
        if self.request_id == "unavailable" {
            return Err(crate::error::AppError::Internal(anyhow::anyhow!(
                "audit request ID unavailable"
            )));
        }
        crate::broker::CallContext::for_principal(&self.request_id, principal)
    }

    pub async fn record(&self, pool: &SqlitePool, event: Event<'_>) -> AppResult<()> {
        registry::record_audit(
            pool,
            NewAuditEvent {
                request_id: &self.request_id,
                tenant_id: event.tenant_id,
                actor_type: event.actor_type,
                actor_id: event.actor_id,
                action: event.action,
                resource_type: event.resource_type,
                resource_id: event.resource_id,
                outcome: event.outcome,
                reason: event.reason,
                src_ip: self.src_ip.as_deref(),
                user_agent: self.user_agent.as_deref(),
            },
        )
        .await
    }
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}
