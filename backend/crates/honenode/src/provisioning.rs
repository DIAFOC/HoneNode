use time::OffsetDateTime;

use crate::audit::{self, Event};
use crate::broker;
use crate::error::{AppError, AppResult};
use crate::models::{JobState, Principal, Service, ServiceKind, ServiceState, TenantStatus};
use crate::service_options::{self, ServiceOptions};
use crate::{catalog, portal_catalog, registry, state::AppState, topup};

/// Metadata matches the broker's fixed allocator; callers cannot supply any host inputs.
/// Instance 1 keeps the legacy `{slug}-{suffix}` name and tenant root; later
/// instances append `-2`, `-3`, ... and live below `{root}/{data_suffix}-n`.
pub fn allocation(
    slug: &str,
    kind: ServiceKind,
    root: &std::path::Path,
    instance: u32,
    port: Option<u16>,
) -> AppResult<(String, String, String)> {
    let number = catalog::tenant_number(slug)
        .filter(|number| (1..=999).contains(number))
        .ok_or_else(|| AppError::BadRequest("tenant outside safe fixed-port range".to_string()))?;
    if instance == 0 || instance > 99 {
        return Err(AppError::BadRequest("instancia fuera de rango".to_string()));
    }
    let unit_suffix = match kind {
        ServiceKind::Web => "site",
        ServiceKind::Database => "db",
        ServiceKind::Storage => "vault",
        ServiceKind::Git => "git",
        ServiceKind::Container => return Err(AppError::BadRequest("usa /portal/containers/new".into())),
    };
    let data_suffix = match kind {
        ServiceKind::Web => "site",
        ServiceKind::Database => "db",
        ServiceKind::Storage => "vault",
        ServiceKind::Git => "gitea",
        ServiceKind::Container => return Err(AppError::BadRequest("usa /portal/containers/new".into())),
    };
    let offset = (instance - 1) * 100;
    let default = |base: u32| base + number + offset;
    let port = |base: u32| port.map(u32::from).unwrap_or_else(|| default(base));
    let ports = match kind {
        ServiceKind::Container => unreachable!("generic containers have no fixed allocation"),
        ServiceKind::Web => serde_json::json!({"web": port(18_000)}),
        ServiceKind::Database => serde_json::json!({"database": port(15_430)}),
        ServiceKind::Storage => serde_json::json!({"web": port(19_000)}),
        ServiceKind::Git => serde_json::json!({"web": port(20_000), "git_ssh": port(20_000) + 2_000}),
    };
    let base = if instance == 1 {
        root.join(slug)
    } else {
        root.join(slug).join(format!("{data_suffix}-{instance}"))
    };
    let path = |name: &str| base.join(name).display().to_string();
    let volumes = match kind {
        ServiceKind::Container => unreachable!("generic containers have no host paths"),
        ServiceKind::Web => serde_json::json!({"site": path("site")}),
        ServiceKind::Database => serde_json::json!({"pgdata": path("pgdata")}),
        ServiceKind::Storage => serde_json::json!({"vault": path("vault"), "database": path("db")}),
        ServiceKind::Git => {
            serde_json::json!({"gitea": path("gitea"), "config": path("gitea-config")})
        }
    };
    let unit = if instance == 1 {
        format!("{slug}-{unit_suffix}")
    } else {
        format!("{slug}-{unit_suffix}-{instance}")
    };
    Ok((unit, ports.to_string(), volumes.to_string()))
}

/// Primary loopback port the broker publishes for a fixed kind.
pub fn primary_port(kind: ServiceKind, ports: &str) -> Option<u16> {
    let value: serde_json::Value = serde_json::from_str(ports).ok()?;
    let key = if kind == ServiceKind::Database { "database" } else { "web" };
    value.get(key)?.as_u64().and_then(|port| u16::try_from(port).ok())
}

/// Next free instance for this tenant and kind, using the same allocator as the
/// broker so a second web or database does not collide with the first.
pub async fn next_allocation(
    state: &AppState,
    tenant: &crate::models::Tenant,
    kind: ServiceKind,
) -> AppResult<(u32, String, String, String)> {
    let used = registry::live_allocations(&state.db, tenant.id).await?;
    for instance in 1..=99 {
        let (unit, ports, volumes) = allocation(
            &tenant.slug,
            kind,
            &state.config.tenant_data_root,
            instance,
            None,
        )?;
        let port = primary_port(kind, &ports);
        if !used.units.contains(&unit) && port.is_none_or(|port| !used.ports.contains(&port)) {
            return Ok((instance, unit, ports, volumes));
        }
    }
    Err(AppError::Conflict(
        "no quedan instancias libres para este tipo de servicio".to_string(),
    ))
}

/// The only provisioning path for both web roles. Root policy remains the final authority.
/// Legacy web/database forms carry no options; storage needs the deploy wizard.
pub async fn provision(
    state: &AppState,
    principal: &Principal,
    tenant_id: i64,
    plan_code: &str,
    kind: ServiceKind,
    context: &audit::Context,
) -> AppResult<Service> {
    provision_with_options(state, principal, tenant_id, plan_code, kind, None, context).await
}

/// Form entry point: options are validated against the freshly loaded root
/// catalogue before they reach `limits.set`, and only their non-secret summary
/// is persisted in jobs and audit rows.
pub async fn provision_with_options(
    state: &AppState,
    principal: &Principal,
    tenant_id: i64,
    plan_code: &str,
    kind: ServiceKind,
    options: Option<ServiceOptions>,
    context: &audit::Context,
) -> AppResult<Service> {
    if !principal.account.is_enabled()
        || (!principal.role().is_operator() && principal.require_tenant()? != tenant_id)
    {
        return Err(AppError::Forbidden);
    }
    let _guard = state.mutations.lock().await;
    let tenant = registry::tenant_by_id(&state.db, tenant_id)
        .await?
        .ok_or(AppError::NotFound)?;
    if tenant.status != TenantStatus::Active {
        return Err(AppError::Forbidden);
    }
    let broker_context = context.broker_context(principal)?;
    let entries = portal_catalog::load(&state.broker, &tenant.slug, &broker_context).await?;
    let entry = portal_catalog::select(&entries, plan_code, kind)?;
    validate_options(&entry, kind, options.as_ref())?;
    let (_instance, unit, ports, volumes) = next_allocation(state, &tenant, kind).await?;
    let port = primary_port(kind, &ports);

    // Local plans are display/reference metadata, never an authorization source.
    let plan_id: i64 = sqlx::query_scalar(
        "INSERT INTO plans (code, monthly_price, cpu_weight, memory_high, memory_max, tasks_max,
                            disk_gb, bandwidth_gb, max_services, description)
         VALUES (?, 0, 100, ?, ?, ?, 0, 0, ?, ?)
         ON CONFLICT(code) DO UPDATE SET memory_high=excluded.memory_high,
             memory_max=excluded.memory_max, tasks_max=excluded.tasks_max,
             max_services=excluded.max_services
         RETURNING id",
    )
    .bind(&entry.plan_code)
    .bind(&entry.memory_high)
    .bind(&entry.memory_max)
    .bind(entry.tasks_max)
    .bind(entry.max_services)
    .bind(&entry.label)
    .fetch_one(&state.db)
    .await?;
    let expires = OffsetDateTime::now_utc() + time::Duration::days(entry.service_days);
    let expires = subscription_cap(&state.db, tenant.id, expires).await?;
    let service = registry::reserve_service(
        &state.db,
        tenant.id,
        kind,
        plan_id,
        &unit,
        None,
        &ports,
        &volumes,
        expires,
        entry.max_services,
    )
    .await?;
    let mut payload = serde_json::json!({"unit": unit, "kind": kind.as_str(), "catalog": entry, "correlation": broker_context});
    if let Some(options) = options.as_ref() {
        payload["options"] = options.audit_summary();
    }
    let job = registry::create_job(
        &state.db, Some(tenant.id), "service.provision",
        &payload.to_string(),
        Some(principal.account.id),
    ).await?;
    registry::start_job(&state.db, job).await?;
    let outcome = sequence(
        state,
        &tenant.slug,
        &service,
        plan_code,
        options,
        port,
        &broker_context,
    )
    .await;
    match &outcome {
        Ok(_) => registry::finish_job(&state.db, job, JobState::Done, None).await?,
        Err(error) => {
            registry::finish_job(&state.db, job, JobState::Failed, Some(&error.to_string()))
                .await?;
            registry::set_service_state(&state.db, service.id, ServiceState::Failed, None).await?;
        }
    }
    context
        .record(
            &state.db,
            Event {
                tenant_id: Some(tenant.id),
                actor_type: "account",
                actor_id: Some(&principal.account.number),
                action: "service.provision",
                resource_type: Some("service"),
                resource_id: Some(&unit),
                outcome: if outcome.is_ok() { "ok" } else { "failed" },
                reason: outcome
                    .as_ref()
                    .err()
                    .map(|error| error.to_string())
                    .as_deref(),
            },
        )
        .await?;
    // Once reserved, broker failure is a persisted service outcome, not a lost form submission.
    // Return the failed record so callers can redirect to its status and audit trail.
    registry::service_by_id(&state.db, service.id)
        .await?
        .ok_or(AppError::NotFound)
}

/// Generic provisioning never uses the template allocator or persists the request.
pub async fn provision_container(
    state: &AppState, principal: &Principal, tenant_id: i64, plan_code: &str,
    name: &str, spec: crate::containers::ContainerSpec, context: &audit::Context,
) -> AppResult<Service> {
    if principal.require_tenant()? != tenant_id { return Err(AppError::Forbidden); }
    let _guard = state.mutations.lock().await;
    let tenant = registry::tenant_by_id(&state.db, tenant_id).await?.ok_or(AppError::NotFound)?;
    if tenant.status != TenantStatus::Active { return Err(AppError::Forbidden); }
    let unit = crate::containers::unit_name(&tenant.slug, name)?;
    let broker_context = context.broker_context(principal)?;
    let entries = portal_catalog::load(&state.broker, &tenant.slug, &broker_context).await
        .map_err(|_| AppError::Broker("catálogo autorizado no disponible".into()))?;
    let entry = portal_catalog::select(&entries, plan_code, ServiceKind::Container)?;
    spec.validate(&entry.registries)?;
    let plan_id: i64 = sqlx::query_scalar(
        "INSERT INTO plans (code, monthly_price, cpu_weight, memory_high, memory_max, tasks_max,
                           disk_gb, bandwidth_gb, max_services, description)
         VALUES (?, 0, 100, ?, ?, ?, 0, 0, ?, ?)
         ON CONFLICT(code) DO UPDATE SET memory_high=excluded.memory_high, memory_max=excluded.memory_max,
             tasks_max=excluded.tasks_max, max_services=excluded.max_services RETURNING id")
        .bind(&entry.plan_code).bind(&entry.memory_high).bind(&entry.memory_max).bind(entry.tasks_max)
        .bind(entry.max_services).bind(&entry.label).fetch_one(&state.db).await?;
    let service = registry::reserve_service(&state.db, tenant.id, ServiceKind::Container, plan_id, &unit,
        None, "{}", "{}", subscription_cap(&state.db, tenant.id,
            OffsetDateTime::now_utc()+time::Duration::days(entry.service_days)).await?, entry.max_services).await?;
    let payload = serde_json::json!({"unit":unit,"container":spec.summary(),"correlation":broker_context});
    let job = registry::create_job(&state.db, Some(tenant.id), "container.provision", &payload.to_string(), Some(principal.account.id)).await?;
    registry::start_job(&state.db, job).await?;
    let outcome: AppResult<()> = async {
        state.broker.call_ok_with_context(&broker::Request::TenantCreate {tenant_id:tenant.slug.clone()}, &broker_context).await?;
        let response = state.broker.call_ok_with_context(&broker::Request::ContainerCreate {
            tenant_id:tenant.slug.clone(),service:unit.clone(),plan_code:plan_code.into(),container:spec,
        }, &broker_context).await?;
        let info = response.container.ok_or_else(|| AppError::Broker("missing safe container info".into()))?;
        info.validate(&unit)?;
        // Only the broker's safe projection is retained; no engine paths or env values.
        sqlx::query("UPDATE services SET ports_json = ?, volume_json = ? WHERE id = ?")
            .bind(serde_json::json!({"web":info.host_port}).to_string())
            .bind(serde_json::json!({"container":info}).to_string()).bind(service.id).execute(&state.db).await?;
        // container.create queues the initial rootless pull/start; do not start twice.
        let _ = crate::observation::refresh(state, &service, &tenant.slug, Some(&broker_context)).await;
        Ok(())
    }.await;
    // The broker's container errors are already sanitized (no image values,
    // environment values or host paths); surface them so the tenant can act.
    let reason = outcome.as_ref().err().map(|error| error.to_string());
    if outcome.is_err() {
        registry::set_service_state(&state.db, service.id, ServiceState::Failed, None).await?;
    }
    registry::finish_job(&state.db, job, if outcome.is_ok() {JobState::Done} else {JobState::Failed}, reason.as_deref()).await?;
    context.record(&state.db, Event {tenant_id:Some(tenant.id),actor_type:"account",actor_id:Some(&principal.account.number),
        action:"container.provision",resource_type:Some("service"),resource_id:Some(&unit),
        outcome:if outcome.is_ok() {"ok"} else {"failed"},reason:reason.as_deref()}).await?;
    registry::service_by_id(&state.db, service.id).await?.ok_or(AppError::NotFound)
}

/// Pure pre-reservation checks; the broker repeats them as the final authority.
fn validate_options(
    entry: &portal_catalog::CatalogEntry,
    kind: ServiceKind,
    options: Option<&ServiceOptions>,
) -> AppResult<()> {
    let approved = entry
        .images
        .get(kind.as_str())
        .and_then(|image| image.as_deref())
        .filter(|image| !image.is_empty());
    if let Some(image) = options.and_then(|options| options.image.as_deref()) {
        if Some(image) != approved {
            return Err(AppError::BadRequest(
                "la imagen debe coincidir exactamente con la imagen aprobada por el catálogo autorizado"
                    .to_string(),
            ));
        }
    }
    let file_manager = options.and_then(|options| options.file_manager.as_ref());
    match kind {
        ServiceKind::Storage => {
            let file_manager = file_manager.ok_or_else(|| {
                AppError::BadRequest(
                    "el almacenamiento requiere usuario y contraseña iniciales del gestor de archivos; usa el asistente de despliegue"
                        .to_string(),
                )
            })?;
            service_options::validate_username(&file_manager.username)?;
            if file_manager.password.is_empty() {
                return Err(AppError::BadRequest(
                    "la contraseña inicial del gestor de archivos no puede estar vacía".to_string(),
                ));
            }
            service_options::validate_new_password(&file_manager.password)?;
        }
        _ => {
            if file_manager.is_some() {
                return Err(AppError::BadRequest(
                    "las credenciales del gestor de archivos solo aplican al almacenamiento"
                        .to_string(),
                ));
            }
        }
    }
    Ok(())
}

/// A service never outlives the plan that pays for it: with a subscription the
/// expiry is capped at `paid_through`, and an expired plan refuses new services.
pub(crate) async fn subscription_cap(
    pool: &sqlx::SqlitePool,
    tenant_id: i64,
    expires: OffsetDateTime,
) -> AppResult<OffsetDateTime> {
    let Some(paid_through) = topup::subscription_paid_through(pool, tenant_id).await? else {
        return Ok(expires);
    };
    if paid_through <= OffsetDateTime::now_utc() {
        return Err(AppError::BadRequest(
            "el plan expiró; canjea una clave de recarga para continuar".to_string(),
        ));
    }
    Ok(expires.min(paid_through))
}

async fn sequence(
    state: &AppState,
    slug: &str,
    service: &Service,
    plan_code: &str,
    options: Option<ServiceOptions>,
    port: Option<u16>,
    context: &broker::CallContext,
) -> AppResult<()> {
    state
        .broker
        .call_ok_with_context(
            &broker::Request::TenantCreate {
                tenant_id: slug.to_string(),
            },
            context,
        )
        .await?;
    state
        .broker
        .call_ok_with_context(
            &broker::Request::LimitsSet {
                tenant_id: slug.to_string(),
                service: service.unit_name.clone(),
                kind: service.kind.as_str().to_string(),
                plan_code: plan_code.to_string(),
                options,
                port,
            },
            context,
        )
        .await?;
    crate::observation::execute(
        state,
        service,
        slug,
        &broker::Request::ServiceStart {
            tenant_id: slug.to_string(),
            service: service.unit_name.clone(),
        },
        ServiceState::Running,
        Some(context),
    )
    .await
}
