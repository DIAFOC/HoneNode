use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceAccess {
    Public,
    Loopback,
}

impl ServiceAccess {
    fn parse(value: &str) -> Self {
        match value.trim() {
            "loopback" => Self::Loopback,
            _ => Self::Public,
        }
    }
}

pub struct Config {
    pub socket: PathBuf,
    pub policy_file: PathBuf,
    pub tenants: Vec<String>,
    pub quadlet_users: PathBuf,
    pub tenant_data_root: PathBuf,
    pub etc_hosting: PathBuf,
    pub caddy_dir: PathBuf,
    pub caddy_admin: String,
    pub tenant_domain: String,
    /// How tenants reach their services: public subdomains or loopback ports.
    /// Fixed templates that embed their own URL (Forgejo) must match it.
    pub service_access: ServiceAccess,
    pub subid_count: u32,
    pub subid_base: u64,
    pub admission_overcommit_percent: u32,
    pub host_memory_total: Option<String>,
    pub host_memory_reserve: Option<String>,
    pub tenant_memory_max: Option<String>,
    pub image_nginx: String,
    pub image_postgres: String,
    pub image_filebrowser: String,
    pub image_forgejo: String,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            socket: PathBuf::from(text("HONE_BROKER_SOCKET", "/run/sharpnode/broker.sock")),
            policy_file: PathBuf::from(text("HONE_POLICY_FILE", "/etc/honenode/policy.json")),
            tenants: list("HONE_TENANTS", "t01,t02,t03,t04"),
            quadlet_users: PathBuf::from(text(
                "HONE_QUADLET_USERS",
                "/etc/containers/systemd/users",
            )),
            tenant_data_root: PathBuf::from(text("HONE_TENANT_DATA", "/srv/hosting")),
            etc_hosting: PathBuf::from(text("HONE_ETC", "/etc/hosting")),
            caddy_dir: PathBuf::from(text("HONE_CADDY_DIR", "/etc/hosting/caddy")),
            caddy_admin: text("HONE_CADDY_ADMIN", "127.0.0.1:2019"),
            tenant_domain: text("HONE_TENANT_DOMAIN", "sharpnode-hosting.lab"),
            service_access: ServiceAccess::parse(&text("HONE_SERVICE_ACCESS", "public")),
            subid_count: number("HONE_SUBID_COUNT", 65536) as u32,
            // Nested environments (distrobox, podman-in-podman) can only map
            // subuids their outer user namespace delegates; the base must fit
            // that window. Bare-metal hosts keep the default.
            subid_base: number("HONE_SUBID_BASE", 100_000) as u64,
            // Soft budgets are the target; this ceiling allows deliberate
            // overcommit before admission refuses (100 = strict, 200 = 2x).
            admission_overcommit_percent: number("HONE_ADMISSION_OVERCOMMIT_PERCENT", 200)
                .clamp(100, 1000) as u32,
            host_memory_total: optional_text("HONE_HOST_MEMORY_TOTAL"),
            host_memory_reserve: optional_text("HONE_HOST_MEMORY_RESERVE"),
            tenant_memory_max: optional_text("HONE_TENANT_MEMORY_MAX"),
            image_nginx: text(
                "HONE_IMAGE_NGINX",
                "docker.io/nginxinc/nginx-unprivileged:1.27-alpine",
            ),
            image_postgres: text(
                "HONE_IMAGE_POSTGRES",
                "docker.io/library/postgres:17-alpine",
            ),
            image_filebrowser: text(
                "HONE_IMAGE_FILEBROWSER",
                "docker.io/filebrowser/filebrowser:v2.63.23",
            ),
            image_forgejo: text(
                "HONE_IMAGE_FORGEJO",
                "codeberg.org/forgejo/forgejo:17-rootless",
            ),
        }
    }
}

impl Config {
    /// The single approved registry image for a kind; tenants cannot import others.
    pub fn image_for(&self, kind: crate::host::ServiceKind) -> &str {
        use crate::host::ServiceKind;
        match kind {
            ServiceKind::Web => &self.image_nginx,
            ServiceKind::Database => &self.image_postgres,
            ServiceKind::Storage => &self.image_filebrowser,
            ServiceKind::Git => &self.image_forgejo,
            ServiceKind::Container => "", // Generic images come only from validated metadata.
        }
    }
}

fn text(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn optional_text(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

fn list(key: &str, default: &str) -> Vec<String> {
    text(key, default)
        .split(',')
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

fn number(key: &str, default: i64) -> i64 {
    std::env::var(key)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(default)
}
