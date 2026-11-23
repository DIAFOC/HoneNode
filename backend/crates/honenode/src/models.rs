use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(rename_all = "lowercase")]
pub enum Role {
    Operator,
    Tenant,
    Support,
}

impl Role {
    pub fn label(self) -> &'static str {
        match self {
            Role::Operator => "Operador",
            Role::Tenant => "Inquilino",
            Role::Support => "Soporte (solo lectura)",
        }
    }

    pub fn is_operator(self) -> bool {
        matches!(self, Role::Operator)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(rename_all = "lowercase")]
pub enum ServiceKind {
    Web,
    Database,
    Storage,
    Git,
    Container,
}

impl ServiceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ServiceKind::Web => "web",
            ServiceKind::Database => "database",
            ServiceKind::Storage => "storage",
            ServiceKind::Git => "git",
            ServiceKind::Container => "container",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "web" => Some(ServiceKind::Web),
            "database" => Some(ServiceKind::Database),
            "storage" => Some(ServiceKind::Storage),
            "git" => Some(ServiceKind::Git),
            "container" => Some(ServiceKind::Container),
            _ => None,
        }
    }

    pub fn all() -> [ServiceKind; 5] {
        [
            ServiceKind::Web,
            ServiceKind::Database,
            ServiceKind::Storage,
            ServiceKind::Git,
            ServiceKind::Container,
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(rename_all = "lowercase")]
pub enum ServiceState {
    Pending,
    Running,
    Stopped,
    Suspended,
    Deleted,
    Failed,
}

impl ServiceState {
    pub fn as_str(self) -> &'static str {
        match self {
            ServiceState::Pending => "pending",
            ServiceState::Running => "running",
            ServiceState::Stopped => "stopped",
            ServiceState::Suspended => "suspended",
            ServiceState::Deleted => "deleted",
            ServiceState::Failed => "failed",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ServiceState::Pending => "Pendiente de observación",
            ServiceState::Running => "Ejecución solicitada",
            ServiceState::Stopped => "Detención solicitada",
            ServiceState::Suspended => "Suspensión solicitada",
            ServiceState::Deleted => "Eliminado",
            ServiceState::Failed => "Fallo",
        }
    }

    pub fn is_live(self) -> bool {
        matches!(self, ServiceState::Running | ServiceState::Pending)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(rename_all = "lowercase")]
pub enum TenantStatus {
    Active,
    Grace,
    Suspended,
    Purged,
}

impl TenantStatus {
    pub fn label(self) -> &'static str {
        match self {
            TenantStatus::Active => "Activo",
            TenantStatus::Grace => "En gracia",
            TenantStatus::Suspended => "Suspendido",
            TenantStatus::Purged => "Purgado",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(rename_all = "lowercase")]
pub enum JobState {
    Queued,
    Running,
    Done,
    Failed,
}

impl JobState {
    pub fn label(self) -> &'static str {
        match self {
            JobState::Queued => "En cola",
            JobState::Running => "Ejecutando",
            JobState::Done => "Completado",
            JobState::Failed => "Fallido",
        }
    }
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Account {
    pub id: i64,
    pub number: String,
    pub role: Role,
    #[serde(skip_serializing)]
    pub password_hash: String,
    pub created_at: OffsetDateTime,
    pub last_login_at: Option<OffsetDateTime>,
    pub disabled_at: Option<OffsetDateTime>,
    pub notes: Option<String>,
}

impl Account {
    pub fn is_enabled(&self) -> bool {
        self.disabled_at.is_none()
    }
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Tenant {
    pub id: i64,
    pub account_id: i64,
    pub slug: String,
    pub name: String,
    pub status: TenantStatus,
    pub created_at: OffsetDateTime,
    pub suspended_at: Option<OffsetDateTime>,
    pub purge_after: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Plan {
    pub id: i64,
    pub code: String,
    pub monthly_price: f64,
    pub cpu_weight: i64,
    pub memory_high: String,
    pub memory_max: String,
    pub tasks_max: i64,
    pub disk_gb: i64,
    pub bandwidth_gb: i64,
    pub max_services: i64,
    pub description: String,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Service {
    pub id: i64,
    pub tenant_id: i64,
    pub kind: ServiceKind,
    pub plan_id: i64,
    pub state: ServiceState,
    pub observed_state: Option<String>,
    pub observed_at: Option<OffsetDateTime>,
    pub unit_name: String,
    pub uid: Option<i64>,
    pub ports_json: String,
    pub volume_json: String,
    pub created_at: OffsetDateTime,
    pub expires_at: Option<OffsetDateTime>,
    pub suspended_at: Option<OffsetDateTime>,
    pub deleted_at: Option<OffsetDateTime>,
}

impl Service {
    pub fn tenant_actions_allowed(
        &self,
        tenant: &Tenant,
        grace_days: i64,
        now: OffsetDateTime,
    ) -> bool {
        self.tenant_id == tenant.id
            && matches!(tenant.status, TenantStatus::Active | TenantStatus::Grace)
            && self.deleted_at.is_none()
            && matches!(self.state, ServiceState::Running | ServiceState::Stopped)
            && self
                .expires_at
                .map(|expiry| now <= expiry + time::Duration::days(grace_days))
                .unwrap_or(true)
    }
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct ServiceView {
    pub id: i64,
    pub tenant_id: i64,
    pub tenant_slug: String,
    pub tenant_name: String,
    pub kind: ServiceKind,
    pub plan_id: i64,
    pub plan_code: String,
    pub state: ServiceState,
    pub observed_state: Option<String>,
    pub observed_at: Option<OffsetDateTime>,
    pub unit_name: String,
    pub ports_json: String,
    pub created_at: OffsetDateTime,
    pub expires_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Job {
    pub id: i64,
    pub tenant_id: Option<i64>,
    pub action: String,
    pub payload_json: String,
    pub state: JobState,
    pub requested_by: Option<i64>,
    pub started_at: Option<OffsetDateTime>,
    pub finished_at: Option<OffsetDateTime>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct AuditEvent {
    pub id: i64,
    pub ts: OffsetDateTime,
    pub request_id: String,
    pub tenant_id: Option<i64>,
    pub actor_type: String,
    pub actor_id: Option<String>,
    pub action: String,
    pub resource_type: Option<String>,
    pub resource_id: Option<String>,
    pub outcome: String,
    pub reason: Option<String>,
    pub src_ip: Option<String>,
    pub user_agent: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
pub struct Session {
    pub id: String,
    pub account_id: i64,
    pub csrf_token: String,
    pub created_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
    pub last_seen_at: OffsetDateTime,
    pub src_ip: Option<String>,
    pub user_agent: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Principal {
    pub account: Account,
    pub session: Session,
    pub tenant: Option<Tenant>,
}

impl Principal {
    pub fn role(&self) -> Role {
        self.account.role
    }

    pub fn tenant_id(&self) -> Option<i64> {
        self.tenant.as_ref().map(|tenant| tenant.id)
    }

    pub fn require_operator(&self) -> Result<(), crate::error::AppError> {
        if self.account.role.is_operator() {
            Ok(())
        } else {
            Err(crate::error::AppError::Forbidden)
        }
    }

    pub fn require_tenant(&self) -> Result<i64, crate::error::AppError> {
        if self.account.role != Role::Tenant || !self.account.is_enabled() {
            return Err(crate::error::AppError::Forbidden);
        }
        self.tenant_id().ok_or(crate::error::AppError::Forbidden)
    }
}
