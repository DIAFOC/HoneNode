use time::OffsetDateTime;

use crate::models::Plan;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    Active,
    Grace,
    Suspended,
    Purgeable,
}

impl Lifecycle {
    pub fn label(self) -> &'static str {
        match self {
            Lifecycle::Active => "Vigente",
            Lifecycle::Grace => "Periodo de gracia",
            Lifecycle::Suspended => "Suspendido",
            Lifecycle::Purgeable => "Listo para purga",
        }
    }

    pub fn is_healthy(self) -> bool {
        matches!(self, Lifecycle::Active)
    }
}

pub fn days_remaining(expires_at: Option<OffsetDateTime>, now: OffsetDateTime) -> Option<i64> {
    expires_at.map(|expiry| (expiry - now).whole_days())
}

pub fn lifecycle(
    expires_at: Option<OffsetDateTime>,
    suspended_at: Option<OffsetDateTime>,
    grace_days: i64,
    retention_days: i64,
    now: OffsetDateTime,
) -> Lifecycle {
    let Some(expires_at) = expires_at else {
        return Lifecycle::Active;
    };

    if now <= expires_at {
        return Lifecycle::Active;
    }
    if now <= expires_at + time::Duration::days(grace_days) {
        return Lifecycle::Grace;
    }
    match suspended_at {
        Some(suspended_at) if now > suspended_at + time::Duration::days(retention_days) => {
            Lifecycle::Purgeable
        }
        _ => Lifecycle::Suspended,
    }
}

pub fn price_for_days(plan: &Plan, days: i64) -> f64 {
    let per_day = plan.monthly_price / 30.0;
    (per_day * days as f64 * 100.0).round() / 100.0
}

pub fn format_money(value: f64) -> String {
    format!("{value:.2}")
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[0])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn format_timestamp(moment: OffsetDateTime) -> String {
    let format = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute] UTC");
    moment.format(&format).unwrap_or_else(|_| "-".to_string())
}

pub fn format_day(moment: OffsetDateTime) -> String {
    let format = time::macros::format_description!("[year]-[month]-[day]");
    moment.format(&format).unwrap_or_else(|_| "-".to_string())
}
