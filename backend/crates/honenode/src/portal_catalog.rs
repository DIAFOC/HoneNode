use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::broker;
use crate::error::{AppError, AppResult};
use crate::models::ServiceKind;

/// Root-owned policy, not the local marketing/price catalog. No disk enforcement is implied.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CatalogEntry {
    pub plan_code: String,
    #[serde(default, deserialize_with = "label")]
    pub label: String,
    pub memory_high: String,
    pub memory_max: String,
    pub cpu_quota_percent: u32,
    pub tasks_max: i64,
    pub max_services: i64,
    pub service_days: i64,
    pub allowed_kinds: Vec<String>,
    #[serde(default)]
    pub images: BTreeMap<String, Option<String>>,
    #[serde(default)]
    pub registries: Vec<String>,
    #[serde(skip)]
    pub reserved_kinds: Vec<String>,
    #[serde(skip)]
    pub can_provision: bool,
}

fn label<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

pub fn parse(value: serde_json::Value) -> AppResult<Vec<CatalogEntry>> {
    let mut entries: Vec<CatalogEntry> = serde_json::from_value(value)
        .map_err(|error| AppError::Broker(format!("invalid catalog: {error}")))?;
    let mut codes = BTreeSet::new();
    for entry in &mut entries {
        if entry.plan_code.is_empty()
            || entry.plan_code.len() > 64
            || !entry
                .plan_code
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
            || !codes.insert(entry.plan_code.clone())
            || !(1..=3650).contains(&entry.service_days)
            || entry.max_services < 1
            || entry.tasks_max < 1
            || entry.cpu_quota_percent == 0
            || entry.memory_high.is_empty()
            || entry.memory_max.is_empty()
            || entry
                .allowed_kinds
                .iter()
                .any(|kind| ServiceKind::parse(kind).is_none())
        {
            return Err(AppError::Broker("invalid catalog policy".to_string()));
        }
        if entry.label.is_empty() {
            entry.label = entry.plan_code.clone();
        }
    }
    Ok(entries)
}

pub async fn load(
    client: &broker::Client,
    slug: &str,
    context: &broker::CallContext,
) -> AppResult<Vec<CatalogEntry>> {
    let response = client
        .call_ok_with_context(
            &broker::Request::CatalogList {
                tenant_id: slug.to_string(),
            },
            context,
        )
        .await?;
    parse(
        response
            .catalog
            .ok_or_else(|| AppError::Broker("catalog missing".to_string()))?,
    )
}

pub fn select<'a>(
    entries: &'a [CatalogEntry],
    code: &str,
    kind: ServiceKind,
) -> AppResult<&'a CatalogEntry> {
    entries
        .iter()
        .find(|entry| {
            entry.plan_code == code
                && entry
                    .allowed_kinds
                    .iter()
                    .any(|value| value == kind.as_str())
        })
        .ok_or_else(|| AppError::BadRequest("plan o tipo no permitido".to_string()))
}
