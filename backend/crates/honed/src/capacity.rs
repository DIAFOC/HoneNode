use std::fmt;

use crate::config::Config;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct MemorySize(u64);

impl MemorySize {
    pub fn parse(value: &str) -> Result<Self, String> {
        let digits = value
            .bytes()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if digits == 0 {
            return Err(format!("invalid memory size {value:?}"));
        }

        let number = value[..digits]
            .parse::<u64>()
            .map_err(|_| format!("invalid memory size {value:?}"))?;
        let multiplier = match &value[digits..] {
            "" => 1,
            "K" | "KB" => 1024,
            "M" | "MB" => 1024_u64.pow(2),
            "G" | "GB" => 1024_u64.pow(3),
            "T" | "TB" => 1024_u64.pow(4),
            _ => return Err(format!("invalid memory size {value:?}")),
        };

        number
            .checked_mul(multiplier)
            .map(Self)
            .ok_or_else(|| format!("memory size overflows bytes: {value:?}"))
    }

    pub fn as_bytes(self) -> u64 {
        self.0
    }
}

impl fmt::Display for MemorySize {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapacityPolicy {
    host_total: MemorySize,
    host_reserve: MemorySize,
    tenant_max: MemorySize,
}

impl CapacityPolicy {
    pub fn new(
        host_total: MemorySize,
        host_reserve: MemorySize,
        tenant_max: MemorySize,
    ) -> Result<Self, String> {
        if host_total.as_bytes() == 0 {
            return Err("host memory total must be greater than zero".to_string());
        }
        if host_reserve >= host_total {
            return Err("host memory reserve must be smaller than host memory total".to_string());
        }
        let host_budget = host_total.as_bytes() - host_reserve.as_bytes();
        if tenant_max.as_bytes() == 0 {
            return Err("tenant memory maximum must be greater than zero".to_string());
        }
        if tenant_max.as_bytes() > host_budget {
            return Err("tenant memory maximum exceeds the host admission budget".to_string());
        }

        Ok(Self {
            host_total,
            host_reserve,
            tenant_max,
        })
    }

    fn with_defaults(
        host_total: MemorySize,
        host_reserve: Option<MemorySize>,
        tenant_max: Option<MemorySize>,
    ) -> Result<Self, String> {
        let host_reserve = host_reserve.unwrap_or(MemorySize(host_total.as_bytes() / 4));
        let host_budget = host_total
            .as_bytes()
            .checked_sub(host_reserve.as_bytes())
            .ok_or_else(|| "host memory reserve exceeds host memory total".to_string())?;
        let tenant_max = tenant_max.unwrap_or(MemorySize(host_budget / 2));
        Self::new(host_total, host_reserve, tenant_max)
    }

    pub fn host_budget(self) -> MemorySize {
        MemorySize(self.host_total.as_bytes() - self.host_reserve.as_bytes())
    }

    pub fn tenant_max(self) -> MemorySize {
        self.tenant_max
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Reservation {
    pub tenant: String,
    pub unit: String,
    pub memory_max: MemorySize,
    pub cpu_quota_percent: Option<u32>,
    pub plan_code: Option<String>,
    pub kind: Option<crate::host::ServiceKind>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Admission {
    pub tenant_committed: MemorySize,
    pub tenant_max: MemorySize,
    pub host_committed: MemorySize,
    pub host_budget: MemorySize,
}

pub fn policy(config: &Config) -> Result<CapacityPolicy, String> {
    let meminfo = std::fs::read_to_string("/proc/meminfo")
        .map_err(|error| format!("cannot read /proc/meminfo: {error}"))?;
    let detected_total = parse_meminfo_total(&meminfo)?;
    let configured_total = config
        .host_memory_total
        .as_deref()
        .map(MemorySize::parse)
        .transpose()?;

    if let Some(configured) = configured_total {
        if configured > detected_total {
            return Err(format!(
                "configured host memory total ({configured}) exceeds detected host memory ({detected_total})"
            ));
        }
    }

    let host_total = configured_total.unwrap_or(detected_total);
    let host_reserve = config
        .host_memory_reserve
        .as_deref()
        .map(MemorySize::parse)
        .transpose()?;
    let tenant_max = config
        .tenant_memory_max
        .as_deref()
        .map(MemorySize::parse)
        .transpose()?;

    CapacityPolicy::with_defaults(host_total, host_reserve, tenant_max)
}

fn parse_meminfo_total(contents: &str) -> Result<MemorySize, String> {
    let line = contents
        .lines()
        .find(|line| line.starts_with("MemTotal:"))
        .ok_or_else(|| "MemTotal is missing from /proc/meminfo".to_string())?;
    let mut fields = line.split_whitespace();
    let _label = fields.next();
    let kib = fields
        .next()
        .ok_or_else(|| "MemTotal has no value".to_string())?
        .parse::<u64>()
        .map_err(|_| "MemTotal is not an unsigned integer".to_string())?;
    if fields.next() != Some("kB") || fields.next().is_some() {
        return Err("MemTotal must use kB".to_string());
    }
    let bytes = kib
        .checked_mul(1024)
        .ok_or_else(|| "MemTotal overflows bytes".to_string())?;
    Ok(MemorySize(bytes))
}

/// Strict admission (no overcommit): the historical behaviour.
pub fn admit(
    policy: CapacityPolicy,
    reservations: &[Reservation],
    tenant: &str,
    unit: &str,
    requested: MemorySize,
) -> Result<Admission, String> {
    admit_with_overcommit(policy, reservations, tenant, unit, requested, 100)
}

/// Admission with a soft budget and a hard overcommit ceiling. MemoryHigh
/// throttles and MemoryMax caps each cgroup, so exceeding the soft budget is a
/// warning; only the hard ceiling refuses.
pub fn admit_with_overcommit(
    policy: CapacityPolicy,
    reservations: &[Reservation],
    tenant: &str,
    unit: &str,
    requested: MemorySize,
    overcommit_percent: u32,
) -> Result<Admission, String> {
    if requested.as_bytes() == 0 {
        return Err("service MemoryMax must be greater than zero".to_string());
    }

    let mut replaced = false;
    let mut tenant_existing = 0_u64;
    let mut host_existing = 0_u64;

    for reservation in reservations {
        if reservation.tenant == tenant && reservation.unit == unit {
            if replaced {
                return Err(format!("duplicate reservation for {tenant}/{unit}"));
            }
            replaced = true;
            continue;
        }

        host_existing = host_existing
            .checked_add(reservation.memory_max.as_bytes())
            .ok_or_else(|| "existing host memory reservations overflow bytes".to_string())?;
        if reservation.tenant == tenant {
            tenant_existing = tenant_existing
                .checked_add(reservation.memory_max.as_bytes())
                .ok_or_else(|| "existing tenant memory reservations overflow bytes".to_string())?;
        }
    }

    let tenant_committed = tenant_existing
        .checked_add(requested.as_bytes())
        .ok_or_else(|| "requested tenant memory reservation overflows bytes".to_string())?;
    let host_committed = host_existing
        .checked_add(requested.as_bytes())
        .ok_or_else(|| "requested host memory reservation overflows bytes".to_string())?;
    let host_budget = policy.host_budget().as_bytes();
    // Soft budgets are the target; the hard ceiling allows deliberate
    // overcommit (like Kubernetes requests vs limits) so admission never
    // becomes a brick wall. The kernel still throttles (MemoryHigh) and caps
    // (MemoryMax) each cgroup, so the host is not starved by construction.
    let factor = u64::from(overcommit_percent.max(100));
    let tenant_soft = policy.tenant_max().as_bytes();
    let tenant_hard = tenant_soft.saturating_mul(factor) / 100;
    let host_hard = host_budget.saturating_mul(factor) / 100;

    if tenant_committed > tenant_hard {
        return Err(format!(
            "tenant memory admission refused for {tenant}: {tenant_committed} bytes requested committed total, hard ceiling {tenant_hard} bytes (soft {tenant_soft}, overcommit {overcommit_percent}%)"
        ));
    }
    if host_committed > host_hard {
        return Err(format!(
            "host memory admission refused: {host_committed} bytes requested committed total, hard ceiling {host_hard} bytes (soft {host_budget}, overcommit {overcommit_percent}%; host total {} bytes, reserve {} bytes)",
            policy.host_total.as_bytes(),
            policy.host_reserve.as_bytes()
        ));
    }
    if tenant_committed > tenant_soft {
        tracing::warn!(
            tenant,
            tenant_committed,
            tenant_soft,
            "tenant memory above its soft budget; MemoryHigh throttles and MemoryMax caps each cgroup"
        );
    }
    if host_committed > host_budget {
        tracing::warn!(
            host_committed,
            host_budget,
            "host memory above its soft budget; per-cgroup limits keep the host from starving"
        );
    }

    Ok(Admission {
        tenant_committed: MemorySize(tenant_committed),
        tenant_max: policy.tenant_max(),
        host_committed: MemorySize(host_committed),
        host_budget: policy.host_budget(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(value: &str) -> MemorySize {
        MemorySize::parse(value).unwrap()
    }

    fn reservation(tenant: &str, unit: &str, memory_max: &str) -> Reservation {
        Reservation {
            tenant: tenant.to_string(),
            unit: unit.to_string(),
            memory_max: size(memory_max),
            cpu_quota_percent: None,
            plan_code: None,
            kind: None,
        }
    }

    #[test]
    fn parses_strict_binary_memory_sizes() {
        assert_eq!(size("384M").as_bytes(), 384 * 1024 * 1024);
        assert_eq!(size("1G").as_bytes(), 1024 * 1024 * 1024);
        assert_eq!(size("2KB").as_bytes(), 2048);
        assert_eq!(size("42").as_bytes(), 42);
    }

    #[test]
    fn rejects_ambiguous_malformed_and_overflowing_memory_sizes() {
        for value in [
            "",
            "M",
            "-1G",
            "+1G",
            "1.5G",
            "1g",
            "1GiB",
            "1G\nCPUQuota=100%",
            "inf",
            "18446744073709551615T",
        ] {
            assert!(MemorySize::parse(value).is_err(), "accepted {value:?}");
        }
    }

    #[test]
    fn rejects_tenant_overcommit_even_when_host_has_capacity() {
        let policy = CapacityPolicy::new(size("16G"), size("2G"), size("4G")).unwrap();
        let existing = [reservation("t01", "t01-db", "3G")];
        let error = admit(policy, &existing, "t01", "t01-web", size("2G")).unwrap_err();
        assert!(error.contains("tenant memory admission refused"));
    }

    #[test]
    fn rejects_host_overcommit_across_tenants() {
        let policy = CapacityPolicy::new(size("8G"), size("2G"), size("6G")).unwrap();
        let existing = [
            reservation("t01", "t01-db", "3G"),
            reservation("t02", "t02-web", "2G"),
        ];
        let error = admit(policy, &existing, "t01", "t01-web", size("2G")).unwrap_err();
        assert!(error.contains("host memory admission refused"));
    }

    #[test]
    fn overcommit_admits_above_the_soft_budget_and_refuses_beyond_the_ceiling() {
        let policy = CapacityPolicy::new(size("8G"), size("2G"), size("6G")).unwrap();
        // Host soft budget 6G: 5G existing + 2G requested exceeds it, but stays
        // below the 2x hard ceiling, so overcommit admits it.
        let existing = [reservation("t02", "t02-web", "5G")];
        assert!(admit(policy, &existing, "t01", "t01-web", size("2G")).is_err());
        assert!(admit_with_overcommit(policy, &existing, "t01", "t01-web", size("2G"), 200).is_ok());
        // 11G + 2G exceeds the 12G hard ceiling and is still refused.
        let too_much = [reservation("t02", "t02-web", "11G")];
        assert!(
            admit_with_overcommit(policy, &too_much, "t01", "t01-web", size("2G"), 200).is_err()
        );
    }

    #[test]
    fn update_replaces_instead_of_double_counting_the_same_unit() {
        let policy = CapacityPolicy::new(size("8G"), size("2G"), size("4G")).unwrap();
        let existing = [
            reservation("t01", "t01-db", "1G"),
            reservation("t02", "t02-web", "2G"),
        ];
        let admission = admit(policy, &existing, "t01", "t01-db", size("3G")).unwrap();
        assert_eq!(admission.tenant_committed, size("3G"));
        assert_eq!(admission.host_committed, size("5G"));
    }

    #[test]
    fn exact_fit_is_admitted_and_one_byte_more_is_refused() {
        let policy = CapacityPolicy::new(size("4G"), size("1G"), size("3G")).unwrap();
        assert!(admit(policy, &[], "t01", "t01-web", size("3G")).is_ok());
        let over = MemorySize(size("3G").as_bytes() + 1);
        assert!(admit(policy, &[], "t01", "t01-web", over).is_err());
    }

    #[test]
    fn arithmetic_overflow_fails_closed() {
        let policy =
            CapacityPolicy::new(MemorySize(u64::MAX), MemorySize(0), MemorySize(u64::MAX)).unwrap();
        let existing = [reservation("t02", "t02-web", "1")];
        let requested = MemorySize(u64::MAX);
        assert!(admit(policy, &existing, "t01", "t01-web", requested).is_err());
    }

    #[test]
    fn invalid_policy_relationships_are_rejected() {
        assert!(CapacityPolicy::new(size("4G"), size("4G"), size("1G")).is_err());
        assert!(CapacityPolicy::new(size("4G"), size("1G"), size("4G")).is_err());
        assert!(CapacityPolicy::new(size("4G"), size("1G"), size("0")).is_err());
    }

    #[test]
    fn duplicate_unit_reservations_fail_closed() {
        let policy = CapacityPolicy::new(size("8G"), size("2G"), size("4G")).unwrap();
        let existing = [
            reservation("t01", "t01-web", "1G"),
            reservation("t01", "t01-web", "1G"),
        ];
        assert!(admit(policy, &existing, "t01", "t01-web", size("1G")).is_err());
    }

    #[test]
    fn defaults_reserve_a_quarter_of_host_and_half_budget_per_tenant() {
        let policy = CapacityPolicy::with_defaults(size("8G"), None, None).unwrap();
        assert_eq!(policy.host_budget(), size("6G"));
        assert_eq!(policy.tenant_max(), size("3G"));
    }

    #[test]
    fn meminfo_total_is_parsed_with_checked_kib_conversion() {
        let total = parse_meminfo_total("MemFree: 1 kB\nMemTotal: 2048 kB\n").unwrap();
        assert_eq!(total, size("2M"));
        assert!(parse_meminfo_total("MemTotal: nope kB\n").is_err());
        assert!(parse_meminfo_total("MemTotal: 2048 MB\n").is_err());
    }
}
