use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Json,
    Plain,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ServiceAccess {
    #[default]
    Public,
    Loopback,
}

impl ServiceAccess {
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim() {
            "public" => Ok(Self::Public),
            "loopback" => Ok(Self::Loopback),
            _ => Err(anyhow!("HONE_SERVICE_ACCESS must be public or loopback")),
        }
    }

    pub fn validate_bind(self, bind: SocketAddr) -> Result<()> {
        if self == Self::Loopback && !bind.ip().is_loopback() {
            return Err(anyhow!(
                "HONE_SERVICE_ACCESS=loopback requires HONE_BIND to use a loopback IP"
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Theme {
    Neutral,
    #[default]
    SharpNode,
}

impl Theme {
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim() {
            "neutral" => Ok(Self::Neutral),
            "sharpnode" => Ok(Self::SharpNode),
            _ => Err(anyhow!("HONE_THEME must be neutral or sharpnode")),
        }
    }

    pub fn css_class(self) -> &'static str {
        match self {
            Self::Neutral => "theme-neutral",
            Self::SharpNode => "theme-sharpnode",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub broker_socket: PathBuf,
    pub registry_path: PathBuf,
    pub tenant_data_root: PathBuf,
    pub tenants: Vec<String>,
    pub panel_domain: String,
    pub tenant_domain: String,
    pub service_access: ServiceAccess,
    pub caddy_admin: String,
    pub static_dir: PathBuf,
    pub log_format: LogFormat,
    pub theme: Theme,
    pub operator_account: Option<String>,
    pub operator_password: Option<String>,
    pub session_ttl: Duration,
    pub secure_cookies: bool,
    pub api_tokens: Vec<String>,
    pub grace_days: i64,
    pub retention_days: i64,
    pub trial_days: i64,
    pub bootstrap_file: Option<PathBuf>,
    pub sweep_interval: Duration,
    pub sample_interval: Duration,
}

impl Config {
    /// Where first-run operator credentials are written when the app generates
    /// them itself: next to the registry unless HONE_BOOTSTRAP_FILE says otherwise.
    pub fn bootstrap_credentials_path(&self) -> PathBuf {
        if let Some(path) = &self.bootstrap_file {
            return path.clone();
        }
        let registry = self.registry_path.clone();
        match (registry.parent(), registry.file_name().and_then(|n| n.to_str())) {
            (Some(parent), Some(_)) => parent.join("bootstrap-credentials.txt"),
            _ => PathBuf::from("bootstrap-credentials.txt"),
        }
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let bind = text("HONE_BIND", "0.0.0.0:8080")
            .parse()
            .context("HONE_BIND is not a socket address")?;

        let service_access = ServiceAccess::parse(&text("HONE_SERVICE_ACCESS", "public"))?;
        service_access.validate_bind(bind)?;

        let log_format = match text("HONE_LOG_FORMAT", "json").as_str() {
            "plain" => LogFormat::Plain,
            _ => LogFormat::Json,
        };

        let operator_account = optional("HONE_OPERATOR_ACCOUNT");
        if let Some(number) = operator_account.as_deref() {
            if !crate::auth::account_number_is_valid(number) {
                return Err(anyhow!(
                    "HONE_OPERATOR_ACCOUNT is not a valid account number"
                ));
            }
        }

        Ok(Self {
            bind,
            broker_socket: PathBuf::from(text("HONE_BROKER_SOCKET", "/run/sharpnode/broker.sock")),
            registry_path: PathBuf::from(text(
                "HONE_REGISTRY",
                "/var/lib/sharpnode/registry/registry.db",
            )),
            tenant_data_root: PathBuf::from(text("HONE_TENANT_DATA", "/srv/hosting")),
            tenants: list("HONE_TENANTS", "t01,t02,t03,t04"),
            panel_domain: text("HONE_PANEL_DOMAIN", "panel.sharpnode.lab"),
            tenant_domain: text("HONE_TENANT_DOMAIN", "sharpnode-hosting.lab"),
            service_access,
            caddy_admin: text("HONE_CADDY_ADMIN", "127.0.0.1:2019"),
            static_dir: PathBuf::from(text("HONE_STATIC_DIR", "static")),
            log_format,
            theme: Theme::parse(&text("HONE_THEME", "sharpnode"))?,
            operator_account,
            operator_password: optional("HONE_OPERATOR_PASSWORD"),
            session_ttl: Duration::from_secs(bounded_number(
                "HONE_SESSION_TTL_SECS",
                8 * 3600,
                60,
                30 * 86400,
            )? as u64),
            secure_cookies: flag("HONE_SECURE_COOKIES", true)?,
            api_tokens: list("HONE_API_TOKENS", ""),
            grace_days: bounded_number("HONE_GRACE_DAYS", 7, 0, 3650)?,
            retention_days: bounded_number("HONE_RETENTION_DAYS", 30, 0, 3650)?,
            trial_days: bounded_number("HONE_TRIAL_DAYS", 7, 1, 3650)?,
            bootstrap_file: optional("HONE_BOOTSTRAP_FILE").map(PathBuf::from),
            sweep_interval: Duration::from_secs(
                bounded_number("HONE_SWEEP_SECS", 300, 1, 86400)? as u64
            ),
            sample_interval: Duration::from_secs(
                bounded_number("HONE_SAMPLE_SECS", 60, 1, 86400)? as u64
            ),
        })
    }
}

fn text(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn optional(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn list(key: &str, default: &str) -> Vec<String> {
    text(key, default)
        .split(',')
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

fn bounded_number(key: &str, default: i64, min: i64, max: i64) -> Result<i64> {
    let value = match std::env::var(key) {
        Ok(value) => value
            .trim()
            .parse::<i64>()
            .with_context(|| format!("invalid {key}"))?,
        Err(_) => default,
    };
    if !(min..=max).contains(&value) {
        return Err(anyhow!("{key} must be between {min} and {max}"));
    }
    Ok(value)
}

fn flag(key: &str, default: bool) -> Result<bool> {
    match std::env::var(key) {
        Ok(value) => match value.trim() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            _ => Err(anyhow!("invalid boolean {key}")),
        },
        Err(_) => Ok(default),
    }
}
