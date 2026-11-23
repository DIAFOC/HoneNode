use sqlx::SqlitePool;
use time::OffsetDateTime;
use tokio::task::JoinHandle;

use crate::broker;
use crate::error::AppResult;
use crate::models::{ServiceState, TenantStatus};
use crate::registry;
use crate::state::AppState;
use crate::telemetry;

pub fn spawn(state: AppState) -> Vec<JoinHandle<()>> {
    vec![
        sweeper(state.clone()),
        reconciler(state.clone()),
        sampler(state),
    ]
}

fn sweeper(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(state.config.sweep_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if let Err(error) = sweep(&state).await {
                tracing::error!(error = %error, "lifecycle sweep failed");
            }
        }
    })
}

fn reconciler(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(state.config.sample_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if let Err(error) = crate::observation::reconcile(&state).await {
                tracing::error!(error = %error, "service reconciliation failed");
            }
        }
    })
}

fn sampler(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(state.config.sample_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if let Err(error) = sample(&state).await {
                tracing::error!(error = %error, "telemetry sampling failed");
            }
        }
    })
}

async fn sweep(state: &AppState) -> AppResult<()> {
    let _guard = state.mutations.lock().await;
    let now = OffsetDateTime::now_utc();
    let grace = state.config.grace_days;
    let retention = state.config.retention_days;

    let purged = crate::auth::purge_expired_sessions(&state.db).await?;
    if purged > 0 {
        tracing::info!(sessions = purged, "expired sessions purged");
    }

    for service in registry::services_expired_before(&state.db, now).await? {
        let expires_at = service.expires_at.unwrap_or(now);
        let tenant = registry::tenant_by_id(&state.db, service.tenant_id).await?;
        let Some(tenant) = tenant else {
            continue;
        };

        if now <= expires_at + time::Duration::days(grace) {
            if tenant.status == TenantStatus::Active {
                registry::set_tenant_status(&state.db, tenant.id, TenantStatus::Grace, None)
                    .await?;
                tracing::info!(tenant = %tenant.slug, "entered grace period");
            }
            continue;
        }

        if service.state == ServiceState::Suspended {
            continue;
        }

        suspend_service(state, &tenant.slug, &service).await?;
    }

    for service in
        registry::services_purgeable(&state.db, now - time::Duration::days(retention)).await?
    {
        let tenant = registry::tenant_by_id(&state.db, service.tenant_id).await?;
        let Some(tenant) = tenant else {
            continue;
        };
        delete_service(state, &tenant.slug, &service).await?;
    }

    Ok(())
}

async fn suspend_service(
    state: &AppState,
    slug: &str,
    service: &crate::models::Service,
) -> AppResult<()> {
    let unit_name = &service.unit_name;
    let request = broker::Request::ServiceSuspend {
        tenant_id: slug.to_string(),
        service: unit_name.to_string(),
    };

    match crate::observation::execute(
        state,
        service,
        slug,
        &request,
        ServiceState::Suspended,
        None,
    )
    .await
    {
        Ok(_) => {
            tracing::info!(tenant = %slug, unit = %unit_name, "suspension requested after expiry; broker report stored");
        }
        Err(error) => {
            tracing::warn!(tenant = %slug, unit = %unit_name, error = %error, "suspension or its observation failed");
        }
    }
    Ok(())
}

async fn delete_service(
    state: &AppState,
    slug: &str,
    service: &crate::models::Service,
) -> AppResult<()> {
    let unit_name = &service.unit_name;
    let request = broker::Request::ServiceDelete {
        tenant_id: slug.to_string(),
        service: unit_name.to_string(),
    };

    match crate::observation::execute(state, service, slug, &request, ServiceState::Deleted, None)
        .await
    {
        Ok(_) => {
            tracing::info!(tenant = %slug, unit = %unit_name, "service unit deletion acknowledged after retention; data erasure not verified");
        }
        Err(error) => {
            tracing::warn!(tenant = %slug, unit = %unit_name, error = %error, "purge refused by the broker");
        }
    }
    Ok(())
}

async fn sample(state: &AppState) -> AppResult<()> {
    for tenant in registry::tenants(&state.db).await? {
        let fresh_since = OffsetDateTime::now_utc()
            - time::Duration::seconds((state.config.sample_interval.as_secs() * 2) as i64);
        let Some(uid) = tenant_uid(&state.db, tenant.id, fresh_since).await? else {
            continue;
        };
        let snapshot = telemetry::tenant_snapshot(uid);
        registry::record_sample(
            &state.db,
            tenant.id,
            None,
            snapshot.memory_bytes.map(|value| value as i64),
            None,
        )
        .await?;
    }
    Ok(())
}

async fn tenant_uid(
    pool: &SqlitePool,
    tenant_id: i64,
    fresh_since: OffsetDateTime,
) -> AppResult<Option<u32>> {
    let uid: Option<i64> = sqlx::query_scalar(
        "SELECT uid FROM services WHERE tenant_id = ? AND uid > 0 AND uid < 4294967295
           AND deleted_at IS NULL AND state != 'deleted' AND observed_state IS NOT NULL
           AND observed_at >= ? AND observed_at <= ?
         ORDER BY observed_at DESC LIMIT 1",
    )
    .bind(tenant_id)
    .bind(fresh_since)
    .bind(OffsetDateTime::now_utc())
    .fetch_optional(pool)
    .await?
    .flatten();
    Ok(uid
        .and_then(|value| u32::try_from(value).ok())
        .filter(|uid| *uid != 0 && *uid != u32::MAX))
}
