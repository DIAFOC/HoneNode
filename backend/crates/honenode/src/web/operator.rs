use askama::Template;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use serde::Deserialize;
use time::OffsetDateTime;

use crate::audit::{self, Event};
use crate::billing;
use crate::broker;
use crate::catalog;
use crate::error::{AppError, AppResult};
use crate::models::{JobState, ServiceKind, ServiceState};
use crate::registry;
use crate::state::AppState;
use crate::telemetry;
use crate::topup;
use crate::web::layout::{
    self, audit_rows, catalog_rows, job_rows, kind_options, plan_rows, principal_view, render,
    service_rows, tenant_rows, PlanEnvelope, PrincipalView, ServiceRow, StatView, TenantRow, GOLD,
};
use crate::web::middleware::{self, Current};

#[derive(Debug, Deserialize)]
pub struct CreateTenantForm {
    pub csrf: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct ActionForm {
    pub csrf: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisionForm {
    pub csrf: String,
    pub kind: String,
    pub plan_code: String,
}

#[derive(Debug, Deserialize)]
pub struct AddTimeForm {
    pub csrf: String,
    pub days: i64,
}

#[derive(Debug, Deserialize)]
pub struct GenerateKeysForm {
    pub csrf: String,
    pub plan_code: String,
    pub days: i64,
    pub count: i64,
}

#[derive(Debug, Clone)]
pub struct KeyRowView {
    pub prefix: String,
    pub plan_code: String,
    pub days: i64,
    pub created: String,
    pub claimed: String,
    pub claimed_by: String,
    pub status_class: String,
}

#[derive(Template)]
#[template(path = "operator/keys.html")]
struct KeysTemplate {
    title: &'static str,
    accent_class: &'static str,
    principal: PrincipalView,
    keys: Vec<KeyRowView>,
    has_keys: bool,
    csrf: String,
    has_error: bool,
    error: String,
    plan_code: String,
    days: i64,
    count: i64,
}

#[derive(Template)]
#[template(path = "operator/keys_created.html")]
struct KeysCreatedTemplate {
    title: &'static str,
    accent_class: &'static str,
    principal: PrincipalView,
    plan_code: String,
    days: i64,
    codes: Vec<String>,
}

pub async fn keys_page(State(state): State<AppState>, current: Current) -> AppResult<Response> {
    let principal = current.require_operator()?;
    keys_response(&state, &principal, String::new(), 30, 10, None).await
}

async fn keys_response(
    state: &AppState,
    principal: &crate::models::Principal,
    plan_code: String,
    days: i64,
    count: i64,
    error: Option<String>,
) -> AppResult<Response> {
    let rows: Vec<KeyRowView> = topup::list(&state.db, 200)
        .await?
        .into_iter()
        .map(|key| KeyRowView {
            prefix: key.code_prefix,
            plan_code: key.plan_code,
            days: key.days,
            created: billing::format_timestamp(key.created_at),
            claimed: key
                .claimed_at
                .map(billing::format_timestamp)
                .unwrap_or_else(|| "—".to_string()),
            claimed_by: key.claimed_by.unwrap_or_else(|| "—".to_string()),
            status_class: if key.claimed_at.is_some() {
                "muted".to_string()
            } else {
                "ok".to_string()
            },
        })
        .collect();
    let has_keys = !rows.is_empty();
    let (has_error, error) = match error {
        Some(error) => (true, error),
        None => (false, String::new()),
    };
    Ok(render(&KeysTemplate {
        title: "Claves de recarga",
        accent_class: GOLD,
        principal: principal_view(Some(principal), state.config.theme),
        keys: rows,
        has_keys,
        csrf: principal.session.csrf_token.clone(),
        has_error,
        error,
        plan_code,
        days,
        count,
    })?
    .into_response())
}

pub async fn generate_keys(
    State(state): State<AppState>,
    headers: HeaderMap,
    current: Current,
    Form(form): Form<GenerateKeysForm>,
) -> AppResult<Response> {
    let principal = current.require_operator()?;
    middleware::check_csrf(&principal, &form.csrf)?;
    let codes = topup::generate(
        &state.db,
        form.plan_code.trim(),
        form.days,
        form.count,
        Some(principal.account.id),
    )
    .await;

    let codes = match codes {
        Ok(codes) => codes,
        Err(error) => {
            return keys_response(
                &state,
                &principal,
                form.plan_code.trim().to_string(),
                form.days,
                form.count,
                Some(error.to_string()),
            )
            .await
        }
    };

    let context = audit::Context::from_headers(&headers);
    context
        .record(
            &state.db,
            Event {
                tenant_id: None,
                actor_type: "account",
                actor_id: Some(&principal.account.number),
                action: "topup.generate",
                resource_type: Some("topup_key"),
                resource_id: Some(&format!("{}x{}d/{}", form.plan_code.trim(), form.days, codes.len())),
                outcome: "ok",
                reason: None,
            },
        )
        .await?;

    Ok(render(&KeysCreatedTemplate {
        title: "Claves generadas",
        accent_class: GOLD,
        principal: principal_view(Some(&principal), state.config.theme),
        plan_code: form.plan_code.trim().to_string(),
        days: form.days,
        codes,
    })?
    .into_response())
}

#[derive(Template)]
#[template(path = "operator/dashboard.html")]
struct DashboardTemplate {
    title: &'static str,
    accent_class: &'static str,
    principal: PrincipalView,
    stats: Vec<StatView>,
    host_stats: Vec<StatView>,
    jobs: Vec<layout::JobRow>,
    has_jobs: bool,
    services: Vec<ServiceRow>,
    has_services: bool,
    with_tenant: bool,
}

#[derive(Template)]
#[template(path = "operator/tenants.html")]
struct TenantsTemplate {
    title: &'static str,
    accent_class: &'static str,
    principal: PrincipalView,
    tenants: Vec<TenantRow>,
    has_tenants: bool,
    csrf: String,
}

#[derive(Template)]
#[template(path = "operator/tenant_created.html")]
struct TenantCreatedTemplate {
    title: &'static str,
    accent_class: &'static str,
    principal: PrincipalView,
    tenant_id: i64,
    slug: String,
    name: String,
    account_number: String,
    password: String,
    outcome_ok: bool,
    broker_note: String,
    has_broker_note: bool,
}

#[derive(Template)]
#[template(path = "operator/tenant_detail.html")]
struct TenantDetailTemplate {
    title: &'static str,
    accent_class: &'static str,
    principal: PrincipalView,
    tenant_id: i64,
    tenant_name: String,
    tenant_slug: String,
    status_label: String,
    status_class: String,
    created: String,
    kind_options: Vec<layout::KindOption>,
    entries: Vec<crate::portal_catalog::CatalogEntry>,
    has_entries: bool,
    plans: Vec<layout::PlanRow>,
    has_plans: bool,
    services: Vec<ServiceRow>,
    has_services: bool,
    with_tenant: bool,
    events: Vec<layout::AuditRow>,
    has_events: bool,
    csrf: String,
}

#[derive(Template)]
#[template(path = "operator/service_detail.html")]
struct ServiceDetailTemplate {
    title: &'static str,
    accent_class: &'static str,
    principal: PrincipalView,
    service_id: i64,
    service_label: String,
    unit: String,
    tenant_slug: String,
    state_label: String,
    state_class: String,
    lifecycle_label: String,
    lifecycle_class: String,
    observed: String,
    plan_code: String,
    expires_text: String,
    days_text: String,
    ports: String,
    plan: PlanEnvelope,
    csrf: String,
    events: Vec<layout::AuditRow>,
    has_events: bool,
}

#[derive(Template)]
#[template(path = "operator/catalog.html")]
struct CatalogTemplate {
    title: &'static str,
    accent_class: &'static str,
    principal: PrincipalView,
    entries: Vec<layout::CatalogRow>,
    has_entries: bool,
    plans: Vec<layout::PlanRow>,
    has_plans: bool,
}

#[derive(Template)]
#[template(path = "operator/services.html")]
struct ServicesTemplate {
    title: &'static str,
    accent_class: &'static str,
    principal: PrincipalView,
    services: Vec<ServiceRow>,
    has_services: bool,
    with_tenant: bool,
}

#[derive(Template)]
#[template(path = "operator/jobs.html")]
struct JobsTemplate {
    title: &'static str,
    accent_class: &'static str,
    principal: PrincipalView,
    jobs: Vec<layout::JobRow>,
    has_jobs: bool,
}

#[derive(Template)]
#[template(path = "operator/audit.html")]
struct AuditTemplate {
    title: &'static str,
    accent_class: &'static str,
    principal: PrincipalView,
    events: Vec<layout::AuditRow>,
    has_events: bool,
}

pub async fn dashboard(State(state): State<AppState>, current: Current) -> AppResult<Response> {
    let principal = current.require_operator()?;
    let tenants = registry::tenants(&state.db).await?;
    let services = registry::all_services(&state.db).await?;
    let jobs = registry::recent_jobs(&state.db, 8).await?;
    let host = telemetry::host_snapshot(&state.config.tenant_data_root);
    let broker_context =
        broker::CallContext::for_principal(&crate::auth::generate_token()?, &principal)?;
    let broker_up = state.broker.reachable(&broker_context).await;

    let live = services.iter().filter(|s| s.state.is_live()).count();
    let expiring = services
        .iter()
        .filter(|s| {
            billing::days_remaining(s.expires_at, OffsetDateTime::now_utc())
                .map(|days| days <= 7)
                .unwrap_or(false)
        })
        .count();

    let stats = vec![
        StatView::new(
            "Inquilinos",
            tenants.len().to_string(),
            "cuentas provisionadas",
        ),
        StatView::new(
            "Servicios",
            services.len().to_string(),
            format!(
                "{live} solicitudes de ejecución · {} observados activos",
                services
                    .iter()
                    .filter(|service| layout::observation_is_active(
                        service,
                        OffsetDateTime::now_utc(),
                        state.config.sample_interval
                    ))
                    .count()
            ),
        ),
        StatView::new("Por vencer", expiring.to_string(), "7 días o menos"),
        StatView::new(
            "Broker",
            if broker_up {
                "en línea"
            } else {
                "sin respuesta"
            },
            "mediador de autoridad",
        ),
    ];

    let mut host_stats = vec![
        StatView::new("Carga", format!("{:.2}", host.load1), "1 minuto"),
        StatView::new(
            "Memoria",
            billing::format_bytes(host.memory_total.saturating_sub(host.memory_available)),
            format!("de {}", billing::format_bytes(host.memory_total)),
        ),
        StatView::new(
            "Disco",
            billing::format_bytes(host.disk_used),
            format!("de {}", billing::format_bytes(host.disk_total)),
        ),
        StatView::new(
            "Uptime",
            format!("{}s", state.uptime_seconds()),
            "del plano de gestión",
        ),
    ];

    for (label, kind) in [
        ("PSI CPU", "cpu"),
        ("PSI memoria", "memory"),
        ("PSI E/S", "io"),
    ] {
        let (value, detail) = match telemetry::read_pressure(kind) {
            Some(pressure) => (
                format!("{:.2}%", pressure.avg10),
                format!("some · 10/60/300 s: {:.2}/{:.2}/{:.2}% · acumulado {:.1} s · visible al proceso",
                    pressure.avg10, pressure.avg60, pressure.avg300, pressure.total_us as f64 / 1_000_000.0),
            ),
            None => ("—".to_string(), "PSI no disponible en el espacio de nombres del proceso".to_string()),
        };
        host_stats.push(StatView::new(label, value, detail));
    }

    let rows = service_rows(&services, OffsetDateTime::now_utc(), &state.config);
    let job_view_rows = job_rows(&jobs);
    let has_jobs = !job_view_rows.is_empty();
    let has_services = !rows.is_empty();

    Ok(render(&DashboardTemplate {
        title: "Panel de operador",
        accent_class: GOLD,
        principal: principal_view(Some(&principal), state.config.theme),
        stats,
        host_stats,
        has_jobs,
        jobs: job_view_rows,
        has_services,
        services: rows,
        with_tenant: true,
    })
    .into_response())
}

pub async fn tenants_page(State(state): State<AppState>, current: Current) -> AppResult<Response> {
    let principal = current.require_operator()?;
    let tenants = registry::tenants(&state.db).await?;
    let rows = tenant_rows(&tenants);

    Ok(render(&TenantsTemplate {
        title: "Inquilinos",
        accent_class: GOLD,
        principal: principal_view(Some(&principal), state.config.theme),
        has_tenants: !rows.is_empty(),
        tenants: rows,
        csrf: principal.session.csrf_token.clone(),
    })
    .into_response())
}

pub async fn create_tenant(
    State(state): State<AppState>,
    headers: HeaderMap,
    current: Current,
    Form(form): Form<CreateTenantForm>,
) -> AppResult<Response> {
    let principal = current.require_operator()?;
    middleware::check_csrf(&principal, &form.csrf)?;

    let name = form.name.trim();
    if name.is_empty() || name.len() > 60 {
        return Err(AppError::BadRequest("nombre inválido".to_string()));
    }

    let _guard = state.mutations.lock().await;
    let slug = registry::next_tenant_slug(&state.db, &state.config.tenants).await?;
    let password = crate::auth::generate_password(24)?;
    let account =
        crate::auth::create_account(&state.db, crate::models::Role::Tenant, &password, None)
            .await?;
    let tenant = registry::create_tenant(&state.db, account.id, &slug, name).await?;

    let context = audit::Context::from_headers(&headers);
    let broker_context = context.broker_context(&principal)?;
    let job = registry::create_job(
        &state.db,
        Some(tenant.id),
        "tenant.create",
        &serde_json::json!({ "slug": slug, "correlation": broker_context }).to_string(),
        Some(principal.account.id),
    )
    .await?;

    let outcome = state
        .broker
        .call_ok_with_context(
            &broker::Request::TenantCreate {
                tenant_id: slug.clone(),
            },
            &broker_context,
        )
        .await;

    let (state_label, reason) = match &outcome {
        Ok(_) => (JobState::Done, None),
        Err(error) => (JobState::Failed, Some(error.to_string())),
    };
    registry::finish_job(&state.db, job, state_label, reason.as_deref()).await?;

    context
        .record(
            &state.db,
            Event {
                tenant_id: Some(tenant.id),
                actor_type: "account",
                actor_id: Some(&principal.account.number),
                action: "tenant.create",
                resource_type: Some("tenant"),
                resource_id: Some(&slug),
                outcome: if outcome.is_ok() { "ok" } else { "failed" },
                reason: reason.as_deref(),
            },
        )
        .await?;

    let has_broker_note = reason.is_some();

    Ok(render(&TenantCreatedTemplate {
        title: "Inquilino creado",
        accent_class: GOLD,
        principal: principal_view(Some(&principal), state.config.theme),
        tenant_id: tenant.id,
        slug,
        name: name.to_string(),
        account_number: account.number,
        password,
        outcome_ok: outcome.is_ok(),
        broker_note: reason.unwrap_or_default(),
        has_broker_note,
    })
    .into_response())
}

pub async fn tenant_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    current: Current,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let principal = current.require_operator()?;
    let tenant = registry::tenant_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let services = registry::services_of_tenant(&state.db, id).await?;
    let plans = registry::plans(&state.db).await?;
    let context = audit::Context::from_headers(&headers).broker_context(&principal)?;
    let mut entries = if tenant.status == crate::models::TenantStatus::Active {
        match crate::portal_catalog::load(&state.broker, &tenant.slug, &context).await {
            Ok(entries) => entries,
            Err(error) => {
                // Catalog failure must not hide existing services or their operator controls.
                tracing::warn!(tenant_id = id, %error, "provisioning catalog unavailable");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    let reserved: Vec<String> = sqlx::query_scalar("SELECT kind FROM services WHERE tenant_id = ?")
        .bind(id).fetch_all(&state.db).await?;
    let count = registry::count_services(&state.db, id).await?;
    for entry in &mut entries {
        entry.reserved_kinds = reserved.clone();
        entry.can_provision = count < entry.max_services && entry.allowed_kinds.iter().any(|kind| !reserved.contains(kind));
    }
    let events = registry::audit_of_tenant(&state.db, id, 15).await?;

    let rows = service_rows(&services, OffsetDateTime::now_utc(), &state.config);
    let plan_view_rows = plan_rows(&plans);
    let event_rows = audit_rows(&events);
    let has_plans = !plan_view_rows.is_empty();
    let has_services = !rows.is_empty();
    let has_events = !event_rows.is_empty();

    Ok(render(&TenantDetailTemplate {
        title: "Inquilino",
        accent_class: GOLD,
        principal: principal_view(Some(&principal), state.config.theme),
        tenant_id: tenant.id,
        tenant_name: tenant.name.clone(),
        tenant_slug: tenant.slug.clone(),
        status_label: tenant.status.label().to_string(),
        status_class: layout::tenant_status_class(tenant.status).to_string(),
        created: billing::format_timestamp(tenant.created_at),
        kind_options: kind_options(),
        has_entries: !entries.is_empty(),
        entries,
        has_plans,
        plans: plan_view_rows,
        has_services,
        services: rows,
        with_tenant: false,
        has_events,
        events: event_rows,
        csrf: principal.session.csrf_token.clone(),
    })
    .into_response())
}

/// Operator-initiated tenant deletion: every live service is removed through
/// the broker first, and the account/tenant rows are deleted only when all of
/// them succeeded. The host Unix account and volume data are retained.
pub async fn delete_tenant(
    State(state): State<AppState>,
    headers: HeaderMap,
    current: Current,
    Path(id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> AppResult<Response> {
    let principal = current.require_operator()?;
    middleware::check_csrf(&principal, &form.csrf)?;
    let _guard = state.mutations.lock().await;
    let tenant = registry::tenant_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let services = registry::services_of_tenant(&state.db, id).await?;
    let context = audit::Context::from_headers(&headers);
    let broker_context = context.broker_context(&principal)?;

    let mut failures = Vec::new();
    for service in &services {
        let request = broker::Request::ServiceDelete {
            tenant_id: tenant.slug.clone(),
            service: service.unit_name.clone(),
        };
        let job = registry::create_job(
            &state.db,
            Some(id),
            request.op(),
            &serde_json::json!({ "unit": service.unit_name, "correlation": broker_context })
                .to_string(),
            Some(principal.account.id),
        )
        .await?;
        registry::start_job(&state.db, job).await?;
        match state
            .broker
            .call_ok_with_context(&request, &broker_context)
            .await
        {
            Ok(_) => registry::finish_job(&state.db, job, JobState::Done, None).await?,
            Err(error) => {
                registry::finish_job(&state.db, job, JobState::Failed, Some(&error.to_string()))
                    .await?;
                failures.push(format!("{}: {error}", service.unit_name));
            }
        }
    }
    if !failures.is_empty() {
        return Err(AppError::Conflict(format!(
            "no se eliminó el inquilino; el broker rechazó: {}",
            failures.join("; ")
        )));
    }

    // Deleting the account cascades the tenant row and its services; audit
    // events keep their history with tenant_id set to NULL.
    sqlx::query("DELETE FROM accounts WHERE id = ?")
        .bind(tenant.account_id)
        .execute(&state.db)
        .await?;
    context
        .record(
            &state.db,
            Event {
                tenant_id: None,
                actor_type: "account",
                actor_id: Some(&principal.account.number),
                action: "tenant.delete",
                resource_type: Some("tenant"),
                resource_id: Some(&tenant.slug),
                outcome: "ok",
                reason: None,
            },
        )
        .await?;
    Ok(Redirect::to("/operator/tenants").into_response())
}

pub async fn provision(
    State(state): State<AppState>,
    headers: HeaderMap,
    current: Current,
    Path(id): Path<i64>,
    Form(form): Form<ProvisionForm>,
) -> AppResult<Response> {
    let principal = current.require_operator()?;
    middleware::check_csrf(&principal, &form.csrf)?;
    let kind = ServiceKind::parse(&form.kind)
        .ok_or_else(|| AppError::BadRequest("tipo de servicio desconocido".to_string()))?;
    let service = crate::provisioning::provision(
        &state, &principal, id, &form.plan_code, kind, &audit::Context::from_headers(&headers),
    ).await?;
    let destination = if service.state == ServiceState::Failed {
        format!("/operator/services/{}", service.id)
    } else {
        format!("/operator/tenants/{id}")
    };
    Ok(Redirect::to(&destination).into_response())
}

pub async fn service_action(
    State(state): State<AppState>,
    headers: HeaderMap,
    current: Current,
    Path((id, action)): Path<(i64, String)>,
    Form(form): Form<crate::web::tenant::ActionForm>,
) -> AppResult<Response> {
    let principal = current.require_operator()?;
    middleware::check_csrf(&principal, &form.csrf)?;
    let _guard = state.mutations.lock().await;

    let service = registry::service_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let tenant = registry::tenant_by_id(&state.db, service.tenant_id)
        .await?
        .ok_or(AppError::NotFound)?;

    if service.deleted_at.is_some() || service.state == ServiceState::Deleted {
        return Err(AppError::Conflict("service deleted".to_string()));
    }
    let request = match action.as_str() {
        "start" => broker::Request::ServiceStart {
            tenant_id: tenant.slug.clone(),
            service: service.unit_name.clone(),
        },
        "stop" => broker::Request::ServiceStop {
            tenant_id: tenant.slug.clone(),
            service: service.unit_name.clone(),
        },
        "restart" => broker::Request::ServiceRestart {
            tenant_id: tenant.slug.clone(),
            service: service.unit_name.clone(),
        },
        "suspend" => broker::Request::ServiceSuspend {
            tenant_id: tenant.slug.clone(),
            service: service.unit_name.clone(),
        },
        "resume" => broker::Request::ServiceResume {
            tenant_id: tenant.slug.clone(),
            service: service.unit_name.clone(),
        },
        "delete" => broker::Request::ServiceDelete {
            tenant_id: tenant.slug.clone(),
            service: service.unit_name.clone(),
        },
        other => return Err(AppError::BadRequest(format!("acción desconocida: {other}"))),
    };

    let context = audit::Context::from_headers(&headers);
    let broker_context = context.broker_context(&principal)?;
    let job = registry::create_job(
        &state.db,
        Some(tenant.id),
        request.op(),
        &serde_json::json!({ "unit": service.unit_name, "correlation": broker_context })
            .to_string(),
        Some(principal.account.id),
    )
    .await?;
    registry::start_job(&state.db, job).await?;

    let target = match action.as_str() {
        "stop" => ServiceState::Stopped,
        "suspend" => ServiceState::Suspended,
        "delete" => ServiceState::Deleted,
        _ => ServiceState::Running,
    };
    let outcome = crate::observation::execute(
        &state,
        &service,
        &tenant.slug,
        &request,
        target,
        Some(&broker_context),
    )
    .await;

    match &outcome {
        Ok(_) => registry::finish_job(&state.db, job, JobState::Done, None).await?,
        Err(error) => {
            registry::finish_job(&state.db, job, JobState::Failed, Some(&error.to_string())).await?
        }
    }
    context
        .record(
            &state.db,
            Event {
                tenant_id: Some(tenant.id),
                actor_type: "account",
                actor_id: Some(&principal.account.number),
                action: request.op(),
                resource_type: Some("service"),
                resource_id: Some(&service.unit_name),
                outcome: if outcome.is_ok() { "ok" } else { "failed" },
                reason: outcome
                    .as_ref()
                    .err()
                    .map(|error| error.to_string())
                    .as_deref(),
            },
        )
        .await?;

    outcome?;
    Ok(Redirect::to(&format!("/operator/tenants/{}", tenant.id)).into_response())
}

pub async fn add_time(
    State(state): State<AppState>,
    headers: HeaderMap,
    current: Current,
    Path(id): Path<i64>,
    Form(form): Form<AddTimeForm>,
) -> AppResult<Response> {
    let principal = current.require_operator()?;
    middleware::check_csrf(&principal, &form.csrf)?;

    if form.days < 1 || form.days > 3650 {
        return Err(AppError::BadRequest("plazo fuera de rango".to_string()));
    }

    let _guard = state.mutations.lock().await;
    let service = registry::service_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let expires_at = registry::extend_service(&state.db, id, form.days)
        .await?
        .ok_or(AppError::NotFound)?;

    let context = audit::Context::from_headers(&headers);
    context
        .record(
            &state.db,
            Event {
                tenant_id: Some(service.tenant_id),
                actor_type: "account",
                actor_id: Some(&principal.account.number),
                action: "billing.extend",
                resource_type: Some("service"),
                resource_id: Some(&service.unit_name),
                outcome: "ok",
                reason: None,
            },
        )
        .await?;

    tracing::info!(service = service.id, days = form.days, expires = %expires_at, "service extended");
    Ok(Redirect::to(&format!("/operator/tenants/{}", service.tenant_id)).into_response())
}

pub async fn service_detail(
    State(state): State<AppState>,
    current: Current,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let principal = current.require_operator()?;
    let service = registry::service_view(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let entry = catalog::entry(service.kind);
    let plan = registry::plan_by_id(&state.db, service.plan_id).await?;
    let events = registry::audit_of_tenant(&state.db, service.tenant_id, 20).await?;
    let now = OffsetDateTime::now_utc();
    let lifecycle = billing::lifecycle(
        service.expires_at,
        None,
        state.config.grace_days,
        state.config.retention_days,
        now,
    );

    let (expires_text, days_text) = match service.expires_at {
        Some(expires_at) => (
            billing::format_day(expires_at),
            billing::days_remaining(service.expires_at, now)
                .map(|days| format!("{days} días"))
                .unwrap_or_else(|| "—".to_string()),
        ),
        None => ("—".to_string(), "—".to_string()),
    };

    let event_rows = audit_rows(&events);
    let has_events = !event_rows.is_empty();

    Ok(render(&ServiceDetailTemplate {
        title: entry.label,
        accent_class: layout::kind_tone(service.kind),
        principal: principal_view(Some(&principal), state.config.theme),
        service_id: service.id,
        service_label: entry.label.to_string(),
        unit: service.unit_name.clone(),
        tenant_slug: service.tenant_slug.clone(),
        state_label: service.state.label().to_string(),
        state_class: layout::service_state_class(service.state).to_string(),
        lifecycle_label: lifecycle.label().to_string(),
        lifecycle_class: layout::lifecycle_class(lifecycle).to_string(),
        observed: layout::observed_text(&service),
        plan_code: service.plan_code.clone(),
        expires_text,
        days_text,
        ports: service.ports_json.clone(),
        plan: PlanEnvelope::from_plan(plan.as_ref()),
        csrf: principal.session.csrf_token.clone(),
        has_events,
        events: event_rows,
    })
    .into_response())
}

pub async fn catalog_page(State(state): State<AppState>, current: Current) -> AppResult<Response> {
    let principal = current.require_operator()?;
    let plans = registry::plans(&state.db).await?;
    let entries = catalog_rows();
    let plan_view_rows = plan_rows(&plans);
    let has_entries = !entries.is_empty();
    let has_plans = !plan_view_rows.is_empty();

    Ok(render(&CatalogTemplate {
        title: "Catálogo",
        accent_class: GOLD,
        principal: principal_view(Some(&principal), state.config.theme),
        has_entries,
        entries,
        has_plans,
        plans: plan_view_rows,
    })
    .into_response())
}

pub async fn services_page(State(state): State<AppState>, current: Current) -> AppResult<Response> {
    let principal = current.require_operator()?;
    let services = registry::all_services(&state.db).await?;
    let rows = service_rows(&services, OffsetDateTime::now_utc(), &state.config);
    let has_services = !rows.is_empty();

    Ok(render(&ServicesTemplate {
        title: "Servicios",
        accent_class: GOLD,
        principal: principal_view(Some(&principal), state.config.theme),
        has_services,
        services: rows,
        with_tenant: true,
    })
    .into_response())
}

pub async fn jobs_page(State(state): State<AppState>, current: Current) -> AppResult<Response> {
    let principal = current.require_operator()?;
    let jobs = registry::recent_jobs(&state.db, 100).await?;
    let rows = job_rows(&jobs);
    let has_jobs = !rows.is_empty();

    Ok(render(&JobsTemplate {
        title: "Trabajos",
        accent_class: GOLD,
        principal: principal_view(Some(&principal), state.config.theme),
        has_jobs,
        jobs: rows,
    })
    .into_response())
}

pub async fn audit_page(State(state): State<AppState>, current: Current) -> AppResult<Response> {
    let principal = current.require_operator()?;
    let events = registry::recent_audit(&state.db, 200).await?;
    let rows = audit_rows(&events);
    let has_events = !rows.is_empty();

    Ok(render(&AuditTemplate {
        title: "Auditoría",
        accent_class: GOLD,
        principal: principal_view(Some(&principal), state.config.theme),
        has_events,
        events: rows,
    })
    .into_response())
}
