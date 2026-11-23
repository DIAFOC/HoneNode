use std::path::Path;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::SqlitePool;
use time::OffsetDateTime;

use crate::error::AppResult;
use crate::models::{
    Account, AuditEvent, Job, JobState, Plan, Service, ServiceKind, ServiceState, ServiceView,
    Tenant, TenantStatus,
};

pub async fn connect(path: &Path) -> anyhow::Result<SqlitePool> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    // SQLite stores password/session material. Do not create a world-readable registry,
    // and refuse a symlink or special file before handing the path to SQLite.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "registry must be a regular file"
    );
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;

    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(options)
        .await?;

    Ok(pool)
}

pub async fn migrate(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::migrate!("./migrations").run(pool).await?;
    Ok(())
}

pub async fn account_by_id(pool: &SqlitePool, id: i64) -> AppResult<Option<Account>> {
    let account = sqlx::query_as::<_, Account>(
        "SELECT id, number, role, password_hash, created_at, last_login_at, disabled_at, notes
         FROM accounts WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(account)
}

pub async fn accounts(pool: &SqlitePool, limit: i64) -> AppResult<Vec<Account>> {
    let accounts = sqlx::query_as::<_, Account>(
        "SELECT id, number, role, password_hash, created_at, last_login_at, disabled_at, notes
         FROM accounts ORDER BY id DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(accounts)
}

pub async fn create_tenant(
    pool: &SqlitePool,
    account_id: i64,
    slug: &str,
    name: &str,
) -> AppResult<Tenant> {
    let now = OffsetDateTime::now_utc();
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO tenants (account_id, slug, name, status, created_at)
         VALUES (?, ?, ?, 'active', ?) RETURNING id",
    )
    .bind(account_id)
    .bind(slug)
    .bind(name)
    .bind(now)
    .fetch_one(pool)
    .await?;

    Ok(Tenant {
        id,
        account_id,
        slug: slug.to_string(),
        name: name.to_string(),
        status: TenantStatus::Active,
        created_at: now,
        suspended_at: None,
        purge_after: None,
    })
}

pub async fn next_tenant_slug(pool: &SqlitePool, allowed: &[String]) -> AppResult<String> {
    let taken: Vec<String> = sqlx::query_scalar("SELECT slug FROM tenants")
        .fetch_all(pool)
        .await?;
    for candidate in allowed {
        if !taken.iter().any(|slug| slug == candidate) {
            return Ok(candidate.clone());
        }
    }
    Err(crate::error::AppError::Conflict(
        "no tenant slot left on this node allowlist".to_string(),
    ))
}

pub async fn tenant_by_id(pool: &SqlitePool, id: i64) -> AppResult<Option<Tenant>> {
    let tenant = sqlx::query_as::<_, Tenant>(
        "SELECT id, account_id, slug, name, status, created_at, suspended_at, purge_after
         FROM tenants WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(tenant)
}

pub async fn tenant_by_account(pool: &SqlitePool, account_id: i64) -> AppResult<Option<Tenant>> {
    let tenant = sqlx::query_as::<_, Tenant>(
        "SELECT id, account_id, slug, name, status, created_at, suspended_at, purge_after
         FROM tenants WHERE account_id = ?",
    )
    .bind(account_id)
    .fetch_optional(pool)
    .await?;
    Ok(tenant)
}

pub async fn tenants(pool: &SqlitePool) -> AppResult<Vec<Tenant>> {
    let tenants = sqlx::query_as::<_, Tenant>(
        "SELECT id, account_id, slug, name, status, created_at, suspended_at, purge_after
         FROM tenants ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    Ok(tenants)
}

pub async fn set_tenant_status(
    pool: &SqlitePool,
    id: i64,
    status: TenantStatus,
    purge_after: Option<OffsetDateTime>,
) -> AppResult<()> {
    let suspended_at = matches!(status, TenantStatus::Suspended | TenantStatus::Purged)
        .then(OffsetDateTime::now_utc);
    sqlx::query("UPDATE tenants SET status = ?, suspended_at = ?, purge_after = ? WHERE id = ?")
        .bind(status)
        .bind(suspended_at)
        .bind(purge_after)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn plans(pool: &SqlitePool) -> AppResult<Vec<Plan>> {
    let plans = sqlx::query_as::<_, Plan>(
        "SELECT id, code, monthly_price, cpu_weight, memory_high, memory_max, tasks_max,
                disk_gb, bandwidth_gb, max_services, description
         FROM plans ORDER BY monthly_price",
    )
    .fetch_all(pool)
    .await?;
    Ok(plans)
}

pub async fn plan_by_code(pool: &SqlitePool, code: &str) -> AppResult<Option<Plan>> {
    let plan = sqlx::query_as::<_, Plan>(
        "SELECT id, code, monthly_price, cpu_weight, memory_high, memory_max, tasks_max,
                disk_gb, bandwidth_gb, max_services, description
         FROM plans WHERE code = ?",
    )
    .bind(code)
    .fetch_optional(pool)
    .await?;
    Ok(plan)
}

pub async fn plan_by_id(pool: &SqlitePool, id: i64) -> AppResult<Option<Plan>> {
    let plan = sqlx::query_as::<_, Plan>(
        "SELECT id, code, monthly_price, cpu_weight, memory_high, memory_max, tasks_max,
                disk_gb, bandwidth_gb, max_services, description
         FROM plans WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(plan)
}

/// Guarantees a display row for `code` and returns its id. The live root
/// catalogue remains the authorization source; this only satisfies the
/// subscription foreign key when a plan has not been provisioned yet.
pub async fn ensure_plan_row(pool: &SqlitePool, code: &str) -> AppResult<i64> {
    sqlx::query(
        "INSERT INTO plans (code, monthly_price, cpu_weight, memory_high, memory_max,
                            tasks_max, disk_gb, bandwidth_gb, max_services, description)
         VALUES (?, 0, 100, '256M', '384M', 128, 0, 0, 1, '')
         ON CONFLICT(code) DO NOTHING",
    )
    .bind(code)
    .execute(pool)
    .await?;
    let id: i64 = sqlx::query_scalar("SELECT id FROM plans WHERE code = ?")
        .bind(code)
        .fetch_one(pool)
        .await?;
    Ok(id)
}

pub async fn create_service(
    pool: &SqlitePool,
    tenant_id: i64,
    kind: ServiceKind,
    plan_id: i64,
    unit_name: &str,
    uid: Option<i64>,
    ports_json: &str,
    volume_json: &str,
    expires_at: OffsetDateTime,
) -> AppResult<Service> {
    let limit = plan_by_id(pool, plan_id).await?.ok_or(crate::error::AppError::NotFound)?.max_services;
    reserve_service(pool, tenant_id, kind, plan_id, unit_name, uid, ports_json, volume_json, expires_at, limit).await
}

/// Unit names and loopback ports already held by live services of one tenant.
/// The app uses this to pick the next instance number before reserving.
#[derive(Debug, Default)]
pub struct LiveAllocations {
    pub units: std::collections::HashSet<String>,
    pub ports: std::collections::HashSet<u16>,
}

pub async fn live_allocations(
    pool: &SqlitePool,
    tenant_id: i64,
) -> AppResult<LiveAllocations> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT unit_name, ports_json FROM services
         WHERE tenant_id = ? AND deleted_at IS NULL AND state != 'deleted'",
    )
    .bind(tenant_id)
    .fetch_all(pool)
    .await?;
    let mut allocations = LiveAllocations::default();
    for (unit, ports_json) in rows {
        allocations.units.insert(unit);
        if let Ok(serde_json::Value::Object(ports)) = serde_json::from_str(&ports_json) {
            for port in ports.values() {
                if let Some(port) = port.as_u64().and_then(|port| u16::try_from(port).ok()) {
                    allocations.ports.insert(port);
                }
            }
        }
    }
    Ok(allocations)
}

/// The selected plan's limit is a tenant-wide admission ceiling across all plans and kinds.
/// One atomic statement covers quota, fresh tenant status and unique unit/port/volume reservations.
pub async fn reserve_service(
    pool: &SqlitePool,
    tenant_id: i64,
    kind: ServiceKind,
    plan_id: i64,
    unit_name: &str,
    uid: Option<i64>,
    ports_json: &str,
    volume_json: &str,
    expires_at: OffsetDateTime,
    max_services: i64,
) -> AppResult<Service> {
    let now = OffsetDateTime::now_utc();
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO services (tenant_id, kind, plan_id, state, unit_name, uid, ports_json,
                               volume_json, created_at, expires_at)
         SELECT ?, ?, ?, 'pending', ?, ?, ?, ?, ?, ?
         WHERE EXISTS (SELECT 1 FROM tenants WHERE id = ? AND status = 'active')
           AND (SELECT COUNT(*) FROM services
                           WHERE tenant_id = ? AND state != 'deleted' AND deleted_at IS NULL) < ?
           AND NOT EXISTS (SELECT 1 FROM services
                           WHERE unit_name = ? AND deleted_at IS NULL AND state != 'deleted')
           AND NOT EXISTS (
               SELECT 1 FROM services s, json_each(s.ports_json) old, json_each(?) new
               WHERE old.value = new.value
                 AND s.deleted_at IS NULL AND s.state != 'deleted'
           )
           AND NOT EXISTS (
               SELECT 1 FROM services s, json_each(s.volume_json) old, json_each(?) new
               WHERE old.value = new.value
                 AND s.deleted_at IS NULL AND s.state != 'deleted'
           )
         RETURNING id",
    )
    .bind(tenant_id)
    .bind(kind)
    .bind(plan_id)
    .bind(unit_name)
    .bind(uid)
    .bind(ports_json)
    .bind(volume_json)
    .bind(now)
    .bind(expires_at)
    .bind(tenant_id)
    .bind(tenant_id)
    .bind(max_services)
    .bind(unit_name)
    .bind(ports_json)
    .bind(volume_json)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| crate::error::AppError::Conflict(
        "tenant inactive, plan quota exhausted, or unit/port/volume already reserved".to_string()
    ))?;

    Ok(Service {
        id,
        tenant_id,
        kind,
        plan_id,
        state: ServiceState::Pending,
        observed_state: None,
        observed_at: None,
        unit_name: unit_name.to_string(),
        uid,
        ports_json: ports_json.to_string(),
        volume_json: volume_json.to_string(),
        created_at: now,
        expires_at: Some(expires_at),
        suspended_at: None,
        deleted_at: None,
    })
}

pub async fn service_by_id(pool: &SqlitePool, id: i64) -> AppResult<Option<Service>> {
    let service = sqlx::query_as::<_, Service>(
        "SELECT id, tenant_id, kind, plan_id, state, observed_state, observed_at, unit_name, uid, ports_json,
                volume_json, created_at, expires_at, suspended_at, deleted_at
         FROM services WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(service)
}

pub async fn service_view(pool: &SqlitePool, id: i64) -> AppResult<Option<ServiceView>> {
    let view = sqlx::query_as::<_, ServiceView>(
        "SELECT s.id, s.tenant_id, t.slug AS tenant_slug, t.name AS tenant_name, s.kind,
                s.plan_id, p.code AS plan_code, s.state, s.observed_state, s.observed_at, s.unit_name,
                s.ports_json, s.created_at, s.expires_at
         FROM services s
         JOIN tenants t ON t.id = s.tenant_id
         JOIN plans p ON p.id = s.plan_id
         WHERE s.id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(view)
}

pub async fn services_of_tenant(pool: &SqlitePool, tenant_id: i64) -> AppResult<Vec<ServiceView>> {
    let views = sqlx::query_as::<_, ServiceView>(
        "SELECT s.id, s.tenant_id, t.slug AS tenant_slug, t.name AS tenant_name, s.kind,
                s.plan_id, p.code AS plan_code, s.state, s.observed_state, s.observed_at, s.unit_name,
                s.ports_json, s.created_at, s.expires_at
         FROM services s
         JOIN tenants t ON t.id = s.tenant_id
         JOIN plans p ON p.id = s.plan_id
         WHERE s.tenant_id = ? AND s.deleted_at IS NULL
         ORDER BY s.id",
    )
    .bind(tenant_id)
    .fetch_all(pool)
    .await?;
    Ok(views)
}

pub async fn all_services(pool: &SqlitePool) -> AppResult<Vec<ServiceView>> {
    let views = sqlx::query_as::<_, ServiceView>(
        "SELECT s.id, s.tenant_id, t.slug AS tenant_slug, t.name AS tenant_name, s.kind,
                s.plan_id, p.code AS plan_code, s.state, s.observed_state, s.observed_at, s.unit_name,
                s.ports_json, s.created_at, s.expires_at
         FROM services s
         JOIN tenants t ON t.id = s.tenant_id
         JOIN plans p ON p.id = s.plan_id
         WHERE s.deleted_at IS NULL
         ORDER BY s.tenant_id, s.id",
    )
    .fetch_all(pool)
    .await?;
    Ok(views)
}

pub async fn count_services(pool: &SqlitePool, tenant_id: i64) -> AppResult<i64> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM services WHERE tenant_id = ? AND state != 'deleted' AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

pub async fn record_observation(
    pool: &SqlitePool,
    service: &Service,
    observation: &crate::broker::Observation,
) -> AppResult<()> {
    if observation.unit() != service.unit_name.as_str() {
        return Err(crate::error::AppError::Conflict(
            "observation unit does not match service".to_string(),
        ));
    }
    let result = sqlx::query(
        "UPDATE services SET uid = ?, observed_state = ?, observed_at = ?,
             state = CASE WHEN state IN ('pending', 'failed') AND ? = 'active'
                          THEN 'running' ELSE state END
         WHERE id = ? AND tenant_id = ? AND unit_name = ?
           AND deleted_at IS NULL AND state != 'deleted'",
    )
    .bind(i64::from(observation.uid()))
    .bind(observation.active_state().as_str())
    .bind(OffsetDateTime::now_utc())
    .bind(observation.active_state().as_str())
    .bind(service.id)
    .bind(service.tenant_id)
    .bind(observation.unit())
    .execute(pool)
    .await?;
    if result.rows_affected() != 1 {
        return Err(crate::error::AppError::Conflict(
            "observation target changed or was deleted".to_string(),
        ));
    }
    Ok(())
}

pub async fn clear_service_observation(pool: &SqlitePool, service: &Service) -> AppResult<()> {
    sqlx::query(
        "UPDATE services SET observed_state = NULL, observed_at = NULL
                 WHERE id = ? AND tenant_id = ? AND unit_name = ?",
    )
    .bind(service.id)
    .bind(service.tenant_id)
    .bind(&service.unit_name)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_service_state(
    pool: &SqlitePool,
    id: i64,
    state: ServiceState,
    observed: Option<&str>,
) -> AppResult<()> {
    let suspended_at = matches!(state, ServiceState::Suspended).then(OffsetDateTime::now_utc);
    let deleted_at = matches!(state, ServiceState::Deleted).then(OffsetDateTime::now_utc);
    // The tombstone keeps a unique unit name: renaming it on the first delete
    // frees the original name for re-provisioning while the row stays for audit.
    sqlx::query(
        "UPDATE services
         SET state = ?,
             observed_state = COALESCE(?, observed_state),
             suspended_at = COALESCE(?, suspended_at),
             deleted_at = COALESCE(?, deleted_at),
             unit_name = CASE WHEN ? = 'deleted' AND deleted_at IS NULL
                              THEN unit_name || '-deleted-' || id ELSE unit_name END
         WHERE id = ?",
    )
    .bind(state.as_str())
    .bind(observed)
    .bind(suspended_at)
    .bind(deleted_at)
    .bind(state.as_str())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn extend_service(
    pool: &SqlitePool,
    id: i64,
    days: i64,
) -> AppResult<Option<OffsetDateTime>> {
    if !(1..=3650).contains(&days) {
        return Err(crate::error::AppError::BadRequest(
            "invalid extension".to_string(),
        ));
    }
    let mut tx = pool.begin().await?;
    // Acquire SQLite's writer lock before reading expiry: concurrent extensions must be additive.
    let result = sqlx::query(
        "UPDATE services SET expires_at = expires_at WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    if result.rows_affected() == 0 {
        return Ok(None);
    }
    let expires: Option<OffsetDateTime> =
        sqlx::query_scalar("SELECT expires_at FROM services WHERE id = ?")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    let now = OffsetDateTime::now_utc();
    let base = expires.filter(|value| *value > now).unwrap_or(now);
    let expires_at = base
        .checked_add(time::Duration::days(days))
        .ok_or_else(|| crate::error::AppError::BadRequest("expiry out of range".to_string()))?;
    sqlx::query("UPDATE services SET expires_at = ? WHERE id = ?")
        .bind(expires_at)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(expires_at))
}

pub async fn services_expired_before(
    pool: &SqlitePool,
    moment: OffsetDateTime,
) -> AppResult<Vec<Service>> {
    let services = sqlx::query_as::<_, Service>(
        "SELECT id, tenant_id, kind, plan_id, state, observed_state, observed_at, unit_name, uid, ports_json,
                volume_json, created_at, expires_at, suspended_at, deleted_at
         FROM services
         WHERE expires_at IS NOT NULL AND expires_at <= ? AND deleted_at IS NULL
           AND state IN ('running', 'pending', 'stopped')
         ORDER BY expires_at",
    )
    .bind(moment)
    .fetch_all(pool)
    .await?;
    Ok(services)
}

pub async fn services_purgeable(
    pool: &SqlitePool,
    moment: OffsetDateTime,
) -> AppResult<Vec<Service>> {
    let services = sqlx::query_as::<_, Service>(
        "SELECT id, tenant_id, kind, plan_id, state, observed_state, observed_at, unit_name, uid, ports_json,
                volume_json, created_at, expires_at, suspended_at, deleted_at
         FROM services
         WHERE state = 'suspended' AND suspended_at IS NOT NULL AND suspended_at <= ?
           AND deleted_at IS NULL AND expires_at <= ?",
    )
    .bind(moment)
    .bind(moment)
    .fetch_all(pool)
    .await?;
    Ok(services)
}

pub async fn create_job(
    pool: &SqlitePool,
    tenant_id: Option<i64>,
    action: &str,
    payload_json: &str,
    requested_by: Option<i64>,
) -> AppResult<i64> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, action, payload_json, state, requested_by)
         VALUES (?, ?, ?, 'queued', ?) RETURNING id",
    )
    .bind(tenant_id)
    .bind(action)
    .bind(payload_json)
    .bind(requested_by)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

pub async fn finish_job(
    pool: &SqlitePool,
    id: i64,
    state: JobState,
    error: Option<&str>,
) -> AppResult<()> {
    sqlx::query("UPDATE jobs SET state = ?, finished_at = ?, error = ? WHERE id = ?")
        .bind(state)
        .bind(OffsetDateTime::now_utc())
        .bind(error)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn start_job(pool: &SqlitePool, id: i64) -> AppResult<()> {
    sqlx::query("UPDATE jobs SET state = 'running', started_at = ? WHERE id = ?")
        .bind(OffsetDateTime::now_utc())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn recent_jobs(pool: &SqlitePool, limit: i64) -> AppResult<Vec<Job>> {
    let jobs = sqlx::query_as::<_, Job>(
        "SELECT id, tenant_id, action, payload_json, state, requested_by, started_at, finished_at, error
         FROM jobs ORDER BY id DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(jobs)
}

pub struct NewAuditEvent<'a> {
    pub request_id: &'a str,
    pub tenant_id: Option<i64>,
    pub actor_type: &'a str,
    pub actor_id: Option<&'a str>,
    pub action: &'a str,
    pub resource_type: Option<&'a str>,
    pub resource_id: Option<&'a str>,
    pub outcome: &'a str,
    pub reason: Option<&'a str>,
    pub src_ip: Option<&'a str>,
    pub user_agent: Option<&'a str>,
}

pub async fn record_audit(pool: &SqlitePool, event: NewAuditEvent<'_>) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO audit_events (ts, request_id, tenant_id, actor_type, actor_id, action,
                                   resource_type, resource_id, outcome, reason, src_ip, user_agent)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(OffsetDateTime::now_utc())
    .bind(event.request_id)
    .bind(event.tenant_id)
    .bind(event.actor_type)
    .bind(event.actor_id)
    .bind(event.action)
    .bind(event.resource_type)
    .bind(event.resource_id)
    .bind(event.outcome)
    .bind(event.reason)
    .bind(event.src_ip)
    .bind(event.user_agent)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn recent_audit(pool: &SqlitePool, limit: i64) -> AppResult<Vec<AuditEvent>> {
    let events = sqlx::query_as::<_, AuditEvent>(
        "SELECT id, ts, request_id, tenant_id, actor_type, actor_id, action, resource_type,
                resource_id, outcome, reason, src_ip, user_agent
         FROM audit_events ORDER BY id DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(events)
}

pub async fn audit_of_tenant(
    pool: &SqlitePool,
    tenant_id: i64,
    limit: i64,
) -> AppResult<Vec<AuditEvent>> {
    let events = sqlx::query_as::<_, AuditEvent>(
        "SELECT id, ts, request_id, tenant_id, actor_type, actor_id, action, resource_type,
                resource_id, outcome, reason, src_ip, user_agent
         FROM audit_events WHERE tenant_id = ? ORDER BY id DESC LIMIT ?",
    )
    .bind(tenant_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(events)
}

pub async fn record_sample(
    pool: &SqlitePool,
    tenant_id: i64,
    cpu_pct: Option<f64>,
    mem_bytes: Option<i64>,
    disk_bytes: Option<i64>,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO resource_samples (tenant_id, ts, cpu_pct, mem_bytes, disk_bytes)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(tenant_id)
    .bind(OffsetDateTime::now_utc())
    .bind(cpu_pct)
    .bind(mem_bytes)
    .bind(disk_bytes)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn latest_sample(
    pool: &SqlitePool,
    tenant_id: i64,
) -> AppResult<Option<(Option<f64>, Option<i64>, Option<i64>)>> {
    let row = sqlx::query_as::<_, (Option<f64>, Option<i64>, Option<i64>)>(
        "SELECT cpu_pct, mem_bytes, disk_bytes FROM resource_samples
         WHERE tenant_id = ? ORDER BY id DESC LIMIT 1",
    )
    .bind(tenant_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}
