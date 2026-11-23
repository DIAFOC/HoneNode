//! Top-up keys: prepaid plan time the operator generates and tenants redeem.
//!
//! Codes are shown exactly once at generation; the registry keeps only a
//! SHA-256 of the normalized code, so a leaked database does not leak
//! redeemable keys. The entropy is in the code itself (80 bits), which is why
//! an unsalted fast hash is appropriate here — unlike passwords.

use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use time::OffsetDateTime;

use crate::error::{AppError, AppResult};

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const GROUPS: usize = 4;
const GROUP_LEN: usize = 4;
const CODE_CHARS: usize = GROUPS * GROUP_LEN;

/// Crockford base32 display form: `HN-XXXX-XXXX-XXXX-XXXX`.
pub fn generate_code() -> AppResult<String> {
    let mut code = String::with_capacity(CODE_CHARS);
    let mut byte = [0u8; 1];
    while code.len() < CODE_CHARS {
        getrandom::getrandom(&mut byte).map_err(|error| {
            AppError::Internal(anyhow::anyhow!("entropy source failed: {error}"))
        })?;
        // Reject the top 128..=255 range so the mapping stays uniform.
        if byte[0] < 250 {
            code.push(ALPHABET[(byte[0] as usize) % ALPHABET.len()] as char);
        }
    }
    Ok(format_code(&code))
}

fn format_code(normalized: &str) -> String {
    let groups: Vec<&str> = (0..GROUPS)
        .map(|index| &normalized[index * GROUP_LEN..(index + 1) * GROUP_LEN])
        .collect();
    format!("HN-{}", groups.join("-"))
}

/// Accepts a code with any dash/space placement, case, or Crockford aliases
/// (O→0, I/L→1) and returns the canonical 16-character form, with or without
/// the `HN` display prefix.
pub fn normalize(input: &str) -> Option<String> {
    let upper = input.to_ascii_uppercase();
    let mut normalized = String::with_capacity(CODE_CHARS + 2);
    for character in upper.chars() {
        match character {
            'A'..='Z' | '0'..='9' => {
                let alias = match character {
                    'O' => '0',
                    'I' | 'L' => '1',
                    other => other,
                };
                normalized.push(alias);
            }
            '-' | ' ' => {}
            _ => return None,
        }
    }
    // The display prefix is not part of the code: `HN-XXXX-…` and `XXXX-…`
    // normalize to the same value. A 16-character code that happens to start
    // with HN is kept as data because its length already matches.
    if normalized.len() == CODE_CHARS + 2 && normalized.starts_with("HN") {
        normalized.drain(..2);
    }
    if normalized.len() != CODE_CHARS
        || !normalized
            .bytes()
            .all(|byte| ALPHABET.contains(&byte))
    {
        return None;
    }
    Some(normalized)
}

pub fn code_hash(normalized: &str) -> String {
    let digest = Sha256::digest(normalized.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

pub fn code_is_valid(plan_code: &str) -> bool {
    !plan_code.is_empty()
        && plan_code.len() <= 32
        && plan_code
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// One redeemable key, as listed for the operator. The full code is never
/// rendered again after generation.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct KeyRow {
    pub id: i64,
    pub code_prefix: String,
    pub plan_code: String,
    pub days: i64,
    pub created_at: OffsetDateTime,
    pub claimed_at: Option<OffsetDateTime>,
    pub claimed_by: Option<String>,
}

pub struct Redemption {
    pub plan_code: String,
    pub days: i64,
    pub paid_through: OffsetDateTime,
    pub extended_services: u32,
}

/// Generates `count` keys and returns their full codes. The codes are shown
/// once by the caller; only hashes reach the registry.
pub async fn generate(
    pool: &SqlitePool,
    plan_code: &str,
    days: i64,
    count: i64,
    created_by: Option<i64>,
) -> AppResult<Vec<String>> {
    if !code_is_valid(plan_code) {
        return Err(AppError::BadRequest("código de plan inválido".to_string()));
    }
    if !(1..=3650).contains(&days) {
        return Err(AppError::BadRequest("días fuera de rango".to_string()));
    }
    if !(1..=50).contains(&count) {
        return Err(AppError::BadRequest(
            "genera entre 1 y 50 claves por lote".to_string(),
        ));
    }
    let now = OffsetDateTime::now_utc();
    let mut codes = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let mut inserted = false;
        for _ in 0..3 {
            let code = generate_code()?;
            let normalized = normalize(&code).ok_or_else(|| {
                AppError::Internal(anyhow::anyhow!("generated key failed normalization"))
            })?;
            let result = sqlx::query(
                "INSERT INTO topup_keys (code_hash, code_prefix, plan_code, days, created_by, created_at)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(code_hash(&normalized))
            .bind(&normalized[..4])
            .bind(plan_code)
            .bind(days)
            .bind(created_by)
            .bind(now)
            .execute(pool)
            .await;
            match result {
                Ok(_) => {
                    codes.push(code);
                    inserted = true;
                    break;
                }
                Err(sqlx::Error::Database(error))
                    if error.code().as_deref() == Some("2067") =>
                {
                    // UNIQUE collision on the hash: regenerate and try again.
                }
                Err(error) => return Err(error.into()),
            }
        }
        if !inserted {
            return Err(AppError::Internal(anyhow::anyhow!(
                "top-up key generation collided repeatedly"
            )));
        }
    }
    Ok(codes)
}

pub async fn list(pool: &SqlitePool, limit: i64) -> AppResult<Vec<KeyRow>> {
    let rows = sqlx::query_as::<_, KeyRow>(
        "SELECT k.id, k.code_prefix, k.plan_code, k.days, k.created_at, k.claimed_at,
                t.slug AS claimed_by
         FROM topup_keys k
         LEFT JOIN tenants t ON t.id = k.claimed_by_tenant
         ORDER BY k.id DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Claims a key for a tenant and applies its time in one transaction:
/// every non-deleted service is extended additively, suspended services return
/// to a startable state, the subscription moves to the key's plan, and a
/// grace/suspended tenant returns to active while the new expiry is future.
pub async fn redeem(
    pool: &SqlitePool,
    tenant_id: i64,
    input: &str,
) -> AppResult<Redemption> {
    let normalized = normalize(input).ok_or_else(|| {
        AppError::BadRequest("formato de clave no válido".to_string())
    })?;
    let hash = code_hash(&normalized);
    let now = OffsetDateTime::now_utc();

    let mut tx = pool.begin().await?;
    let claimed: Option<(String, i64)> = sqlx::query_as(
        "UPDATE topup_keys SET claimed_by_tenant = ?, claimed_at = ?
         WHERE code_hash = ? AND claimed_at IS NULL
         RETURNING plan_code, days",
    )
    .bind(tenant_id)
    .bind(now)
    .bind(&hash)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((plan_code, days)) = claimed else {
        // Distinguish an unknown code from an already-claimed one for the UI.
        let state: Option<Option<i64>> = sqlx::query_scalar(
            "SELECT claimed_by_tenant FROM topup_keys WHERE code_hash = ?",
        )
        .bind(&hash)
        .fetch_optional(&mut *tx)
        .await?;
        return match state {
            Some(_) => Err(AppError::Conflict("la clave ya fue canjeada".to_string())),
            None => Err(AppError::NotFound),
        };
    };

    // Additive extension over every live service, mirroring registry::extend_service.
    let services: Vec<(i64, Option<OffsetDateTime>)> = sqlx::query_as(
        "SELECT id, expires_at FROM services
         WHERE tenant_id = ? AND deleted_at IS NULL AND state != 'deleted'",
    )
    .bind(tenant_id)
    .fetch_all(&mut *tx)
    .await?;
    let mut extended: u32 = 0;
    for (service_id, expires_at) in services {
        let base = expires_at.filter(|value| *value > now).unwrap_or(now);
        let expires = base
            .checked_add(time::Duration::days(days))
            .ok_or_else(|| AppError::BadRequest("expiry out of range".to_string()))?;
        sqlx::query("UPDATE services SET expires_at = ? WHERE id = ?")
            .bind(expires)
            .bind(service_id)
            .execute(&mut *tx)
            .await?;
        extended += 1;
    }

    // Suspension was only the desired-state record; the unit is already stopped,
    // so returning to `stopped` lets the tenant start it again.
    sqlx::query(
        "UPDATE services SET state = 'stopped', suspended_at = NULL
         WHERE tenant_id = ? AND state = 'suspended' AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "INSERT INTO plans (code, monthly_price, cpu_weight, memory_high, memory_max,
                            tasks_max, disk_gb, bandwidth_gb, max_services, description)
         VALUES (?, 0, 100, '256M', '384M', 128, 0, 0, 1, '')
         ON CONFLICT(code) DO NOTHING",
    )
    .bind(&plan_code)
    .execute(&mut *tx)
    .await?;
    let plan_id: i64 = sqlx::query_scalar("SELECT id FROM plans WHERE code = ?")
        .bind(&plan_code)
        .fetch_one(&mut *tx)
        .await?;

    let current: Option<OffsetDateTime> = sqlx::query_scalar(
        "SELECT paid_through FROM subscriptions WHERE tenant_id = ? ORDER BY id DESC LIMIT 1",
    )
    .bind(tenant_id)
    .fetch_optional(&mut *tx)
    .await?
    .flatten();
    let paid_through = current
        .filter(|value| *value > now)
        .unwrap_or(now)
        .checked_add(time::Duration::days(days))
        .ok_or_else(|| AppError::BadRequest("expiry out of range".to_string()))?;

    let updated = sqlx::query(
        "UPDATE subscriptions SET plan_id = ?, paid_through = ? WHERE id = (
             SELECT id FROM subscriptions WHERE tenant_id = ? ORDER BY id DESC LIMIT 1)",
    )
    .bind(plan_id)
    .bind(paid_through)
    .bind(tenant_id)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() == 0 {
        sqlx::query(
            "INSERT INTO subscriptions (tenant_id, plan_id, started_at, paid_through, auto_renew)
             VALUES (?, ?, ?, ?, 0)",
        )
        .bind(tenant_id)
        .bind(plan_id)
        .bind(now)
        .bind(paid_through)
        .execute(&mut *tx)
        .await?;
    }

    sqlx::query(
        "UPDATE tenants SET status = 'active', suspended_at = NULL
         WHERE id = ? AND status IN ('grace', 'suspended')",
    )
    .bind(tenant_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(Redemption {
        plan_code,
        days,
        paid_through,
        extended_services: extended,
    })
}

/// The tenant's current subscription plan code, if any.
pub async fn subscription_plan_code(pool: &SqlitePool, tenant_id: i64) -> AppResult<Option<String>> {
    let code: Option<String> = sqlx::query_scalar(
        "SELECT p.code FROM subscriptions s JOIN plans p ON p.id = s.plan_id
         WHERE s.tenant_id = ? ORDER BY s.id DESC LIMIT 1",
    )
    .bind(tenant_id)
    .fetch_optional(pool)
    .await?;
    Ok(code)
}

/// The tenant's current paid-through date, if any.
pub async fn subscription_paid_through(
    pool: &SqlitePool,
    tenant_id: i64,
) -> AppResult<Option<OffsetDateTime>> {
    let paid_through: Option<Option<OffsetDateTime>> = sqlx::query_scalar(
        "SELECT paid_through FROM subscriptions WHERE tenant_id = ? ORDER BY id DESC LIMIT 1",
    )
    .bind(tenant_id)
    .fetch_optional(pool)
    .await?;
    Ok(paid_through.flatten())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip_through_normalization() {
        for _ in 0..32 {
            let code = generate_code().unwrap();
            assert!(code.starts_with("HN-"));
            assert_eq!(normalize(&code).as_deref(), normalize(&code).as_deref());
            let normalized = normalize(&code).unwrap();
            assert_eq!(normalized.len(), 16);
            assert_eq!(format_code(&normalized), code);
        }
    }

    #[test]
    fn normalization_accepts_case_dashes_and_crockford_aliases() {
        let normalized = normalize("hn-abcd-efgh-jkmn-pqrs").unwrap();
        assert_eq!(normalized, "ABCDEFGHJKMNPQRS");
        // O→0, I→1, L→1 aliases land on alphabet characters.
        let aliased = normalize("HN-OILO-ILOI-LOIL-OILO").unwrap();
        assert!(aliased.bytes().all(|byte| ALPHABET.contains(&byte)));
        // Dashes and spaces are interchangeable separators.
        assert_eq!(
            normalize("HN-ABCD-EFGH-JKMN-PQRS"),
            normalize("HNABCDEFGHJKMNPQRS")
        );
        assert_eq!(
            normalize("HN-ABCD-EFGH-JKMN-PQRS"),
            normalize("hn abcd efgh jkmn pqrs")
        );
        // Wrong length and foreign characters are refused.
        assert!(normalize("HN-ABCD-EFGH-JKMN").is_none());
        assert!(normalize("HN-ABCD-EFGH-JKMN-PQRS-X").is_none());
        assert!(normalize("HN-ABCD-EFGH-JKMN-PQÑS").is_none());
    }

    #[test]
    fn hashes_are_stable_and_prefix_is_derived() {
        let normalized = normalize("HN-ABCD-EFGH-JKMN-PQRS").unwrap();
        let hash = code_hash(&normalized);
        assert_eq!(hash.len(), 64);
        assert_eq!(hash, code_hash(&normalize("hn abcd efgh jkmn pqrs").unwrap()));
        assert_eq!(&normalized[..4], "ABCD");
    }

    #[test]
    fn plan_codes_follow_the_broker_alphabet() {
        assert!(code_is_valid("starter"));
        assert!(code_is_valid("pro-2026"));
        assert!(!code_is_valid(""));
        assert!(!code_is_valid("Pro"));
        assert!(!code_is_valid("evil;drop"));
        assert!(!code_is_valid(&"a".repeat(33)));
    }
}
