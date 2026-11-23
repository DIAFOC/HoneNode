use std::time::Duration as StdDuration;

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use sqlx::SqlitePool;
use time::{Duration, OffsetDateTime};

use crate::error::{AppError, AppResult};
use crate::models::{Account, Role, Session};

const ACCOUNT_DIGITS: usize = 11;
const ACCOUNT_LENGTH: usize = ACCOUNT_DIGITS + 1;

pub fn account_number_is_valid(number: &str) -> bool {
    if number.len() != ACCOUNT_LENGTH || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    let digits: Vec<u8> = number.bytes().map(|byte| byte - b'0').collect();
    let (payload, check) = digits.split_at(ACCOUNT_DIGITS);
    check_digit(payload) == check[0]
}

pub fn generate_account_number() -> AppResult<String> {
    let mut payload = [0u8; ACCOUNT_DIGITS];
    for slot in payload.iter_mut() {
        *slot = random_digit()?;
    }
    let mut number = String::with_capacity(ACCOUNT_LENGTH);
    for digit in payload {
        number.push((b'0' + digit) as char);
    }
    number.push((b'0' + check_digit(&payload)) as char);
    Ok(number)
}

fn check_digit(payload: &[u8]) -> u8 {
    let mut sum = 0u32;
    for (index, digit) in payload.iter().rev().enumerate() {
        let mut value = *digit as u32;
        if index % 2 == 0 {
            value *= 2;
            if value > 9 {
                value -= 9;
            }
        }
        sum += value;
    }
    ((10 - (sum % 10)) % 10) as u8
}

fn random_digit() -> AppResult<u8> {
    loop {
        let mut byte = [0u8; 1];
        getrandom::getrandom(&mut byte).map_err(random_failure)?;
        if byte[0] < 250 {
            return Ok(byte[0] % 10);
        }
    }
}

pub fn generate_token() -> AppResult<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(random_failure)?;
    Ok(hex(&bytes))
}

pub fn generate_password(length: usize) -> AppResult<String> {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
    const LIMIT: u8 = 228;
    let mut password = String::with_capacity(length);
    let mut byte = [0u8; 1];
    while password.len() < length {
        getrandom::getrandom(&mut byte).map_err(random_failure)?;
        if byte[0] < LIMIT {
            password.push(ALPHABET[(byte[0] as usize) % ALPHABET.len()] as char);
        }
    }
    Ok(password)
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

fn random_failure(error: getrandom::Error) -> AppError {
    AppError::Internal(anyhow::anyhow!("entropy source failed: {error}"))
}

pub fn hash_password(password: &str) -> AppResult<String> {
    let mut salt_bytes = [0u8; 16];
    getrandom::getrandom(&mut salt_bytes).map_err(random_failure)?;
    let salt = SaltString::encode_b64(&salt_bytes)
        .map_err(|error| AppError::Internal(anyhow::anyhow!("salt: {error}")))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|error| AppError::Internal(anyhow::anyhow!("argon2: {error}")))
}

pub fn verify_password(password: &str, encoded: &str) -> bool {
    match PasswordHash::new(encoded) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

pub async fn authenticate(
    pool: &SqlitePool,
    number: &str,
    password: &str,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> AppResult<Option<Account>> {
    let account = sqlx::query_as::<_, Account>(
        "SELECT id, number, role, password_hash, created_at, last_login_at, disabled_at, notes
         FROM accounts WHERE number = ?",
    )
    .bind(number)
    .fetch_optional(pool)
    .await?;

    let password = password.to_string();
    let hash = account
        .as_ref()
        .map(|account| account.password_hash.clone());
    // Unknown and disabled accounts still incur the password KDF cost, off the async executor.
    let verified = tokio::task::spawn_blocking(move || -> AppResult<bool> {
        // Keep the permit inside the blocking task, even if the HTTP request is cancelled.
        let _permit = permit;
        static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        if DUMMY.get().is_none() {
            let _ = DUMMY.set(hash_password("not-a-login-password")?);
        }
        let encoded = hash
            .as_deref()
            .or_else(|| DUMMY.get().map(String::as_str))
            .ok_or_else(|| AppError::Internal(anyhow::anyhow!("missing dummy password hash")))?;
        Ok(verify_password(&password, encoded))
    })
    .await
    .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))??;
    let Some(account) = account.filter(|account| account.is_enabled() && verified) else {
        return Ok(None);
    };

    sqlx::query("UPDATE accounts SET last_login_at = ? WHERE id = ?")
        .bind(OffsetDateTime::now_utc())
        .bind(account.id)
        .execute(pool)
        .await?;

    Ok(Some(account))
}

pub async fn create_account(
    pool: &SqlitePool,
    role: Role,
    password: &str,
    number: Option<&str>,
) -> AppResult<Account> {
    let number = match number {
        Some(number) if account_number_is_valid(number) => number.to_string(),
        Some(_) => return Err(AppError::BadRequest("invalid account number".to_string())),
        None => generate_account_number()?,
    };
    let hash = new_password_hash(password).await?;
    let now = OffsetDateTime::now_utc();

    let id: i64 = sqlx::query_scalar(
        "INSERT INTO accounts (number, role, password_hash, created_at)
         VALUES (?, ?, ?, ?) RETURNING id",
    )
    .bind(&number)
    .bind(role)
    .bind(&hash)
    .bind(now)
    .fetch_one(pool)
    .await?;

    Ok(Account {
        id,
        number,
        role,
        password_hash: hash,
        created_at: now,
        last_login_at: None,
        disabled_at: None,
        notes: None,
    })
}

pub async fn set_password(pool: &SqlitePool, account_id: i64, password: &str) -> AppResult<()> {
    let hash = new_password_hash(password).await?;
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE accounts SET password_hash = ? WHERE id = ?")
        .bind(hash)
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM sessions WHERE account_id = ?")
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

async fn new_password_hash(password: &str) -> AppResult<String> {
    if !(12..=1024).contains(&password.len()) {
        return Err(AppError::BadRequest(
            "password must contain 12 to 1024 bytes".to_string(),
        ));
    }
    let password = password.to_string();
    tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))?
}

pub async fn operator_exists(pool: &SqlitePool) -> AppResult<bool> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accounts WHERE role = 'operator'")
        .fetch_one(pool)
        .await?;
    Ok(count > 0)
}

pub async fn create_session(
    pool: &SqlitePool,
    account_id: i64,
    ttl: StdDuration,
    src_ip: Option<&str>,
    user_agent: Option<&str>,
) -> AppResult<Session> {
    let now = OffsetDateTime::now_utc();
    let session = Session {
        id: generate_token()?,
        account_id,
        csrf_token: generate_token()?,
        created_at: now,
        expires_at: now + Duration::seconds(ttl.as_secs() as i64),
        last_seen_at: now,
        src_ip: src_ip.map(str::to_string),
        user_agent: user_agent.map(str::to_string),
    };

    sqlx::query(
        "INSERT INTO sessions (id, account_id, csrf_token, created_at, expires_at, last_seen_at, src_ip, user_agent)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&session.id)
    .bind(session.account_id)
    .bind(&session.csrf_token)
    .bind(session.created_at)
    .bind(session.expires_at)
    .bind(session.last_seen_at)
    .bind(&session.src_ip)
    .bind(&session.user_agent)
    .execute(pool)
    .await?;

    Ok(session)
}

pub async fn load_session(pool: &SqlitePool, token: &str) -> AppResult<Option<Session>> {
    let session = sqlx::query_as::<_, Session>(
        "SELECT id, account_id, csrf_token, created_at, expires_at, last_seen_at, src_ip, user_agent
         FROM sessions WHERE id = ? AND expires_at > ?",
    )
    .bind(token)
    .bind(OffsetDateTime::now_utc())
    .fetch_optional(pool)
    .await?;
    Ok(session)
}

pub async fn touch_session(pool: &SqlitePool, id: &str) -> AppResult<()> {
    sqlx::query("UPDATE sessions SET last_seen_at = ? WHERE id = ?")
        .bind(OffsetDateTime::now_utc())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete_session(pool: &SqlitePool, id: &str) -> AppResult<()> {
    sqlx::query("DELETE FROM sessions WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn purge_expired_sessions(pool: &SqlitePool) -> AppResult<u64> {
    let result = sqlx::query("DELETE FROM sessions WHERE expires_at <= ?")
        .bind(OffsetDateTime::now_utc())
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

pub async fn login_challenge(pool: &SqlitePool) -> AppResult<String> {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    sqlx::query("DELETE FROM login_challenges WHERE expires_at <= ?")
        .bind(now)
        .execute(pool)
        .await?;
    let token = generate_token()?;
    let result = sqlx::query(
        "INSERT INTO login_challenges (token, expires_at)
        SELECT ?, ? WHERE (SELECT COUNT(*) FROM login_challenges) < 4096",
    )
    .bind(&token)
    .bind(now + 600)
    .execute(pool)
    .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::RateLimited);
    }
    Ok(token)
}

pub async fn consume_login_challenge(pool: &SqlitePool, token: &str) -> AppResult<()> {
    let result = sqlx::query("DELETE FROM login_challenges WHERE token = ? AND expires_at > ?")
        .bind(token)
        .bind(OffsetDateTime::now_utc().unix_timestamp())
        .execute(pool)
        .await?;
    if result.rows_affected() != 1 {
        return Err(AppError::Forbidden);
    }
    Ok(())
}

pub async fn limit_login(pool: &SqlitePool, number: &str) -> AppResult<()> {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    // A global ceiling bounds KDF concurrency; account limits cannot be bypassed by spoofing XFF.
    for (bucket, window, limit) in [
        ("global".to_string(), now / 60, 60),
        (format!("account:{number}"), now / 900, 10),
    ] {
        limit_bucket(pool, &bucket, window, limit).await?;
    }
    sqlx::query("DELETE FROM login_limits WHERE bucket != 'global' AND window < ?")
        .bind(now / 900 - 1)
        .execute(pool)
        .await?;
    Ok(())
}

/// Self-service sign-up ceiling: a global hourly cap plus a per-source cap so
/// one address cannot flood the node with tenant slots.
pub async fn limit_signup(pool: &SqlitePool, src_ip: Option<&str>) -> AppResult<()> {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    limit_bucket(pool, "signup:global", now / 3600, 20).await?;
    if let Some(ip) = src_ip {
        limit_bucket(pool, &format!("signup:ip:{ip}"), now / 3600, 5).await?;
    }
    sqlx::query("DELETE FROM login_limits WHERE bucket LIKE 'signup:%' AND window < ?")
        .bind(now / 3600 - 1)
        .execute(pool)
        .await?;
    Ok(())
}

async fn limit_bucket(pool: &SqlitePool, bucket: &str, window: i64, limit: i64) -> AppResult<()> {
    let attempts: i64 = sqlx::query_scalar(
        "INSERT INTO login_limits (bucket, window, attempts) VALUES (?, ?, 1)
         ON CONFLICT(bucket) DO UPDATE SET
           attempts = CASE WHEN window = excluded.window THEN attempts + 1 ELSE 1 END,
           window = excluded.window RETURNING attempts",
    )
    .bind(bucket)
    .bind(window)
    .fetch_one(pool)
    .await?;
    if attempts > limit {
        return Err(AppError::RateLimited);
    }
    Ok(())
}

pub fn csrf_matches(session: &Session, presented: &str) -> bool {
    constant_time_eq(session.csrf_token.as_bytes(), presented.as_bytes())
}

pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        difference |= a ^ b;
    }
    difference == 0
}
