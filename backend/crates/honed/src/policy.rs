use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

use serde::{Deserialize, Serialize};

use crate::capacity::{MemorySize, Reservation};
use crate::config::Config;
use crate::host::{Envelope, ServiceKind};
use crate::Request;

const MAX_POLICY_BYTES: u64 = 64 * 1024;
const DEFAULT_SERVICE_DAYS: u32 = 30;
/// Wildcard tenant entry: entitles every otherwise unlisted tenant of the
/// platform shape (`t` + two digits). Exact entries always take precedence.
const WILDCARD_TENANT: &str = "t*";

fn default_service_days() -> u32 {
    DEFAULT_SERVICE_DAYS
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    version: u32,
    #[serde(default)]
    pub allowed_registries: Vec<String>,
    host_cpu_quota_percent: u32,
    max_services_per_tenant: u32,
    plans: Vec<Plan>,
    tenants: Vec<TenantPolicy>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub code: String,
    allowed_kinds: Vec<ServiceKind>,
    cpu_weight: i64,
    memory_high: String,
    memory_max: String,
    tasks_max: i64,
    cpu_quota_percent: u32,
    pub max_services: u32,
    #[serde(default = "default_service_days")]
    pub service_days: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantPolicy {
    tenant_id: String,
    allowed_plans: Vec<String>,
}

pub fn load(config: &Config) -> Result<Policy, String> {
    let path = &config.policy_file;
    crate::host::validate_root_owned_path(path)
        .map_err(|error| format!("broker policy unavailable or untrusted: {error}"))?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| format!("cannot open broker policy {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("policy metadata: {error}"))?;
    if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        return Err(
            "broker policy must be a root-owned regular file, not writable by group/other"
                .to_string(),
        );
    }
    let mut body = String::new();
    file.take(MAX_POLICY_BYTES + 1)
        .read_to_string(&mut body)
        .map_err(|error| format!("cannot read broker policy: {error}"))?;
    if body.len() as u64 > MAX_POLICY_BYTES {
        return Err("broker policy exceeds 64 KiB".to_string());
    }
    let cpu_count = std::thread::available_parallelism()
        .map_err(|error| format!("cannot determine host CPU capacity: {error}"))?
        .get();
    let detected_cpu_percent = u32::try_from(cpu_count)
        .ok()
        .and_then(|count| count.checked_mul(100))
        .ok_or_else(|| "detected CPU capacity overflow".to_string())?;
    Policy::parse(&body, detected_cpu_percent)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CatalogEntry {
    pub plan_code: String,
    pub memory_high: String,
    pub memory_max: String,
    pub cpu_quota_percent: u32,
    pub tasks_max: i64,
    pub max_services: u32,
    pub service_days: u32,
    pub allowed_kinds: Vec<ServiceKind>,
    pub images: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub registries: Vec<String>,
}

pub fn valid_code(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

impl Policy {
    fn parse(body: &str, detected_cpu_percent: u32) -> Result<Self, String> {
        let policy: Self = serde_json::from_str(body)
            .map_err(|error| format!("invalid broker policy JSON: {error}"))?;
        if policy.version != 1 {
            return Err("unsupported broker policy version".to_string());
        }
        if policy.host_cpu_quota_percent == 0
            || policy.host_cpu_quota_percent > detected_cpu_percent
        {
            return Err(
                "host CPU budget must be positive and no greater than detected CPU capacity"
                    .to_string(),
            );
        }
        if policy.max_services_per_tenant == 0
            || policy.max_services_per_tenant > 1000
            || policy.plans.is_empty()
            || policy.plans.len() > 128
            || policy.tenants.len() > 100
        {
            return Err("invalid broker policy collection or service-count limits".to_string());
        }
        if policy.allowed_registries.len() > 128
            || policy.allowed_registries.iter().enumerate().any(|(i, registry)| {
                !crate::containers::valid_registry(registry)
                    || policy.allowed_registries[..i].contains(registry)
            })
        {
            return Err("invalid or duplicate allowed_registries".to_string());
        }
        let mut codes = std::collections::HashSet::new();
        for plan in &policy.plans {
            if !valid_code(&plan.code) || !codes.insert(plan.code.as_str()) {
                return Err("invalid or duplicate policy plan code".to_string());
            }
            if plan.allowed_kinds.is_empty()
                || plan.allowed_kinds.len() > 5
                || !(1..=366).contains(&plan.service_days)
                || plan.max_services == 0
                || plan.max_services > policy.max_services_per_tenant
                || plan.cpu_quota_percent == 0
                || plan.cpu_quota_percent > policy.host_cpu_quota_percent
            {
                return Err(format!("invalid policy limits for plan {}", plan.code));
            }
            for (index, kind) in plan.allowed_kinds.iter().enumerate() {
                if plan.allowed_kinds[..index].contains(kind) {
                    return Err(format!("duplicate kind for plan {}", plan.code));
                }
                plan.envelope(*kind)?;
            }
        }
        let mut tenants = std::collections::HashSet::new();
        for tenant in &policy.tenants {
            if (tenant.tenant_id != WILDCARD_TENANT && !crate::shape_is_valid(&tenant.tenant_id))
                || !tenants.insert(&tenant.tenant_id)
                || tenant.allowed_plans.is_empty()
                || tenant.allowed_plans.len() > policy.plans.len()
            {
                return Err("invalid or duplicate policy tenant".to_string());
            }
            let mut allowed = std::collections::HashSet::new();
            for code in &tenant.allowed_plans {
                if !codes.contains(code.as_str()) || !allowed.insert(code) {
                    return Err("unknown or duplicate tenant plan entitlement".to_string());
                }
            }
        }
        Ok(policy)
    }

    /// Exact tenant entries take precedence; the `t*` wildcard entitles any
    /// remaining tenant of the platform shape (self-service sign-ups).
    fn entitlement(&self, tenant: &str) -> Option<&TenantPolicy> {
        self.tenants
            .iter()
            .find(|entry| entry.tenant_id == tenant)
            .or_else(|| {
                self.tenants
                    .iter()
                    .find(|entry| entry.tenant_id == WILDCARD_TENANT)
            })
    }

    pub fn require_tenant(&self, tenant: &str) -> Result<(), String> {
        self.entitlement(tenant)
            .map(|_| ())
            .ok_or_else(|| "tenant has no broker policy entitlement".to_string())
    }

    pub fn catalog(&self, tenant: &str, config: &Config) -> Result<Vec<CatalogEntry>, String> {
        self.require_tenant(tenant)?;
        let entitlement = self
            .entitlement(tenant)
            .ok_or_else(|| "tenant has no broker policy entitlement".to_string())?;
        Ok(self
            .plans
            .iter()
            .filter(|plan| entitlement.allowed_plans.contains(&plan.code))
            .filter_map(|plan| {
                // Every kind the plan allows is offered, including the fixed
                // Forgejo (git) template: the operator policy is the authority.
                let allowed_kinds: Vec<_> = plan.allowed_kinds.clone();
                if allowed_kinds.is_empty() {
                    return None;
                }
                let images = allowed_kinds
                    .iter()
                    .filter(|kind| **kind != ServiceKind::Container)
                    .map(|kind| (kind.as_str().to_string(), config.image_for(*kind).to_string()))
                    .collect();
                Some(CatalogEntry {
                    plan_code: plan.code.clone(),
                    memory_high: plan.memory_high.clone(),
                    memory_max: plan.memory_max.clone(),
                    cpu_quota_percent: plan.cpu_quota_percent,
                    tasks_max: plan.tasks_max,
                    max_services: plan.max_services,
                    service_days: plan.service_days,
                    allowed_kinds,
                    images,
                    registries: self.allowed_registries.clone(),
                })
            })
            .collect())
    }

    fn plan(&self, tenant: &str, code: &str) -> Result<&Plan, String> {
        if !valid_code(code) {
            return Err("invalid plan_code".to_string());
        }
        self.require_tenant(tenant)?;
        let entitlement = self
            .entitlement(tenant)
            .ok_or_else(|| "tenant has no broker policy entitlement".to_string())?;
        if !entitlement
            .allowed_plans
            .iter()
            .any(|allowed| allowed == code)
        {
            return Err("plan is not allowed for this tenant by broker policy".to_string());
        }
        self.plans
            .iter()
            .find(|plan| plan.code == code)
            .ok_or_else(|| "unknown policy plan".to_string())
    }

    pub fn resolve(&self, request: &Request) -> Result<Envelope, String> {
        if request.service_days.is_some() {
            return Err("service_days is fixed by operator policy, not tenant requests".to_string());
        }
        let code = request.plan_code.as_deref().ok_or_else(|| {
            "missing plan_code; raw limits cannot bypass broker policy".to_string()
        })?;
        let tenant = request
            .tenant_id
            .as_deref()
            .ok_or_else(|| "missing tenant_id".to_string())?;
        let kind = ServiceKind::parse(
            request
                .kind
                .as_deref()
                .ok_or_else(|| "missing kind".to_string())?,
        )?;
        let envelope = self.plan(tenant, code)?.envelope(kind)?;
        if request
            .cpu_weight
            .is_some_and(|value| value != envelope.cpu_weight)
            || request
                .tasks_max
                .is_some_and(|value| value != envelope.tasks_max)
            || request
                .cpu_quota_percent
                .is_some_and(|value| value != envelope.cpu_quota_percent)
        {
            return Err("raw CPU/task fields do not match broker policy plan".to_string());
        }
        for (raw, expected) in [
            (&request.memory_high, envelope.memory_high),
            (&request.memory_max, envelope.memory_max),
        ] {
            if let Some(value) = raw {
                if MemorySize::parse(value)? != expected {
                    return Err("raw memory fields do not match broker policy plan".to_string());
                }
            }
        }
        Ok(envelope)
    }

    /// Strict admission (no overcommit): the historical behaviour.
    pub fn admit(
        &self,
        reservations: &[Reservation],
        tenant: &str,
        unit: &str,
        requested: &Envelope,
    ) -> Result<(), String> {
        self.admit_with_overcommit(reservations, tenant, unit, requested, 100)
    }

    /// Admission with a soft budget and a hard overcommit ceiling. CPUQuota
    /// throttles each cgroup and never kills it, so exceeding the soft budget
    /// is a warning; only the hard ceiling refuses.
    pub fn admit_with_overcommit(
        &self,
        reservations: &[Reservation],
        tenant: &str,
        unit: &str,
        requested: &Envelope,
        overcommit_percent: u32,
    ) -> Result<(), String> {
        let code = requested
            .plan_code
            .as_deref()
            .ok_or_else(|| "missing committed plan code".to_string())?;
        let plan = self.plan(tenant, code)?;
        let mut host_cpu = u64::from(requested.cpu_quota_percent);
        let mut tenant_count = 1_u32;
        let mut seen = std::collections::HashSet::new();
        for existing in reservations {
            if !seen.insert((&existing.tenant, &existing.unit)) {
                return Err("duplicate committed unit reservation".to_string());
            }
            if existing.tenant == tenant && existing.unit == unit {
                continue;
            }
            if existing.plan_code.is_none() {
                return Err(
                    "legacy reservation has no plan; stop and migrate units before admission"
                        .to_string(),
                );
            }
            let cpu = existing.cpu_quota_percent.ok_or_else(|| {
                "legacy reservation has no CPUQuota; migration required".to_string()
            })?;
            if cpu == 0 {
                return Err("zero CPU reservation".to_string());
            }
            host_cpu = host_cpu
                .checked_add(u64::from(cpu))
                .ok_or_else(|| "host CPU reservation overflow".to_string())?;
            if existing.tenant == tenant {
                // Multiple instances of the same kind are allowed; the plan's
                // service count and the host budgets are the real limits.
                let _ = existing.kind.ok_or_else(|| {
                    "legacy reservation has no kind; stop and migrate units before admission"
                        .to_string()
                })?;
                tenant_count = tenant_count
                    .checked_add(1)
                    .ok_or_else(|| "tenant service count overflow".to_string())?;
            }
        }
        // The selected plan caps all committed tenant services, not only services
        // carrying the same plan code; otherwise changing plans bypasses its cap.
        if tenant_count > self.max_services_per_tenant || tenant_count > plan.max_services {
            return Err("broker policy service-count admission refused".to_string());
        }
        // CPUQuota throttles each cgroup and never kills it, so the host CPU
        // budget is a soft target: admission only refuses beyond the hard
        // overcommit ceiling.
        let soft = u64::from(self.host_cpu_quota_percent);
        let hard = soft.saturating_mul(u64::from(overcommit_percent.max(100))) / 100;
        if host_cpu > hard {
            return Err(format!(
                "host CPU admission refused: {host_cpu}% committed, hard ceiling {hard}% (soft {soft}%, overcommit {overcommit_percent}%)"
            ));
        }
        if host_cpu > soft {
            tracing::warn!(
                host_cpu,
                soft,
                "host CPU above its soft budget; per-service CPUQuota throttles instead of killing"
            );
        }
        Ok(())
    }

    pub fn validate_activation(
        &self,
        reservation: &Reservation,
        body: &str,
    ) -> Result<Envelope, String> {
        let code = reservation.plan_code.as_deref().ok_or_else(|| {
            "unit has no broker plan; stop and migrate before activation".to_string()
        })?;
        let kind = crate::host::unit_directive(body, "# HonedKind=")?;
        if reservation
            .kind
            .is_some_and(|committed| committed.as_str() != kind)
        {
            return Err("unit kind marker does not match its committed reservation".to_string());
        }
        let request = Request {
            tenant_id: Some(reservation.tenant.clone()),
            plan_code: Some(code.to_string()),
            kind: Some(kind.to_string()),
            cpu_weight: Some(
                crate::host::unit_directive(body, "CPUWeight=")?
                    .parse()
                    .map_err(|_| "invalid unit CPUWeight".to_string())?,
            ),
            tasks_max: Some(
                crate::host::unit_directive(body, "TasksMax=")?
                    .parse()
                    .map_err(|_| "invalid unit TasksMax".to_string())?,
            ),
            memory_high: Some(crate::host::unit_directive(body, "MemoryHigh=")?.to_string()),
            memory_max: Some(reservation.memory_max.to_string()),
            cpu_quota_percent: reservation.cpu_quota_percent,
            ..Request::default()
        };
        if request.cpu_quota_percent.is_none() {
            return Err("unit has no CPUQuota; migration required".to_string());
        }
        self.resolve(&request)
    }
}

impl Plan {
    fn envelope(&self, kind: ServiceKind) -> Result<Envelope, String> {
        if !self.allowed_kinds.contains(&kind) {
            return Err("service kind is not allowed by this policy plan".to_string());
        }
        let mut envelope = Envelope::new(
            kind.as_str(),
            self.cpu_weight,
            &self.memory_high,
            &self.memory_max,
            self.tasks_max,
        )?;
        envelope.cpu_quota_percent = self.cpu_quota_percent;
        envelope.plan_code = Some(self.code.clone());
        Ok(envelope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example() -> Policy {
        Policy::parse(include_str!("../policy.example.json"), 400).unwrap()
    }

    fn request() -> Request {
        serde_json::from_str(r#"{"op":"limits.set","tenant_id":"t01","service":"t01-site","kind":"web","plan_code":"starter"}"#).unwrap()
    }

    fn reservation(tenant: &str, unit: &str, plan: &str, cpu: u32) -> Reservation {
        Reservation {
            tenant: tenant.to_string(),
            unit: unit.to_string(),
            plan_code: Some(plan.to_string()),
            memory_max: MemorySize::parse("384M").unwrap(),
            cpu_quota_percent: Some(cpu),
            kind: Some(ServiceKind::Web),
        }
    }

    #[test]
    fn example_policy_resolves_limits_and_hard_cpu_quota() {
        let envelope = example().resolve(&request()).unwrap();
        assert_eq!(envelope.cpu_weight, 100);
        assert_eq!(envelope.cpu_quota_percent, 25);
        assert_eq!(envelope.memory_max, MemorySize::parse("384M").unwrap());
        assert_eq!(envelope.plan_code.as_deref(), Some("starter"));
    }

    #[test]
    fn selfservice_wildcard_entitles_unlisted_tenants_and_exact_entries_win() {
        let policy = Policy::parse(
            include_str!("../policy.selfservice.example.json"),
            800,
        )
        .unwrap();
        // An allowlisted but policy-unlisted tenant is entitled through the wildcard.
        policy.require_tenant("t42").unwrap();
        let catalog = policy.catalog("t42", &crate::config::Config::from_env()).unwrap();
        let codes: Vec<_> = catalog.iter().map(|entry| entry.plan_code.as_str()).collect();
        assert!(codes.contains(&"starter"));
        assert!(codes.contains(&"pro"));
        assert_eq!(catalog.iter().find(|e| e.plan_code == "starter").unwrap().max_services, 2);
        assert_eq!(catalog.iter().find(|e| e.plan_code == "starter").unwrap().service_days, 7);
        assert_eq!(catalog.iter().find(|e| e.plan_code == "pro").unwrap().max_services, 6);

        // An exact entry overrides the wildcard for the same tenant.
        let mixed = Policy::parse(
            r#"{
                "version": 1,
                "host_cpu_quota_percent": 300,
                "max_services_per_tenant": 6,
                "plans": [
                    {"code":"starter","allowed_kinds":["web"],"cpu_weight":100,
                     "memory_high":"256M","memory_max":"384M","tasks_max":128,
                     "cpu_quota_percent":25,"max_services":2,"service_days":7},
                    {"code":"pro","allowed_kinds":["web"],"cpu_weight":300,
                     "memory_high":"1G","memory_max":"1536M","tasks_max":512,
                     "cpu_quota_percent":150,"max_services":4,"service_days":30}
                ],
                "tenants": [
                    {"tenant_id":"t01","allowed_plans":["pro"]},
                    {"tenant_id":"t*","allowed_plans":["starter"]}
                ]
            }"#,
            400,
        )
        .unwrap();
        mixed.require_tenant("t01").unwrap();
        let catalog = mixed.catalog("t01", &crate::config::Config::from_env()).unwrap();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].plan_code, "pro");
        // The wildcard still covers everyone else.
        mixed.require_tenant("t77").unwrap();
        let catalog = mixed.catalog("t77", &crate::config::Config::from_env()).unwrap();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].plan_code, "starter");
    }

    #[test]
    fn wildcard_entry_is_rejected_when_malformed_or_duplicated() {
        // A wildcard with an unknown plan code is refused.
        assert!(Policy::parse(
            r#"{
                "version": 1,
                "host_cpu_quota_percent": 300,
                "max_services_per_tenant": 6,
                "plans": [
                    {"code":"starter","allowed_kinds":["web"],"cpu_weight":100,
                     "memory_high":"256M","memory_max":"384M","tasks_max":128,
                     "cpu_quota_percent":25,"max_services":2,"service_days":7}
                ],
                "tenants": [{"tenant_id":"t*","allowed_plans":["missing"]}]
            }"#,
            400,
        )
        .is_err());
        // Duplicate wildcard entries are refused.
        assert!(Policy::parse(
            r#"{
                "version": 1,
                "host_cpu_quota_percent": 300,
                "max_services_per_tenant": 6,
                "plans": [
                    {"code":"starter","allowed_kinds":["web"],"cpu_weight":100,
                     "memory_high":"256M","memory_max":"384M","tasks_max":128,
                     "cpu_quota_percent":25,"max_services":2,"service_days":7}
                ],
                "tenants": [
                    {"tenant_id":"t*","allowed_plans":["starter"]},
                    {"tenant_id":"t*","allowed_plans":["starter"]}
                ]
            }"#,
            400,
        )
        .is_err());
    }

    #[test]
    fn raw_limits_cannot_bypass_or_override_policy() {
        let policy = example();
        let mut request = request();
        request.memory_max = Some("1G".to_string());
        assert!(policy.resolve(&request).is_err());
        request.memory_max = Some("402653184".to_string());
        assert!(policy.resolve(&request).is_ok());
        request.cpu_quota_percent = Some(100);
        assert!(policy.resolve(&request).is_err());
        request.cpu_quota_percent = None;
        request.cpu_weight = Some(10001);
        assert!(policy.resolve(&request).is_err());
        request.cpu_weight = None;
        request.tasks_max = Some(1000000);
        assert!(policy.resolve(&request).is_err());
        request.plan_code = None;
        assert!(policy.resolve(&request).is_err());
    }

    #[test]
    fn plan_kind_and_tenant_entitlements_are_independent_allowlists() {
        let policy = example();
        let mut request = request();
        request.kind = Some("database".to_string());
        assert!(policy.resolve(&request).is_err());
        request.kind = Some("web".to_string());
        request.tenant_id = Some("t03".to_string());
        assert!(policy.resolve(&request).is_err());
        request.tenant_id = Some("t99".to_string());
        assert!(policy.resolve(&request).is_err());
        request.tenant_id = Some("t01".to_string());
        request.plan_code = Some("missing".to_string());
        assert!(policy.resolve(&request).is_err());
    }

    #[test]
    fn cpu_admission_sums_other_tenants_and_allows_exact_fit() {
        let policy = example();
        let envelope = policy.resolve(&request()).unwrap();
        let mut existing = vec![reservation("t02", "t02-db", "pro", 275)];
        assert!(policy
            .admit(&existing, "t01", "t01-site", &envelope)
            .is_ok());
        existing[0].cpu_quota_percent = Some(276);
        assert!(policy
            .admit(&existing, "t01", "t01-site", &envelope)
            .unwrap_err()
            .contains("host CPU"));
    }

    #[test]
    fn cpu_overcommit_admits_above_the_soft_budget_and_refuses_beyond_the_ceiling() {
        // The example policy has a 300% soft CPU budget.
        let policy = example();
        let envelope = policy.resolve(&request()).unwrap(); // starter: 25%
        // 300% existing + 25% requested = 325%: above soft, below the 2x ceiling.
        let existing: Vec<_> = (0..12)
            .map(|index| reservation("t02", &format!("t02-{index}"), "starter", 25))
            .collect();
        assert!(policy.admit(&existing, "t01", "t01-site", &envelope).is_err());
        assert!(policy
            .admit_with_overcommit(&existing, "t01", "t01-site", &envelope, 200)
            .is_ok());
        // 600% existing + 25% requested = 625%: beyond the 600% hard ceiling.
        let too_much: Vec<_> = (0..24)
            .map(|index| reservation("t02", &format!("t02-{index}"), "starter", 25))
            .collect();
        assert!(policy
            .admit_with_overcommit(&too_much, "t01", "t01-site", &envelope, 200)
            .is_err());
    }

    #[test]
    fn same_unit_update_replaces_its_cpu_and_count_reservation() {
        let policy = example();
        let envelope = policy.resolve(&request()).unwrap();
        let existing = [reservation("t01", "t01-site", "starter", 300)];
        assert!(policy
            .admit(&existing, "t01", "t01-site", &envelope)
            .is_ok());
    }

    #[test]
    fn count_limits_cannot_be_bypassed_by_new_unit_names() {
        let policy = example();
        let envelope = policy.resolve(&request()).unwrap();
        assert!(policy
            .admit(
                &[reservation("t01", "t01-old", "starter", 25)],
                "t01",
                "t01-site",
                &envelope
            )
            .is_err());
        let existing: Vec<_> = (0..6)
            .map(|index| reservation("t01", &format!("t01-unit{index}"), "standard", 1))
            .collect();
        assert!(policy
            .admit(&existing, "t01", "t01-site", &envelope)
            .is_err());
    }

    #[test]
    fn selected_plan_cap_spans_all_tenant_plan_codes() {
        let policy = example();
        let starter = policy.resolve(&request()).unwrap();
        let mut existing_database = reservation("t01", "t01-db", "standard", 75);
        existing_database.kind = Some(ServiceKind::Database);
        // Starter permits one service total. Its previous per-plan counter would
        // have allowed this second service because the existing unit is standard.
        assert!(policy
            .admit(
                &[existing_database],
                "t01",
                "t01-site",
                &starter
            )
            .is_err());

        let mut standard_request = request();
        standard_request.plan_code = Some("standard".to_string());
        standard_request.kind = Some("database".to_string());
        standard_request.service = Some("t01-db".to_string());
        let standard = policy.resolve(&standard_request).unwrap();
        let existing_web = reservation("t01", "t01-site", "starter", 25);
        assert!(policy
            .admit(&[existing_web], "t01", "t01-db", &standard)
            .is_ok());
    }

    #[test]
    fn unknown_cpu_or_plan_reservations_fail_closed() {
        let policy = example();
        let envelope = policy.resolve(&request()).unwrap();
        let mut existing = reservation("t02", "t02-db", "standard", 75);
        existing.cpu_quota_percent = None;
        assert!(policy
            .admit(&[existing.clone()], "t01", "t01-site", &envelope)
            .is_err());
        existing.cpu_quota_percent = Some(75);
        existing.plan_code = None;
        assert!(policy
            .admit(&[existing], "t01", "t01-site", &envelope)
            .is_err());
    }

    #[test]
    fn policy_schema_rejects_unknown_fields_duplicates_and_invalid_limits() {
        let original: serde_json::Value =
            serde_json::from_str(include_str!("../policy.example.json")).unwrap();
        for (field, value) in [
            ("version", serde_json::json!(2)),
            ("host_cpu_quota_percent", serde_json::json!(0)),
            ("max_services_per_tenant", serde_json::json!(0)),
            ("allow_tenant_delete", serde_json::json!(true)),
        ] {
            let mut changed = original.clone();
            changed[field] = value;
            assert!(Policy::parse(&changed.to_string(), 400).is_err());
        }
        for (field, value) in [
            ("cpu_quota_percent", serde_json::json!(0)),
            ("max_services", serde_json::json!(1001)),
            ("memory_max", serde_json::json!("infinity")),
            ("allowed_kinds", serde_json::json!(["shell"])),
        ] {
            let mut changed = original.clone();
            changed["plans"][0][field] = value;
            assert!(Policy::parse(&changed.to_string(), 400).is_err());
        }
        let mut changed = original.clone();
        changed["plans"][1]["code"] = serde_json::json!("starter");
        assert!(Policy::parse(&changed.to_string(), 400).is_err());
        assert!(Policy::parse(include_str!("../policy.example.json"), 200).is_err());
    }

    #[test]
    fn activation_requires_full_policy_match_not_only_a_plan_marker() {
        let policy = example();
        let record = reservation("t01", "t01-site", "starter", 25);
        let body = "# HonedKind=web\nCPUWeight=100\nMemoryHigh=256M\nTasksMax=128\n";
        assert!(policy.validate_activation(&record, body).is_ok());
        let mut bad = record.clone();
        bad.cpu_quota_percent = Some(26);
        assert!(policy.validate_activation(&bad, body).is_err());
        assert!(policy
            .validate_activation(&record, &body.replace("256M", "1G"))
            .is_err());
    }

    #[test]
    fn catalog_is_tenant_entitled_and_service_days_are_operator_controlled() {
        let policy = example();
        let catalog = policy.catalog("t02", &Config::from_env()).unwrap();
        assert_eq!(catalog.len(), 2);
        assert_eq!(catalog[0].plan_code, "standard");
        assert_eq!(catalog[0].service_days, 30);
        // The plan's allowed kinds are offered as-is, including the fixed
        // Forgejo (git) template.
        assert_eq!(
            catalog[0].allowed_kinds,
            vec![ServiceKind::Web, ServiceKind::Database, ServiceKind::Git]
        );
        assert!(!catalog[0].images.contains_key("storage"));
        assert_eq!(
            catalog[0].images.get("git"),
            Some(&Config::from_env().image_forgejo)
        );
        let expected_image = Config::from_env().image_postgres;
        assert_eq!(catalog[0].images.get("database"), Some(&expected_image));
        assert!(policy.catalog("t99", &Config::from_env()).is_err());
        assert!(catalog.iter().all(|entry| {
            !entry.allowed_kinds.is_empty()
                && entry.allowed_kinds.iter().all(|kind| matches!(
                    kind,
                    ServiceKind::Web | ServiceKind::Database | ServiceKind::Git
                ))
        }));
    }

    #[test]
    fn storage_and_git_are_cataloged_with_their_approved_images() {
        let policy = example();
        let config = Config {
            image_filebrowser: "registry.example/filebrowser/filebrowser:v2.63.23".to_string(),
            ..Config::from_env()
        };
        let catalog = policy.catalog("t03", &config).unwrap();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].plan_code, "vault");
        assert_eq!(catalog[0].allowed_kinds, vec![ServiceKind::Storage]);
        assert_eq!(
            catalog[0].images,
            std::collections::BTreeMap::from([(
                "storage".to_string(),
                "registry.example/filebrowser/filebrowser:v2.63.23".to_string()
            )])
        );
        for tenant in ["t01", "t02", "t04"] {
            for entry in policy.catalog(tenant, &config).unwrap() {
                if entry.allowed_kinds.contains(&ServiceKind::Git) {
                    assert_eq!(
                        entry.images.get("git"),
                        Some(&config.image_forgejo),
                        "git is cataloged with the approved Forgejo image"
                    );
                }
            }
        }
    }

    #[test]
    fn service_days_defaults_to_30_and_accepts_only_1_through_366() {
        let original: serde_json::Value =
            serde_json::from_str(include_str!("../policy.example.json")).unwrap();
        for valid in [1, 30, 366] {
            let mut changed = original.clone();
            changed["plans"][0]["service_days"] = serde_json::json!(valid);
            let policy = Policy::parse(&changed.to_string(), 400).unwrap();
            assert_eq!(policy.plans[0].service_days, valid);
        }
        for invalid in [0, 367, -1] {
            let mut changed = original.clone();
            changed["plans"][0]["service_days"] = serde_json::json!(invalid);
            assert!(Policy::parse(&changed.to_string(), 400).is_err());
        }
        let mut tenant_request = request();
        tenant_request.service_days = Some(45);
        assert!(example().resolve(&tenant_request).is_err());
    }

    #[test]
    fn multiple_units_of_the_same_kind_are_admitted_within_the_plan_count() {
        let policy = example();
        let mut web = request();
        web.plan_code = Some("standard".to_string());
        let web = policy.resolve(&web).unwrap();
        let existing = reservation("t01", "t01-old-site", "standard", 75);
        assert!(policy
            .admit(&[existing.clone()], "t01", "t01-new-site", &web)
            .is_ok());

        // The plan's service count still applies: three existing units fill it.
        let mut second = existing.clone();
        second.unit = "t01-second-site".to_string();
        let mut third = existing.clone();
        third.unit = "t01-third-site".to_string();
        let error = policy
            .admit(&[existing, second, third], "t01", "t01-fourth-site", &web)
            .unwrap_err();
        assert!(error.contains("service-count admission refused"));
    }

    #[test]
    fn missing_policy_file_is_not_an_implicit_default_plan() {
        let config = Config {
            policy_file: std::path::PathBuf::from("/honed-policy-test-missing/policy.json"),
            ..Config::from_env()
        };
        assert!(load(&config).is_err());
    }
    #[test]
    fn generic_catalog_registry_optin_and_multiple_instances_preserve_admission() {
        let mut value: serde_json::Value = serde_json::from_str(include_str!("../policy.example.json")).unwrap();
        assert!(example().allowed_registries.is_empty());
        value["allowed_registries"] = serde_json::json!(["codeberg.org", "docker.io"]);
        value["plans"][2]["allowed_kinds"] = serde_json::json!(["web", "database", "storage", "git", "container"]);
        value["plans"][2]["cpu_quota_percent"] = serde_json::json!(25);
        let policy = Policy::parse(&value.to_string(), 400).unwrap();
        let entry = policy.catalog("t04", &Config::from_env()).unwrap().remove(0);
        assert!(entry.allowed_kinds.contains(&ServiceKind::Container));
        assert_eq!(entry.registries, ["codeberg.org", "docker.io"]);
        assert!(!entry.images.contains_key("container"));
        let request = Request { tenant_id: Some("t04".into()), plan_code: Some("pro".into()), kind: Some("container".into()), ..Request::default() };
        let env = policy.resolve(&request).unwrap();
        let mut existing = reservation("t04", "t04-one", "pro", 25);
        existing.kind = Some(ServiceKind::Container);
        policy.admit(&[existing.clone()], "t04", "t04-two", &env).unwrap();
        let reservations: Vec<_> = (0..6).map(|i| { let mut r = existing.clone(); r.unit = format!("t04-unit{i}"); r }).collect();
        assert!(policy.admit(&reservations, "t04", "t04-seven", &env).is_err());
        existing.cpu_quota_percent = Some(276);
        assert!(policy.admit(&[existing], "t04", "t04-two", &env).unwrap_err().contains("host CPU"));
        for invalid in [serde_json::json!(["docker.io", "docker.io"]), serde_json::json!(["https://docker.io"]), serde_json::json!(["docker.io/namespace"]), serde_json::json!(["user:password@docker.io"])] {
            value["allowed_registries"] = invalid;
            assert!(Policy::parse(&value.to_string(), 400).is_err());
        }
    }

}
