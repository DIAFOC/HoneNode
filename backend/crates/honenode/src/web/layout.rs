use askama::Template;
use axum::response::Html;
use time::OffsetDateTime;

use crate::billing;
use crate::catalog;
use crate::config::{Config, ServiceAccess, Theme};
use crate::error::{AppError, AppResult};
use crate::models::{
    AuditEvent, Job, JobState, Plan, Principal, ServiceKind, ServiceState, ServiceView, Tenant,
    TenantStatus,
};

pub const GOLD: &str = "tone-gold";

#[derive(Debug, Clone)]
pub struct PrincipalView {
    pub has_principal: bool,
    pub number: String,
    pub role_label: String,
    pub is_operator: bool,
    pub csrf: String,
    pub theme_class: &'static str,
    pub is_sharpnode: bool,
}

pub fn principal_view(principal: Option<&Principal>, theme: Theme) -> PrincipalView {
    // Tenant branding never applies to the operator console or roleless login.
    let theme = match principal {
        Some(principal) if !principal.role().is_operator() => theme,
        _ => Theme::Neutral,
    };
    match principal {
        Some(principal) => PrincipalView {
            has_principal: true,
            number: principal.account.number.clone(),
            role_label: principal.role().label().to_string(),
            is_operator: principal.role().is_operator(),
            csrf: principal.session.csrf_token.clone(),
            theme_class: theme.css_class(),
            is_sharpnode: theme == Theme::SharpNode,
        },
        None => PrincipalView {
            has_principal: false,
            number: String::new(),
            role_label: String::new(),
            is_operator: false,
            csrf: String::new(),
            theme_class: theme.css_class(),
            is_sharpnode: theme == Theme::SharpNode,
        },
    }
}

#[derive(Debug, Clone)]
pub struct StatView {
    pub label: String,
    pub value: String,
    pub unit: String,
}

impl StatView {
    pub fn new(
        label: impl Into<String>,
        value: impl Into<String>,
        unit: impl Into<String>,
    ) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            unit: unit.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TenantRow {
    pub id: i64,
    pub slug: String,
    pub name: String,
    pub status_label: String,
    pub status_class: String,
    pub created: String,
}

#[derive(Debug, Clone)]
pub struct ServiceRow {
    pub id: i64,
    pub tenant_name: String,
    pub kind_label: String,
    pub kind_tone: &'static str,
    pub unit_name: String,
    pub plan_code: String,
    pub state_label: String,
    pub state_class: String,
    pub observed: String,
    pub days: String,
    pub public_url: String,
    pub has_public_url: bool,
}

#[derive(Debug, Clone)]
pub struct JobRow {
    pub id: i64,
    pub action: String,
    pub state_label: String,
    pub state_class: String,
    pub started: String,
    pub error: String,
}

#[derive(Debug, Clone)]
pub struct AuditRow {
    pub ts: String,
    pub actor: String,
    pub action: String,
    pub resource: String,
    pub outcome: String,
    pub outcome_class: String,
    pub reason: String,
    pub src_ip: String,
}

#[derive(Debug, Clone)]
pub struct PlanRow {
    pub code: String,
    pub price: String,
    pub envelope: String,
    pub disk_gb: i64,
    pub max_services: i64,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct PlanEnvelope {
    pub has_plan: bool,
    pub code: String,
    pub cpu_weight: String,
    pub memory_high: String,
    pub memory_max: String,
    pub tasks_max: String,
    pub disk_gb: String,
    pub monthly_price: String,
}

impl PlanEnvelope {
    pub fn from_plan(plan: Option<&Plan>) -> Self {
        match plan {
            Some(plan) => Self {
                has_plan: true,
                code: plan.code.clone(),
                cpu_weight: plan.cpu_weight.to_string(),
                memory_high: plan.memory_high.clone(),
                memory_max: plan.memory_max.clone(),
                tasks_max: plan.tasks_max.to_string(),
                disk_gb: plan.disk_gb.to_string(),
                monthly_price: billing::format_money(plan.monthly_price),
            },
            None => Self {
                has_plan: false,
                code: "—".to_string(),
                cpu_weight: "—".to_string(),
                memory_high: "—".to_string(),
                memory_max: "—".to_string(),
                tasks_max: "—".to_string(),
                disk_gb: "—".to_string(),
                monthly_price: "—".to_string(),
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct CatalogRow {
    pub label: String,
    pub tone: &'static str,
    pub summary: String,
    pub image: String,
    pub egress: String,
    pub public_ports: String,
    pub controls: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct KindOption {
    pub value: String,
    pub label: String,
}

pub fn render<T: Template>(template: &T) -> AppResult<Html<String>> {
    template
        .render()
        .map(Html)
        .map_err(|error| AppError::Internal(anyhow::anyhow!("template: {error}")))
}

pub fn kind_tone(kind: ServiceKind) -> &'static str {
    match kind {
        ServiceKind::Web => "tone-web",
        ServiceKind::Database => "tone-database",
        ServiceKind::Storage => "tone-storage",
        ServiceKind::Git => "tone-git",
        ServiceKind::Container => "tone-web",
    }
}

pub fn service_state_class(state: ServiceState) -> &'static str {
    match state {
        ServiceState::Running => "ok",
        ServiceState::Pending => "warn",
        ServiceState::Stopped => "muted",
        ServiceState::Suspended => "warn",
        ServiceState::Failed => "danger",
        ServiceState::Deleted => "muted",
    }
}

pub fn job_state_class(state: JobState) -> &'static str {
    match state {
        JobState::Done => "ok",
        JobState::Failed => "danger",
        JobState::Running => "warn",
        JobState::Queued => "muted",
    }
}

pub fn tenant_status_class(status: TenantStatus) -> &'static str {
    match status {
        TenantStatus::Active => "ok",
        TenantStatus::Grace => "warn",
        TenantStatus::Suspended => "danger",
        TenantStatus::Purged => "muted",
    }
}

pub fn lifecycle_class(lifecycle: billing::Lifecycle) -> &'static str {
    match lifecycle {
        billing::Lifecycle::Active => "ok",
        billing::Lifecycle::Grace => "warn",
        billing::Lifecycle::Suspended => "danger",
        billing::Lifecycle::Purgeable => "danger",
    }
}

pub fn tenant_rows(tenants: &[Tenant]) -> Vec<TenantRow> {
    tenants
        .iter()
        .map(|tenant| TenantRow {
            id: tenant.id,
            slug: tenant.slug.clone(),
            name: tenant.name.clone(),
            status_label: tenant.status.label().to_string(),
            status_class: tenant_status_class(tenant.status).to_string(),
            created: billing::format_day(tenant.created_at),
        })
        .collect()
}

pub fn observed_text(service: &ServiceView) -> String {
    match (service.observed_state.as_deref(), service.observed_at) {
        (Some(state), Some(at)) => format!("{state} · {}", billing::format_timestamp(at)),
        _ => "desconocido · sin observación válida".to_string(),
    }
}

pub fn observation_is_active(
    service: &ServiceView,
    now: OffsetDateTime,
    interval: std::time::Duration,
) -> bool {
    service.observed_state.as_deref() == Some("active")
        && service
            .observed_at
            .map(|at| {
                at <= now && now - at <= time::Duration::seconds((interval.as_secs() * 2) as i64)
            })
            .unwrap_or(false)
}

pub fn public_url(config: &Config, service: &ServiceView) -> Option<String> {
    match config.service_access {
        ServiceAccess::Public => match service.kind {
            ServiceKind::Database | ServiceKind::Container => None,
            _ => Some(format!(
                "https://{}.{}/",
                service.tenant_slug, config.tenant_domain
            )),
        },
        ServiceAccess::Loopback => {
            // Databases are reached with psql, not a browser URL. Every other
            // fixed template and generic container records the actual allocated
            // port, so a second instance never shows the first's URL.
            if service.kind == ServiceKind::Database {
                return None;
            }
            let ports: serde_json::Value = serde_json::from_str(&service.ports_json).ok()?;
            let port = ports
                .get("web")?
                .as_u64()
                .filter(|p| (1..=65535).contains(p))?;
            Some(format!("http://127.0.0.1:{port}/"))
        }
    }
}

pub fn service_rows(
    services: &[ServiceView],
    now: OffsetDateTime,
    config: &Config,
) -> Vec<ServiceRow> {
    services
        .iter()
        .map(|service| {
            let public_url = public_url(config, service);
            let has_public_url = public_url.is_some();
            ServiceRow {
                id: service.id,
                tenant_name: service.tenant_name.clone(),
                kind_label: catalog::label(service.kind).to_string(),
                kind_tone: kind_tone(service.kind),
                unit_name: service.unit_name.clone(),
                plan_code: service.plan_code.clone(),
                state_label: service.state.label().to_string(),
                state_class: service_state_class(service.state).to_string(),
                observed: observed_text(service),
                days: billing::days_remaining(service.expires_at, now)
                    .map(|days| days.to_string())
                    .unwrap_or_else(|| "—".to_string()),
                has_public_url,
                public_url: public_url.unwrap_or_default(),
            }
        })
        .collect()
}

pub fn job_rows(jobs: &[Job]) -> Vec<JobRow> {
    jobs.iter()
        .map(|job| JobRow {
            id: job.id,
            action: job.action.clone(),
            state_label: job.state.label().to_string(),
            state_class: job_state_class(job.state).to_string(),
            started: job
                .started_at
                .map(billing::format_timestamp)
                .unwrap_or_else(|| "—".to_string()),
            error: job.error.clone().unwrap_or_else(|| "—".to_string()),
        })
        .collect()
}

pub fn audit_rows(events: &[AuditEvent]) -> Vec<AuditRow> {
    events
        .iter()
        .map(|event| AuditRow {
            ts: billing::format_timestamp(event.ts),
            actor: format!(
                "{}:{}",
                event.actor_type,
                event.actor_id.as_deref().unwrap_or("-")
            ),
            action: event.action.clone(),
            resource: format!(
                "{}:{}",
                event.resource_type.as_deref().unwrap_or("-"),
                event.resource_id.as_deref().unwrap_or("-")
            ),
            outcome: event.outcome.clone(),
            outcome_class: if event.outcome == "ok" {
                "ok".to_string()
            } else {
                "danger".to_string()
            },
            reason: event.reason.clone().unwrap_or_else(|| "—".to_string()),
            src_ip: event.src_ip.clone().unwrap_or_else(|| "—".to_string()),
        })
        .collect()
}

pub fn plan_rows(plans: &[Plan]) -> Vec<PlanRow> {
    plans
        .iter()
        .map(|plan| PlanRow {
            code: plan.code.clone(),
            price: billing::format_money(plan.monthly_price),
            envelope: format!(
                "cpu {} · mem {} · {} tareas",
                plan.cpu_weight, plan.memory_max, plan.tasks_max
            ),
            disk_gb: plan.disk_gb,
            max_services: plan.max_services,
            description: plan.description.clone(),
        })
        .collect()
}

pub fn catalog_rows() -> Vec<CatalogRow> {
    catalog::all()
        .iter()
        .map(|entry| CatalogRow {
            label: entry.label.to_string(),
            tone: kind_tone(entry.kind),
            summary: entry.summary.to_string(),
            image: entry.image.to_string(),
            egress: if entry.needs_egress {
                "requiere perfil declarado".to_string()
            } else {
                "ninguna".to_string()
            },
            public_ports: if entry.public_ports.is_empty() {
                "ninguno".to_string()
            } else {
                entry.public_ports.join(", ")
            },
            controls: entry
                .controls
                .iter()
                .map(|control| (*control).to_string())
                .collect(),
        })
        .collect()
}

pub fn kind_options() -> Vec<KindOption> {
    ServiceKind::all()
        .iter()
        .map(|kind| KindOption {
            value: kind.as_str().to_string(),
            label: catalog::label(*kind).to_string(),
        })
        .collect()
}
