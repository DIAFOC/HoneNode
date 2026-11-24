use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use serde::Deserialize;
use time::OffsetDateTime;

use crate::audit::{self, Event};
use crate::billing;
use crate::broker;
use crate::catalog;
use crate::error::{AppError, AppResult};
use crate::images::ImageReference;
use crate::models::{JobState, Service, ServiceKind, ServiceState, Tenant, TenantStatus};
use crate::portal_catalog::CatalogEntry;
use crate::registry;
use crate::service_options::{
    self, DatabaseOptions, FileManagerOptions, Secret, ServiceConfiguration, ServiceOptions,
};
use crate::state::AppState;
use crate::topup;
use crate::web::layout::{
    self, audit_rows, principal_view, render, service_rows, PrincipalView, ServiceRow, StatView,
    GOLD,
};
use crate::web::middleware::{self, Current};

#[derive(Debug, Deserialize)]
pub struct ActionForm {
    pub csrf: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisionForm {
    pub csrf: String,
    pub plan_code: String,
    pub kind: String,
    /// Exact approved reference from the deploy wizard; never a tenant-chosen image.
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub file_manager_username: Option<String>,
    #[serde(default)]
    pub file_manager_password: Option<Secret>,
    #[serde(default)]
    pub file_manager_read_only: Option<String>,
    #[serde(default)]
    pub database_password: Option<Secret>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeployQuery {
    pub plan_code: String,
    pub kind: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigureForm {
    pub csrf: String,
    #[serde(default)]
    pub read_only: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigurationQuery {
    #[serde(default)]
    pub updated: Option<String>,
}

#[derive(Debug, Clone)]
pub struct KeyValue {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct ImageRow {
    pub plan_code: String,
    pub kind: String,
    pub kind_label: String,
    pub tone: &'static str,
    pub reference: String,
    pub repository: String,
    pub tag: String,
    pub digest: String,
    pub has_registry_url: bool,
    pub registry_url: String,
    pub can_deploy: bool,
    pub deploy_url: String,
    pub archived: bool,
}

#[derive(Template)]
#[template(path = "tenant/images.html")]
struct ImagesTemplate {
    title: &'static str,
    accent_class: &'static str,
    principal: PrincipalView,
    tenant_name: String,
    rows: Vec<ImageRow>,
    registries: Vec<String>,
    has_rows: bool,
    catalog_unavailable: bool,
}

#[derive(Template)]
#[template(path = "tenant/deploy.html")]
struct DeployTemplate {
    title: &'static str,
    accent_class: &'static str,
    principal: PrincipalView,
    csrf: String,
    tenant_name: String,
    plan_code: String,
    kind: String,
    kind_label: String,
    image: String,
    repository: String,
    tag: String,
    digest: String,
    has_registry_url: bool,
    registry_url: String,
    memory_high: String,
    memory_max: String,
    cpu_quota_percent: u32,
    tasks_max: i64,
    service_days: i64,
    unit: String,
    ports: Vec<KeyValue>,
    volumes: Vec<KeyValue>,
    is_storage: bool,
    is_database: bool,
    is_git: bool,
    database_name: String,
    archived: bool,
    can_provision: bool,
    has_error: bool,
    error: String,
    username: String,
    read_only: bool,
}

#[derive(Template)]
#[template(path = "tenant/service_configuration.html")]
struct ConfigurationTemplate {
    title: &'static str,
    accent_class: &'static str,
    principal: PrincipalView,
    csrf: String,
    service_id: i64,
    tab: &'static str,
    service_label: String,
    tenant_name: String,
    unit: String,
    state_label: String,
    state_class: String,
    kind: String,
    image: String,
    is_storage: bool,
    is_web: bool,
    username: String,
    read_only: bool,
    stopped: bool,
    can_configure: bool,
    has_notice: bool,
    notice_class: &'static str,
    notice: String,
}

#[derive(Template)]
#[template(path = "tenant/service_logs.html")]
struct LogsTemplate {
    title: &'static str,
    accent_class: &'static str,
    principal: PrincipalView,
    service_id: i64,
    tab: &'static str,
    service_label: String,
    tenant_name: String,
    unit: String,
    is_web: bool,
    lines: i64,
    logs: String,
}

#[derive(Template)]
#[template(path = "tenant/catalog.html")]
struct CatalogTemplate {
    title: &'static str,
    accent_class: &'static str,
    principal: PrincipalView,
    csrf: String,
    tenant_name: String,
    entries: Vec<crate::portal_catalog::CatalogEntry>,
    has_entries: bool,
    catalog_unavailable: bool,
}

#[derive(Template)]
#[template(path = "tenant/portal.html")]
struct PortalTemplate {
    title: &'static str,
    accent_class: &'static str,
    principal: PrincipalView,
    tenant_name: String,
    has_notice: bool,
    notice_message: String,
    stats: Vec<StatView>,
    services: Vec<ServiceRow>,
    has_services: bool,
    events: Vec<layout::AuditRow>,
    has_events: bool,
    has_subscription: bool,
    plan_code: String,
    paid_through: String,
    subscription_days: String,
    trial_expired: bool,
    has_redeemed: bool,
    redeemed_days: i64,
    has_redeem_error: bool,
    redeem_error: String,
    csrf: String,
}

#[derive(Debug, Deserialize)]
pub struct RedeemForm {
    pub csrf: String,
    pub code: String,
}

#[derive(Debug, Deserialize)]
pub struct PortalQuery {
    pub topup: Option<i64>,
}

#[derive(Template)]
#[template(path = "tenant/service_detail.html")]
struct ServiceDetailTemplate {
    title: &'static str,
    accent_class: &'static str,
    principal: PrincipalView,
    service_id: i64,
    service_label: String,
    tenant_name: String,
    unit: String,
    plan_code: String,
    state_label: String,
    state_class: String,
    lifecycle_label: String,
    lifecycle_class: String,
    observed: String,
    expires_text: String,
    days_text: String,
    created: String,
    plan: layout::PlanEnvelope,
    has_public_url: bool,
    public_url: String,
    controls: Vec<String>,
    container_info: String,
    is_running: bool,
    can_manage: bool,
    is_storage: bool,
    is_web: bool,
    is_database: bool,
    is_git: bool,
    database_port: String,
    database_name: String,
    tab: &'static str,
    csrf: String,
    events: Vec<layout::AuditRow>,
    has_events: bool,
}

#[derive(Template)]
#[template(path = "tenant/audit.html")]
struct AuditTemplate {
    title: &'static str,
    accent_class: &'static str,
    principal: PrincipalView,
    events: Vec<layout::AuditRow>,
    has_events: bool,
}

/// Tenant owning the session, with an active subscription for catalogue reads.
pub(super) async fn catalogue_tenant(state: &AppState, principal: &crate::models::Principal) -> AppResult<Tenant> {
    let tenant_id = principal.require_tenant()?;
    let tenant = registry::tenant_by_id(&state.db, tenant_id)
        .await?
        .ok_or(AppError::NotFound)?;
    if tenant.status != TenantStatus::Active {
        return Err(AppError::Forbidden);
    }
    Ok(tenant)
}

/// Live root catalogue annotated with local reservations; local plans are never
/// an authorization source. The second value reports an unavailable broker.
async fn annotated_catalog(
    state: &AppState,
    tenant: &Tenant,
    context: &broker::CallContext,
) -> AppResult<(Vec<CatalogEntry>, bool)> {
    let (mut entries, catalog_unavailable) =
        match crate::portal_catalog::load(&state.broker, &tenant.slug, context).await {
            Ok(entries) => (entries, false),
            Err(error) => {
                tracing::warn!(tenant_id = tenant.id, %error, "tenant provisioning catalog unavailable");
                (Vec::new(), true)
            }
        };
    // Only live services occupy a kind; a deleted service frees it for a new
    // instance. Multiple instances of one kind are allowed, so the plan count
    // is the only local admission hint.
    let reserved: Vec<String> = sqlx::query_scalar(
        "SELECT kind FROM services
         WHERE tenant_id = ? AND deleted_at IS NULL AND state != 'deleted'",
    )
    .bind(tenant.id)
    .fetch_all(&state.db)
    .await?;
    let count = registry::count_services(&state.db, tenant.id).await?;
    for entry in &mut entries {
        entry.reserved_kinds = reserved.clone();
        entry.can_provision = count < entry.max_services;
    }
    Ok((entries, catalog_unavailable))
}

pub async fn catalog_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    current: Current,
) -> AppResult<Response> {
    let principal = current.require()?;
    let tenant = catalogue_tenant(&state, &principal).await?;
    let context = audit::Context::from_headers(&headers).broker_context(&principal)?;
    let (entries, catalog_unavailable) = annotated_catalog(&state, &tenant, &context).await?;
    Ok(render(&CatalogTemplate {
        title: "Catálogo", accent_class: GOLD,
        principal: principal_view(Some(&principal), state.config.theme),
        csrf: principal.session.csrf_token.clone(), tenant_name: tenant.name,
        has_entries: !entries.is_empty(), entries, catalog_unavailable,
    }).into_response())
}

pub async fn provision(
    State(state): State<AppState>,
    headers: HeaderMap,
    current: Current,
    Form(form): Form<ProvisionForm>,
) -> AppResult<Response> {
    let principal = current.require()?;
    let tenant_id = principal.require_tenant()?;
    middleware::check_csrf(&principal, &form.csrf)?;
    let kind = ServiceKind::parse(&form.kind)
        .ok_or_else(|| AppError::BadRequest("tipo de servicio desconocido".to_string()))?;
    let read_only = service_options::checkbox(form.file_manager_read_only.as_deref())?;
    let wizard = form.image.is_some()
        || form.file_manager_username.is_some()
        || form.file_manager_password.is_some()
        || form.database_password.is_some();
    let username = form.file_manager_username.clone().unwrap_or_default();
    let file_manager = match (form.file_manager_username, form.file_manager_password) {
        (Some(username), Some(password)) => Some(FileManagerOptions {
            username,
            password,
            read_only,
        }),
        (None, None) => None,
        _ => {
            return Err(AppError::BadRequest(
                "usuario y contraseña del gestor de archivos deben enviarse juntos".to_string(),
            ))
        }
    };
    let database = form.database_password.map(|password| DatabaseOptions { password });
    let options = wizard.then_some(ServiceOptions {
        image: form.image.clone(),
        file_manager,
        database,
    });
    let result = crate::provisioning::provision_with_options(
        &state,
        &principal,
        tenant_id,
        &form.plan_code,
        kind,
        options,
        &audit::Context::from_headers(&headers),
    )
    .await;
    match result {
        Ok(service) => Ok(Redirect::to(&format!("/portal/services/{}", service.id)).into_response()),
        // Wizard validation failures re-render the form with the password blank;
        // legacy web/database posts keep the original error responses.
        Err(AppError::BadRequest(message)) if wizard => {
            let tenant = catalogue_tenant(&state, &principal).await?;
            let context = audit::Context::from_headers(&headers).broker_context(&principal)?;
            let (entries, unavailable) = annotated_catalog(&state, &tenant, &context).await?;
            if unavailable {
                return Err(AppError::BadRequest(message));
            }
            let entry = crate::portal_catalog::select(&entries, &form.plan_code, kind)?;
            let template = deploy_view(
                &state,
                &principal,
                &tenant,
                entry,
                kind,
                Some(message),
                username,
                read_only,
            )
            .await?;
            Ok(render(&template).into_response())
        }
        Err(error) => Err(error),
    }
}

pub async fn portal(
    State(state): State<AppState>,
    current: Current,
    Query(query): Query<PortalQuery>,
) -> AppResult<Response> {
    let principal = current.require()?;
    let tenant_id = principal.require_tenant()?;
    let tenant = registry::tenant_by_id(&state.db, tenant_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let services = registry::services_of_tenant(&state.db, tenant_id).await?;
    let events = registry::audit_of_tenant(&state.db, tenant_id, 10).await?;
    let sample = registry::latest_sample(&state.db, tenant_id).await?;

    let now = OffsetDateTime::now_utc();
    let soonest = services
        .iter()
        .filter_map(|service| billing::days_remaining(service.expires_at, now))
        .min();

    let plan_code = topup::subscription_plan_code(&state.db, tenant_id)
        .await?
        .unwrap_or_default();
    let paid_through = topup::subscription_paid_through(&state.db, tenant_id).await?;
    let has_subscription = paid_through.is_some();
    let subscription_days = paid_through
        .map(|expiry| billing::days_remaining(Some(expiry), now).unwrap_or(0).to_string())
        .unwrap_or_default();
    let trial_expired = paid_through.is_some_and(|expiry| expiry <= now);

    let stats = vec![
        StatView::new("Servicios", services.len().to_string(), "contratados"),
        StatView::new(
            "Días restantes",
            soonest
                .map(|days| days.to_string())
                .unwrap_or_else(|| "—".to_string()),
            "del servicio más próximo a vencer",
        ),
        StatView::new(
            "Memoria",
            sample
                .and_then(|(_, memory, _)| memory)
                .map(|bytes| billing::format_bytes(bytes as u64))
                .unwrap_or_else(|| "—".to_string()),
            "cgroup del inquilino",
        ),
    ];

    let rows = service_rows(&services, now, &state.config);
    let event_rows = audit_rows(&events);
    let has_notice = tenant.status != crate::models::TenantStatus::Active;
    let has_services = !rows.is_empty();
    let has_events = !event_rows.is_empty();
    let has_redeemed = query.topup.is_some_and(|days| days > 0);
    let redeemed_days = query.topup.unwrap_or(0);

    Ok(render(&PortalTemplate {
        title: "Mis servicios",
        accent_class: GOLD,
        principal: principal_view(Some(&principal), state.config.theme),
        tenant_name: tenant.name.clone(),
        has_notice,
        notice_message: "La cuenta está fuera de su periodo vigente. Los servicios se suspenden al terminar la gracia.".to_string(),
        has_services,
        services: rows,
        has_events,
        events: event_rows,
        stats,
        has_subscription,
        plan_code,
        paid_through: paid_through
            .map(billing::format_day)
            .unwrap_or_else(|| "—".to_string()),
        subscription_days,
        trial_expired,
        has_redeemed,
        redeemed_days,
        has_redeem_error: false,
        redeem_error: String::new(),
        csrf: principal.session.csrf_token.clone(),
    })
    .into_response())
}

/// Redeems a top-up key: plan time is additive, suspended services become
/// startable again, and the account returns to active while time remains.
pub async fn redeem(
    State(state): State<AppState>,
    headers: HeaderMap,
    current: Current,
    Form(form): Form<RedeemForm>,
) -> AppResult<Response> {
    let principal = current.require()?;
    let tenant_id = principal.require_tenant()?;
    middleware::check_csrf(&principal, &form.csrf)?;

    let result = topup::redeem(&state.db, tenant_id, form.code.trim()).await;
    let context = audit::Context::from_headers(&headers);
    match &result {
        Ok(redemption) => {
            context
                .record(
                    &state.db,
                    Event {
                        tenant_id: Some(tenant_id),
                        actor_type: "account",
                        actor_id: Some(&principal.account.number),
                        action: "topup.redeem",
                        resource_type: Some("topup_key"),
                        resource_id: Some(&redemption.plan_code),
                        outcome: "ok",
                        reason: None,
                    },
                )
                .await?;
            Ok(Redirect::to(&format!("/portal?topup={}", redemption.days)).into_response())
        }
        Err(error) => {
            context
                .record(
                    &state.db,
                    Event {
                        tenant_id: Some(tenant_id),
                        actor_type: "account",
                        actor_id: Some(&principal.account.number),
                        action: "topup.redeem",
                        resource_type: Some("topup_key"),
                        resource_id: None,
                        outcome: "denied",
                        reason: Some(&error.to_string()),
                    },
                )
                .await?;
            portal_with_error(&state, principal, tenant_id, error.to_string()).await
        }
    }
}

async fn portal_with_error(
    state: &AppState,
    principal: crate::models::Principal,
    tenant_id: i64,
    error: String,
) -> AppResult<Response> {
    let tenant = registry::tenant_by_id(&state.db, tenant_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let services = registry::services_of_tenant(&state.db, tenant_id).await?;
    let events = registry::audit_of_tenant(&state.db, tenant_id, 10).await?;
    let now = OffsetDateTime::now_utc();
    let soonest = services
        .iter()
        .filter_map(|service| billing::days_remaining(service.expires_at, now))
        .min();
    let plan_code = topup::subscription_plan_code(&state.db, tenant_id)
        .await?
        .unwrap_or_default();
    let paid_through = topup::subscription_paid_through(&state.db, tenant_id).await?;
    let sample = registry::latest_sample(&state.db, tenant_id).await?;
    let stats = vec![
        StatView::new("Servicios", services.len().to_string(), "contratados"),
        StatView::new(
            "Días restantes",
            soonest
                .map(|days| days.to_string())
                .unwrap_or_else(|| "—".to_string()),
            "del servicio más próximo a vencer",
        ),
        StatView::new(
            "Memoria",
            sample
                .and_then(|(_, memory, _)| memory)
                .map(|bytes| billing::format_bytes(bytes as u64))
                .unwrap_or_else(|| "—".to_string()),
            "cgroup del inquilino",
        ),
    ];
    let rows = service_rows(&services, now, &state.config);
    let event_rows = audit_rows(&events);
    Ok(render(&PortalTemplate {
        title: "Mis servicios",
        accent_class: GOLD,
        principal: principal_view(Some(&principal), state.config.theme),
        tenant_name: tenant.name.clone(),
        has_notice: tenant.status != crate::models::TenantStatus::Active,
        notice_message: "La cuenta está fuera de su periodo vigente. Los servicios se suspenden al terminar la gracia.".to_string(),
        has_services: !rows.is_empty(),
        services: rows,
        has_events: !event_rows.is_empty(),
        events: event_rows,
        stats,
        has_subscription: paid_through.is_some(),
        plan_code,
        paid_through: paid_through
            .map(billing::format_day)
            .unwrap_or_else(|| "—".to_string()),
        subscription_days: paid_through
            .map(|expiry| billing::days_remaining(Some(expiry), now).unwrap_or(0).to_string())
            .unwrap_or_default(),
        trial_expired: paid_through.is_some_and(|expiry| expiry <= now),
        has_redeemed: false,
        redeemed_days: 0,
        has_redeem_error: true,
        redeem_error: error,
        csrf: principal.session.csrf_token.clone(),
    })
    .into_response())
}

pub async fn service_detail(
    State(state): State<AppState>,
    current: Current,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let principal = current.require()?;
    let tenant_id = principal.require_tenant()?;
    let service = registry::service_view(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;

    if service.tenant_id != tenant_id || service.state == ServiceState::Deleted {
        return Err(AppError::NotFound);
    }
    let record = registry::service_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let tenant = principal.tenant.as_ref().ok_or(AppError::Forbidden)?;
    let can_manage =
        record.tenant_actions_allowed(tenant, state.config.grace_days, OffsetDateTime::now_utc());

    let container_info = if service.kind == ServiceKind::Container {
        let (_, tenant) = owned_service(&state, &principal, id).await?;
        safe_container_text(&state, &principal, &record, &tenant).await
    } else {String::new()};
    let entry = catalog::entry(service.kind);
    let plan = registry::plan_by_id(&state.db, service.plan_id).await?;
    let events = registry::audit_of_tenant(&state.db, tenant_id, 20).await?;
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

    let public_url = layout::public_url(&state.config, &service);
    let has_public_url = public_url.is_some();
    let database_port = if service.kind == ServiceKind::Database {
        serde_json::from_str::<serde_json::Value>(&service.ports_json)
            .ok()
            .and_then(|ports| ports.get("database").and_then(|port| port.as_u64()))
            .map(|port| port.to_string())
            .unwrap_or_else(|| "—".to_string())
    } else {
        String::new()
    };
    let event_rows = audit_rows(&events);
    let has_events = !event_rows.is_empty();

    Ok(render(&ServiceDetailTemplate {
        container_info,
        title: entry.label,
        accent_class: layout::kind_tone(service.kind),
        principal: principal_view(Some(&principal), state.config.theme),
        service_id: service.id,
        service_label: entry.label.to_string(),
        tenant_name: service.tenant_name.clone(),
        unit: service.unit_name.clone(),
        plan_code: service.plan_code.clone(),
        state_label: service.state.label().to_string(),
        state_class: layout::service_state_class(service.state).to_string(),
        lifecycle_label: lifecycle.label().to_string(),
        lifecycle_class: layout::lifecycle_class(lifecycle).to_string(),
        observed: layout::observed_text(&service),
        expires_text,
        days_text,
        created: billing::format_timestamp(service.created_at),
        plan: layout::PlanEnvelope::from_plan(plan.as_ref()),
        has_public_url,
        public_url: public_url.unwrap_or_default(),
        controls: entry
            .controls
            .iter()
            .map(|control| (*control).to_string())
            .collect(),
        is_running: layout::observation_is_active(&service, now, state.config.sample_interval),
        can_manage,
        is_storage: service.kind == ServiceKind::Storage,
        is_web: service.kind == ServiceKind::Web,
        is_database: service.kind == ServiceKind::Database,
        is_git: service.kind == ServiceKind::Git,
        database_port,
        database_name: tenant.slug.clone(),
        tab: "overview",
        csrf: principal.session.csrf_token.clone(),
        has_events,
        events: event_rows,
    })
    .into_response())
}

pub async fn audit_page(State(state): State<AppState>, current: Current) -> AppResult<Response> {
    let principal = current.require()?;
    let tenant_id = principal.require_tenant()?;
    let events = registry::audit_of_tenant(&state.db, tenant_id, 200).await?;
    let rows = audit_rows(&events);
    let has_events = !rows.is_empty();

    Ok(render(&AuditTemplate {
        title: "Actividad",
        accent_class: GOLD,
        principal: principal_view(Some(&principal), state.config.theme),
        has_events,
        events: rows,
    })
    .into_response())
}

#[derive(Debug, Clone)]
pub struct TrustedRow {
    pub name: String,
    pub purpose: String,
    pub reference: String,
    pub registry_url: String,
    pub kind: String,
    pub demo: bool,
    pub note: String,
}

#[derive(Template)]
#[template(path = "tenant/registry.html")]
struct RegistryTemplate {
    title: &'static str,
    accent_class: &'static str,
    principal: PrincipalView,
    tenant_name: String,
    rows: Vec<TrustedRow>,
}

/// The curated trusted-registry index: documentation for tenants, never an
/// authorization surface. Provisioning keeps validating against the live root
/// catalogue.
pub async fn registry_page(State(state): State<AppState>, current: Current) -> AppResult<Response> {
    let principal = current.require()?;
    let tenant_id = principal.require_tenant()?;
    let tenant = registry::tenant_by_id(&state.db, tenant_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let rows = crate::trusted_registry::ENTRIES
        .iter()
        .map(|entry| TrustedRow {
            name: entry.name.to_string(),
            purpose: entry.purpose.to_string(),
            reference: entry.reference.to_string(),
            registry_url: entry.registry_url.to_string(),
            kind: entry.kind.to_string(),
            demo: entry.demo,
            note: entry.note.to_string(),
        })
        .collect();

    Ok(render(&RegistryTemplate {
        title: "Registro de confianza",
        accent_class: GOLD,
        principal: principal_view(Some(&principal), state.config.theme),
        tenant_name: tenant.name.clone(),
        rows,
    })
    .into_response())
}

pub async fn service_action(
    State(state): State<AppState>,
    headers: HeaderMap,
    current: Current,
    Path((id, action)): Path<(i64, String)>,
    Form(form): Form<ActionForm>,
) -> AppResult<Response> {
    let principal = current.require()?;
    let tenant_id = principal.require_tenant()?;
    middleware::check_csrf(&principal, &form.csrf)?;
    let _guard = state.mutations.lock().await;

    let service = registry::service_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    if service.tenant_id != tenant_id {
        return Err(AppError::NotFound);
    }
    let tenant = registry::tenant_by_id(&state.db, tenant_id)
        .await?
        .ok_or(AppError::NotFound)?;

    // Deletion is cleanup, not a lifecycle action: it must stay available for
    // failed, suspended or expired services the tenant owns.
    let deleting = action == "delete";
    if deleting {
        if service.deleted_at.is_some() || service.state == ServiceState::Deleted {
            return Err(AppError::NotFound);
        }
    } else if !service.tenant_actions_allowed(
        &tenant,
        state.config.grace_days,
        OffsetDateTime::now_utc(),
    ) {
        return Err(AppError::Forbidden);
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
        Some(tenant_id),
        request.op(),
        &serde_json::json!({ "unit": service.unit_name, "correlation": broker_context })
            .to_string(),
        Some(principal.account.id),
    )
    .await?;
    registry::start_job(&state.db, job).await?;

    let target = match action.as_str() {
        "stop" => ServiceState::Stopped,
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
                tenant_id: Some(tenant_id),
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
    let target = if deleting {
        "/portal".to_string()
    } else {
        format!("/portal/services/{}", service.id)
    };
    Ok(Redirect::to(&target).into_response())
}

/// Approved image catalogue: the primary deploy screen. References come from the
/// live root policy; upstream links are documentation, never image imports.
pub async fn images_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    current: Current,
) -> AppResult<Response> {
    let principal = current.require()?;
    let tenant = catalogue_tenant(&state, &principal).await?;
    let context = audit::Context::from_headers(&headers).broker_context(&principal)?;
    let (entries, catalog_unavailable) = annotated_catalog(&state, &tenant, &context).await?;
    // Plans no longer segregate service kinds, so the catalogue is deduplicated
    // to one card per kind. The deploy action uses the tenant's subscription
    // plan when it allows the kind, otherwise the first entitled plan.
    let subscription_plan = crate::topup::subscription_plan_code(&state.db, tenant.id).await?;
    let mut by_kind: std::collections::BTreeMap<String, &CatalogEntry> =
        std::collections::BTreeMap::new();
    for entry in &entries {
        for kind_name in &entry.allowed_kinds {
            let preferred = subscription_plan.as_deref() == Some(entry.plan_code.as_str());
            by_kind
                .entry(kind_name.clone())
                .and_modify(|chosen| {
                    if !entry.can_provision {
                        return;
                    }
                    // Prefer the subscription plan when it is available; without
                    // one, prefer the available plan with the most services. An
                    // unavailable plan must never hide another plan that can
                    // provision the same kind.
                    let chosen_preferred =
                        subscription_plan.as_deref() == Some(chosen.plan_code.as_str());
                    if !chosen.can_provision
                        || (preferred && !chosen_preferred)
                        || (subscription_plan.is_none()
                            && entry.max_services > chosen.max_services)
                    {
                        *chosen = entry;
                    }
                })
                .or_insert(entry);
        }
    }
    let mut rows = Vec::new();
    for (kind_name, entry) in &by_kind {
        let Some(kind) = ServiceKind::parse(kind_name) else {
            continue;
        };
        // Generic containers have their own wizard; every fixed template
        // (web, database, storage, git) is offered from this catalogue.
        if matches!(kind, ServiceKind::Container) {
            continue;
        }
        let Some(reference) = entry
            .images
            .get(kind_name)
            .and_then(|image| image.clone())
            .filter(|image| !image.is_empty())
        else {
            continue;
        };
        let parsed = ImageReference::parse(&reference);
        let registry_url = parsed.as_ref().and_then(ImageReference::registry_url);
        let can_deploy = parsed.is_some() && entry.can_provision;
        rows.push(ImageRow {
            plan_code: entry.plan_code.clone(),
            kind: kind_name.clone(),
            kind_label: catalog::label(kind).to_string(),
            tone: layout::kind_tone(kind),
            repository: parsed
                .as_ref()
                .map(ImageReference::repository_display)
                .unwrap_or_default(),
            tag: parsed
                .as_ref()
                .and_then(|image| image.tag.clone())
                .unwrap_or_else(|| "—".to_string()),
            digest: parsed
                .as_ref()
                .and_then(|image| image.digest.clone())
                .unwrap_or_else(|| "—".to_string()),
            has_registry_url: registry_url.is_some(),
            registry_url: registry_url.unwrap_or_default(),
            can_deploy,
            deploy_url: if can_deploy {
                format!("/portal/deploy?plan_code={}&kind={kind_name}", entry.plan_code)
            } else {
                String::new()
            },
            archived: kind == ServiceKind::Storage,
            reference,
        });
    }
    let has_rows = !rows.is_empty();
    Ok(render(&ImagesTemplate {
        title: "Imágenes",
        accent_class: GOLD,
        principal: principal_view(Some(&principal), state.config.theme),
        tenant_name: tenant.name,
        registries: entries.iter().flat_map(|e| e.registries.clone()).collect::<std::collections::BTreeSet<_>>().into_iter().collect(),
        rows,
        has_rows,
        catalog_unavailable,
    })
    .into_response())
}

pub async fn deploy_form(
    State(state): State<AppState>,
    headers: HeaderMap,
    current: Current,
    Query(query): Query<DeployQuery>,
) -> AppResult<Response> {
    let principal = current.require()?;
    let tenant = catalogue_tenant(&state, &principal).await?;
    let kind = ServiceKind::parse(&query.kind)
        .filter(|kind| !matches!(kind, ServiceKind::Container))
        .ok_or_else(|| AppError::BadRequest("tipo de servicio desconocido".to_string()))?;
    let context = audit::Context::from_headers(&headers).broker_context(&principal)?;
    let (entries, unavailable) = annotated_catalog(&state, &tenant, &context).await?;
    if unavailable {
        return Err(AppError::Broker(
            "el catálogo autorizado no está disponible; inténtalo de nuevo más tarde".to_string(),
        ));
    }
    let entry = crate::portal_catalog::select(&entries, &query.plan_code, kind)?;
    let template = deploy_view(
        &state,
        &principal,
        &tenant,
        entry,
        kind,
        None,
        String::new(),
        false,
    )
    .await?;
    Ok(render(&template).into_response())
}

#[allow(clippy::too_many_arguments)]
async fn deploy_view(
    state: &AppState,
    principal: &crate::models::Principal,
    tenant: &Tenant,
    entry: &CatalogEntry,
    kind: ServiceKind,
    error: Option<String>,
    username: String,
    read_only: bool,
) -> AppResult<DeployTemplate> {
    let image = entry
        .images
        .get(kind.as_str())
        .and_then(|image| image.clone())
        .filter(|image| !image.is_empty())
        .ok_or_else(|| {
            AppError::BadRequest("este tipo no tiene una imagen aprobada en el catálogo".to_string())
        })?;
    let parsed = ImageReference::parse(&image).ok_or_else(|| {
        AppError::BadRequest(
            "la imagen aprobada no es una referencia de registro válida; avisa al operador"
                .to_string(),
        )
    })?;
    let (_instance, unit, ports, volumes) =
        crate::provisioning::next_allocation(state, tenant, kind).await?;
    let registry_url = parsed.registry_url();
    Ok(DeployTemplate {
        title: "Desplegar",
        accent_class: layout::kind_tone(kind),
        principal: principal_view(Some(principal), state.config.theme),
        csrf: principal.session.csrf_token.clone(),
        tenant_name: tenant.name.clone(),
        plan_code: entry.plan_code.clone(),
        kind: kind.as_str().to_string(),
        kind_label: catalog::label(kind).to_string(),
        repository: parsed.repository_display(),
        tag: parsed.tag.clone().unwrap_or_else(|| "—".to_string()),
        digest: parsed.digest.clone().unwrap_or_else(|| "—".to_string()),
        has_registry_url: registry_url.is_some(),
        registry_url: registry_url.unwrap_or_default(),
        memory_high: entry.memory_high.clone(),
        memory_max: entry.memory_max.clone(),
        cpu_quota_percent: entry.cpu_quota_percent,
        tasks_max: entry.tasks_max,
        service_days: entry.service_days,
        unit,
        ports: key_values(&ports),
        volumes: key_values(&volumes),
        is_storage: kind == ServiceKind::Storage,
        is_database: kind == ServiceKind::Database,
        is_git: kind == ServiceKind::Git,
        database_name: tenant.slug.clone(),
        archived: kind == ServiceKind::Storage,
        can_provision: entry.can_provision,
        has_error: error.is_some(),
        error: error.unwrap_or_default(),
        username,
        read_only,
        image,
    })
}

fn key_values(encoded: &str) -> Vec<KeyValue> {
    serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(encoded)
        .map(|map| {
            map.into_iter()
                .map(|(key, value)| KeyValue {
                    key,
                    value: match value {
                        serde_json::Value::String(text) => text,
                        other => other.to_string(),
                    },
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Ownership, tenancy, and expiry are enforced before any broker call. Read-only
/// inspection works for failed services too; mutations add their own state checks.
async fn owned_service(
    state: &AppState,
    principal: &crate::models::Principal,
    id: i64,
) -> AppResult<(Service, Tenant)> {
    let tenant_id = principal.require_tenant()?;
    let service = registry::service_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    if service.tenant_id != tenant_id
        || service.deleted_at.is_some()
        || service.state == ServiceState::Deleted
    {
        return Err(AppError::NotFound);
    }
    let tenant = registry::tenant_by_id(&state.db, tenant_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let now = OffsetDateTime::now_utc();
    let current = matches!(tenant.status, TenantStatus::Active | TenantStatus::Grace)
        && service
            .expires_at
            .map(|expiry| now <= expiry + time::Duration::days(state.config.grace_days))
            .unwrap_or(true);
    if !current {
        return Err(AppError::Forbidden);
    }
    Ok((service, tenant))
}

fn configuration_view(
    state: &AppState,
    principal: &crate::models::Principal,
    service: &Service,
    tenant_name: &str,
    configuration: &ServiceConfiguration,
    notice: Option<(&'static str, String)>,
) -> ConfigurationTemplate {
    let is_storage = service.kind == ServiceKind::Storage;
    let stopped = service.state == ServiceState::Stopped;
    ConfigurationTemplate {
        title: "Configuración",
        accent_class: layout::kind_tone(service.kind),
        principal: principal_view(Some(principal), state.config.theme),
        csrf: principal.session.csrf_token.clone(),
        service_id: service.id,
        tab: "configuration",
        service_label: catalog::label(service.kind).to_string(),
        tenant_name: tenant_name.to_string(),
        unit: service.unit_name.clone(),
        state_label: service.state.label().to_string(),
        state_class: layout::service_state_class(service.state).to_string(),
        kind: configuration.kind.clone(),
        image: configuration.image.clone(),
        is_storage,
        is_web: service.kind == ServiceKind::Web,
        username: configuration.username.clone().unwrap_or_default(),
        read_only: configuration.read_only.unwrap_or(false),
        stopped,
        can_configure: is_storage && stopped,
        has_notice: notice.is_some(),
        notice_class: notice.as_ref().map(|(class, _)| *class).unwrap_or(""),
        notice: notice.map(|(_, message)| message).unwrap_or_default(),
    }
}

pub async fn service_configuration(
    State(state): State<AppState>,
    headers: HeaderMap,
    current: Current,
    Path(id): Path<i64>,
    Query(query): Query<ConfigurationQuery>,
) -> AppResult<Response> {
    let principal = current.require()?;
    let (service, tenant) = owned_service(&state, &principal, id).await?;
    if service.kind == ServiceKind::Container {
        let info = safe_container_text(&state, &principal, &service, &tenant).await;
        return Ok(render(&ContainerInspectTemplate {title:"Configuración",accent_class:GOLD,principal:principal_view(Some(&principal),state.config.theme),service_id:id,service_label:"Contenedor",tenant_name:tenant.name.clone(),unit:service.unit_name.clone(),tab:"configuration",info,is_web:false}).into_response());
    }
    let context = audit::Context::from_headers(&headers);
    let broker_context = context.broker_context(&principal)?;
    let configuration = match state
        .broker
        .inspect(&tenant.slug, &service.unit_name, &broker_context)
        .await
    {
        Ok(configuration) => configuration,
        Err(error) => {
            // A failed or removed unit must not turn the tab into a 500.
            let info = format!(
                "La configuración no está disponible para este servicio: {error}"
            );
            return Ok(render(&ContainerInspectTemplate {
                title: "Configuración",
                accent_class: layout::kind_tone(service.kind),
                principal: principal_view(Some(&principal), state.config.theme),
                service_id: id,
                service_label: catalog::label(service.kind),
                tenant_name: tenant.name.clone(),
                unit: service.unit_name.clone(),
                tab: "configuration",
                info,
                is_web: service.kind == ServiceKind::Web,
            })
            .into_response());
        }
    };
    configuration.validate(service.kind)?;
    let notice = (query.updated.as_deref() == Some("1")).then(|| {
        (
            "ok",
            "Configuración guardada. Inicia el servicio para volver a usarlo.".to_string(),
        )
    });
    Ok(render(&configuration_view(
        &state,
        &principal,
        &service,
        &tenant.name,
        &configuration,
        notice,
    ))
    .into_response())
}

/// Storage-only mutable setting. The username always comes from a fresh inspect,
/// never from the form, and the password stays empty: no resets or rotation.
pub async fn configure_service(
    State(state): State<AppState>,
    headers: HeaderMap,
    current: Current,
    Path(id): Path<i64>,
    Form(form): Form<ConfigureForm>,
) -> AppResult<Response> {
    let principal = current.require()?;
    middleware::check_csrf(&principal, &form.csrf)?;
    let _guard = state.mutations.lock().await;
    let (service, tenant) = owned_service(&state, &principal, id).await?;
    if service.kind != ServiceKind::Storage {
        return Err(AppError::BadRequest(
            "solo el almacenamiento tiene ajustes configurables desde el panel".to_string(),
        ));
    }
    if service.state != ServiceState::Stopped {
        return Err(AppError::Conflict(
            "detén el servicio antes de configurarlo y vuelve a iniciarlo después".to_string(),
        ));
    }
    let read_only = service_options::checkbox(form.read_only.as_deref())?;
    let context = audit::Context::from_headers(&headers);
    let broker_context = context.broker_context(&principal)?;
    let inspected = state
        .broker
        .inspect(&tenant.slug, &service.unit_name, &broker_context)
        .await?;
    inspected.validate(service.kind)?;
    let username = inspected.username.clone().ok_or_else(|| {
        AppError::Broker("storage configuration missing username".to_string())
    })?;
    let options = ServiceOptions {
        image: None,
        file_manager: Some(FileManagerOptions {
            username,
            password: Secret::new(""),
            read_only,
        }),
        database: None,
    };
    let job = registry::create_job(
        &state.db,
        Some(tenant.id),
        "service.configure",
        &serde_json::json!({
            "unit": service.unit_name,
            "read_only": read_only,
            "correlation": broker_context,
        })
        .to_string(),
        Some(principal.account.id),
    )
    .await?;
    registry::start_job(&state.db, job).await?;
    let outcome = state
        .broker
        .call_ok_with_context(
            &broker::Request::ServiceConfigure {
                tenant_id: tenant.slug.clone(),
                service: service.unit_name.clone(),
                options,
            },
            &broker_context,
        )
        .await
        .and_then(|response| {
            if let Some(configuration) = response.configuration {
                configuration.validate(service.kind)?;
            }
            Ok(())
        });
    match &outcome {
        Ok(()) => registry::finish_job(&state.db, job, JobState::Done, None).await?,
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
                action: "service.configure",
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
    match outcome {
        Ok(()) => Ok(Redirect::to(&format!(
            "/portal/services/{}/configuration?updated=1",
            service.id
        ))
        .into_response()),
        Err(error) => {
            // The failed job and audit row persist; show the committed settings again.
            let configuration = state
                .broker
                .inspect(&tenant.slug, &service.unit_name, &broker_context)
                .await
                .unwrap_or(inspected);
            let response = render(&configuration_view(
                &state,
                &principal,
                &service,
                &tenant.name,
                &configuration,
                Some(("danger", error.to_string())),
            ))?;
            Ok((StatusCode::BAD_GATEWAY, response).into_response())
        }
    }
}

/// Bounded, tenant-scoped journal excerpt. No unit, line count, or host path is
/// tenant-selectable, and the output is escaped by the template.
pub async fn service_logs(
    State(state): State<AppState>,
    headers: HeaderMap,
    current: Current,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    const LINES: i64 = 500;
    const MAX_CHARS: usize = 400_000;
    let principal = current.require()?;
    let (service, tenant) = owned_service(&state, &principal, id).await?;
    let context = audit::Context::from_headers(&headers);
    let broker_context = context.broker_context(&principal)?;
    let mut logs = match state
        .broker
        .call_ok_with_context(
            &broker::Request::LogsFetch {
                tenant_id: tenant.slug.clone(),
                service: service.unit_name.clone(),
                lines: LINES,
            },
            &broker_context,
        )
        .await
    {
        Ok(response) => response.detail.unwrap_or_default(),
        // A failed or removed unit must not turn the tab into a 500.
        Err(error) => format!("No se pudieron obtener los registros: {error}"),
    };
    if logs.len() > MAX_CHARS {
        let mut boundary = MAX_CHARS;
        while !logs.is_char_boundary(boundary) {
            boundary -= 1;
        }
        logs.truncate(boundary);
        logs.push_str("\n… salida truncada por el panel …");
    }
    Ok(render(&LogsTemplate {
        title: "Registros",
        accent_class: layout::kind_tone(service.kind),
        principal: principal_view(Some(&principal), state.config.theme),
        service_id: service.id,
        tab: "logs",
        service_label: catalog::label(service.kind).to_string(),
        tenant_name: tenant.name.clone(),
        unit: service.unit_name.clone(),
        is_web: service.kind == ServiceKind::Web,
        lines: LINES,
        logs,
    })
    .into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SiteForm {
    csrf: String,
    site_path: String,
    site_content: String,
}

#[derive(Deserialize)]
pub struct SiteQuery {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    updated: Option<String>,
}

#[derive(Template)]
#[template(path = "tenant/service_site.html")]
struct SiteTemplate {
    title: &'static str,
    accent_class: &'static str,
    principal: PrincipalView,
    service_id: i64,
    tab: &'static str,
    service_label: String,
    tenant_name: String,
    unit: String,
    is_web: bool,
    site_path: String,
    site_content: String,
    has_notice: bool,
    notice: String,
}

pub async fn site_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    current: Current,
    Path(id): Path<i64>,
    Query(query): Query<SiteQuery>,
) -> AppResult<Response> {
    let principal = current.require()?;
    let (service, tenant) = owned_service(&state, &principal, id).await?;
    if service.kind != ServiceKind::Web {
        return Err(AppError::NotFound);
    }
    let path = query
        .path
        .filter(|path| !path.is_empty())
        .unwrap_or_else(|| "index.html".to_string());
    let context = audit::Context::from_headers(&headers);
    let broker_context = context.broker_context(&principal)?;
    let (content, mut has_notice, mut notice) = match state
        .broker
        .call_ok_with_context(
            &broker::Request::SiteRead {
                tenant_id: tenant.slug.clone(),
                service: service.unit_name.clone(),
                site_path: path.clone(),
            },
            &broker_context,
        )
        .await
    {
        Ok(response) => (
            response.site.map(|site| site.content).unwrap_or_default(),
            false,
            String::new(),
        ),
        Err(_) => (
            String::new(),
            true,
            "El archivo aún no existe. Escribe su contenido y guarda para crearlo."
                .to_string(),
        ),
    };
    if query.updated.as_deref() == Some("1") {
        has_notice = true;
        notice = "Contenido guardado. Recarga la URL del servicio para verlo.".to_string();
    }
    Ok(render(&SiteTemplate {
        title: "Contenido",
        accent_class: layout::kind_tone(service.kind),
        principal: principal_view(Some(&principal), state.config.theme),
        service_id: service.id,
        tab: "site",
        service_label: catalog::label(service.kind).to_string(),
        tenant_name: tenant.name.clone(),
        unit: service.unit_name.clone(),
        is_web: true,
        site_path: path,
        site_content: content,
        has_notice,
        notice,
    })
    .into_response())
}

pub async fn site_write(
    State(state): State<AppState>,
    headers: HeaderMap,
    current: Current,
    Path(id): Path<i64>,
    Form(form): Form<SiteForm>,
) -> AppResult<Response> {
    let principal = current.require()?;
    middleware::check_csrf(&principal, &form.csrf)?;
    let (service, tenant) = owned_service(&state, &principal, id).await?;
    if service.kind != ServiceKind::Web {
        return Err(AppError::NotFound);
    }
    let context = audit::Context::from_headers(&headers);
    let broker_context = context.broker_context(&principal)?;
    state
        .broker
        .call_ok_with_context(
            &broker::Request::SiteWrite {
                tenant_id: tenant.slug.clone(),
                service: service.unit_name.clone(),
                site_path: form.site_path.clone(),
                site_content: form.site_content,
            },
            &broker_context,
        )
        .await?;
    Ok(Redirect::to(&format!(
        "/portal/services/{id}/site?updated=1&path={}",
        form.site_path
    ))
    .into_response())
}

#[derive(Template)]
#[template(path="tenant/container_inspect.html")]
struct ContainerInspectTemplate {
    title: &'static str, accent_class: &'static str, principal: PrincipalView,
    service_id:i64, service_label:&'static str, tenant_name:String, unit:String, tab:&'static str, info:String,
    is_web: bool,
}
async fn safe_container_text(state:&AppState, principal:&crate::models::Principal, service:&Service, tenant:&Tenant) -> String {
    let result = async {
        let context = broker::CallContext::for_principal(&crate::auth::generate_token()?,principal)?;
        let info = state.broker.inspect_container(&tenant.slug,&service.unit_name,&context).await?;
        serde_json::to_string_pretty(&info).map_err(|_| AppError::Broker("invalid safe info".into()))
    }.await;
    result.unwrap_or_else(|_:AppError| "Inspección no disponible: la unidad del servicio no existe o la creación falló. Revisa Registros o vuelve a desplegar el servicio.".into())
}
