mod capacity;
mod config;
mod containers;
mod host;
mod policy;
mod provisioning;
mod service_config;
mod site;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex, RwLock};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::config::Config;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_REQUEST_BYTES: u64 = 128 * 1024;
const MAX_CONNECTIONS: usize = 32;
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

const OPS_WITH_TENANT: &[&str] = &[
    "tenant.create",
    "tenant.delete",
    "catalog.list",
    "container.create",
    "container.inspect",
    "service.start",
    "service.stop",
    "service.restart",
    "service.suspend",
    "service.resume",
    "service.status",
    "service.inspect",
    "service.configure",
    "edge.reload",
    "service.delete",
    "limits.set",
    "logs.fetch",
    "site.read",
    "site.write",
    "db.console",
];

const OPS_PLATFORM: &[&str] = &["ping", "status"];

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
struct RequestId(String);

impl TryFrom<String> for RequestId {
    type Error = String;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        validate_correlation_token(&value, 64)?;
        Ok(Self(value))
    }
}

impl From<RequestId> for String {
    fn from(value: RequestId) -> Self {
        value.0
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
struct ActorId(String);

impl TryFrom<String> for ActorId {
    type Error = String;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        validate_correlation_token(&value, 128)?;
        Ok(Self(value))
    }
}

impl From<ActorId> for String {
    fn from(value: ActorId) -> Self {
        value.0
    }
}

fn validate_correlation_token(value: &str, maximum: usize) -> Result<(), String> {
    if value.is_empty()
        || value.len() > maximum
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:@".contains(&byte))
    {
        return Err(
            "invalid audit correlation token: expected bounded ASCII letters, digits, -_.:@"
                .to_string(),
        );
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ActorKind {
    Operator,
    Tenant,
    System,
}

impl ActorKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::Tenant => "tenant",
            Self::System => "system",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AssertedActor {
    kind: ActorKind,
    id: ActorId,
}

#[derive(Debug, Default, Deserialize)]
struct Request {
    op: String,
    #[serde(default)]
    request_id: Option<RequestId>,
    #[serde(default)]
    actor: Option<AssertedActor>,
    #[serde(default)]
    plan_code: Option<String>,
    #[serde(default)]
    service_days: Option<u32>,
    #[serde(default)]
    cpu_quota_percent: Option<u32>,
    #[serde(default)]
    tenant_id: Option<String>,
    #[serde(default)]
    service: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    cpu_weight: Option<i64>,
    #[serde(default)]
    memory_high: Option<String>,
    #[serde(default)]
    memory_max: Option<String>,
    #[serde(default)]
    tasks_max: Option<i64>,
    #[serde(default)]
    hostname: Option<String>,
    #[serde(default)]
    upstream: Option<String>,
    #[serde(default)]
    lines: Option<i64>,
    /// Only for `limits.set` and `service.configure`; Debug redacts secrets.
    #[serde(default)]
    options: Option<service_config::ServiceOptions>,
    #[serde(default)]
    container: Option<containers::ContainerSpec>,
    /// Only for `site.read` and `site.write`.
    #[serde(default)]
    site_path: Option<String>,
    #[serde(default)]
    site_content: Option<String>,
    /// App-allocated loopback host port for `limits.set` on a fixed template.
    #[serde(default)]
    port: Option<u16>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Response {
    ok: bool,
    op: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    uid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active_state: Option<host::ActiveState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    catalog: Option<Vec<policy::CatalogEntry>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    configuration: Option<service_config::ServiceConfiguration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    container: Option<containers::ContainerInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    site: Option<site::SiteContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request_id: Option<RequestId>,
}

impl Response {
    fn ok(op: &str, detail: impl Into<String>) -> Self {
        Self {
            ok: true,
            op: op.to_string(),
            error: None,
            detail: Some(detail.into()),
            version: Some(VERSION.to_string()),
            uid: None,
            unit: None,
            active_state: None,
            catalog: None,
            configuration: None,
            container: None,
            site: None,
            request_id: None,
        }
    }

    fn configuration(
        op: &str,
        detail: impl Into<String>,
        configuration: service_config::ServiceConfiguration,
    ) -> Self {
        let mut response = Self::ok(op, detail);
        response.configuration = Some(configuration);
        response
    }

    fn catalog(op: &str, catalog: Vec<policy::CatalogEntry>) -> Self {
        let mut response = Self::ok(op, format!("{} entitled plans", catalog.len()));
        response.catalog = Some(catalog);
        response
    }

    fn service_status(op: &str, status: host::ServiceStatus) -> Self {
        let mut response = Self::ok(op, format!("{} observed from systemd", status.unit));
        response.uid = Some(status.uid);
        response.unit = Some(status.unit);
        response.active_state = Some(status.active_state);
        response
    }

    fn refused(op: &str, error: impl Into<String>) -> Self {
        Self {
            ok: false,
            op: op.to_string(),
            error: Some(error.into()),
            detail: None,
            version: Some(VERSION.to_string()),
            uid: None,
            unit: None,
            active_state: None,
            catalog: None,
            configuration: None,
            container: None,
            site: None,
            request_id: None,
        }
    }
}

struct Broker {
    config: Config,
    allowed: Vec<String>,
    tenants: RwLock<Vec<String>>,
    operations: Mutex<()>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();

    let config = Config::from_env();
    let socket = config.socket.clone();
    let allowed = config.tenants.clone();
    let tenants: Vec<String> = allowed
        .iter()
        .filter(|tenant| host::user_exists(tenant.as_str()))
        .cloned()
        .collect();

    if socket.exists() {
        fs::remove_file(&socket)?;
    }
    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent)?;
    }

    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o660))?;

    tracing::info!(
        socket = %socket.display(),
        tenants = %tenants.join(","),
        version = VERSION,
        "honed listening"
    );

    let broker = Arc::new(Broker {
        config,
        allowed,
        tenants: RwLock::new(tenants),
        operations: Mutex::new(()),
    });
    let connections = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));

    loop {
        let permit = Arc::clone(&connections).acquire_owned().await?;
        let (stream, _) = listener.accept().await?;
        let broker = Arc::clone(&broker);
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(error) = handle(stream, broker).await {
                tracing::warn!(error = %error, "connection ended with an error");
            }
        });
    }
}

async fn handle(stream: UnixStream, broker: Arc<Broker>) -> std::io::Result<()> {
    let peer_uid = stream
        .peer_cred()
        .map(|cred| cred.uid())
        .unwrap_or(u32::MAX);
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half).take(MAX_REQUEST_BYTES);

    let mut line = String::new();
    let response = match tokio::time::timeout(REQUEST_TIMEOUT, reader.read_line(&mut line)).await {
        Err(_) => {
            broker.audit(peer_uid, "?", None, "refused", "request timed out");
            Response::refused("?", "request timed out")
        }
        Ok(Err(error)) => return Err(error),
        Ok(Ok(0)) => return Ok(()),
        Ok(Ok(_)) if line.len() as u64 == MAX_REQUEST_BYTES && !line.ends_with('\n') => {
            broker.audit(peer_uid, "?", None, "refused", "request too large");
            Response::refused("?", "request too large")
        }
        Ok(Ok(_)) => match parse_request(&mut line) {
            Ok(request) => {
                let op = request.op.clone();
                let dispatch_broker = Arc::clone(&broker);
                match tokio::task::spawn_blocking(move || {
                    dispatch_broker.dispatch(request, peer_uid)
                })
                .await
                {
                    Ok(response) => response,
                    Err(error) => {
                        let reason = format!("dispatch task failed: {error}");
                        broker.audit(peer_uid, &op, None, "failed", &reason);
                        Response::refused(&op, "broker dispatch failed")
                    }
                }
            }
            Err(reason) => {
                broker.audit(peer_uid, "?", None, "refused", &reason);
                Response::refused("?", reason)
            }
        },
    };

    let mut payload = match serde_json::to_vec(&response) {
        Ok(payload) => payload,
        Err(error) => {
            tracing::error!(error = %error, "could not serialise the response");
            return Ok(());
        }
    };
    payload.push(b'\n');
    tokio::time::timeout(REQUEST_TIMEOUT, async {
        write_half.write_all(&payload).await?;
        write_half.flush().await
    })
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "response write timed out"))??;
    Ok(())
}

impl Broker {
    fn dispatch(&self, request: Request, peer_uid: u32) -> Response {
        let request_id = request.request_id.clone();
        let span = tracing::info_span!(
            "broker_correlation",
            request_id = request_id.as_ref().map(|id| id.0.as_str()).unwrap_or("-"),
            asserted_actor_kind = request
                .actor
                .as_ref()
                .map(|actor| actor.kind.as_str())
                .unwrap_or("-"),
            asserted_actor_id = request
                .actor
                .as_ref()
                .map(|actor| actor.id.0.as_str())
                .unwrap_or("-"),
            actor_is_app_asserted = request.actor.is_some(),
        );
        let _context = span.enter();
        let mut response = self.dispatch_inner(request, peer_uid);
        response.request_id = request_id;
        response
    }

    fn dispatch_inner(&self, request: Request, peer_uid: u32) -> Response {
        let op = request.op.as_str();

        if !OPS_WITH_TENANT.contains(&op) && !OPS_PLATFORM.contains(&op) {
            self.audit(peer_uid, op, None, "refused", "unknown operation");
            return Response::refused(op, "unknown operation");
        }

        if OPS_WITH_TENANT.contains(&op) {
            let Some(tenant_id) = request.tenant_id.as_deref() else {
                self.audit(peer_uid, op, None, "refused", "missing tenant_id");
                return Response::refused(op, "missing tenant_id");
            };
            if !shape_is_valid(tenant_id) {
                self.audit(
                    peer_uid,
                    op,
                    Some(tenant_id),
                    "refused",
                    "malformed tenant_id",
                );
                return Response::refused(op, "invalid tenant_id");
            }
            if !self
                .allowed
                .iter()
                .any(|allowed| allowed.as_str() == tenant_id)
            {
                self.audit(
                    peer_uid,
                    op,
                    Some(tenant_id),
                    "refused",
                    "tenant outside the node allowlist",
                );
                return Response::refused(op, "tenant outside the node allowlist");
            }
            if !matches!(op, "tenant.create" | "catalog.list") && !self.knows(tenant_id) {
                self.audit(peer_uid, op, Some(tenant_id), "refused", "unknown tenant");
                return Response::refused(op, "unknown tenant");
            }
        }

        let _operation = match self.operations.lock() {
            Ok(guard) => guard,
            Err(_) => {
                self.audit(
                    peer_uid,
                    op,
                    request.tenant_id.as_deref(),
                    "failed",
                    "operation lock poisoned",
                );
                return Response::refused(op, "broker operation lock is unavailable");
            }
        };
        let outcome = self.execute(&request);
        match outcome {
            Ok(response) => {
                self.audit(
                    peer_uid,
                    op,
                    request.tenant_id.as_deref(),
                    "ok",
                    if op == "logs.fetch" { "journal fetched (content omitted)" } else { response.detail.as_deref().unwrap_or_default() },
                );
                response
            }
            Err(error) => {
                self.audit(peer_uid, op, request.tenant_id.as_deref(), "failed", &error);
                Response::refused(op, error)
            }
        }
    }

    fn execute(&self, request: &Request) -> Result<Response, String> {
        if request.container.is_some() && request.op != "container.create" {
            return Err("container is accepted only by container.create".to_string());
        }
        if request.service_days.is_some() {
            return Err("service_days is fixed by operator policy, not tenant requests".to_string());
        }
        if request.options.is_some()
            && !matches!(request.op.as_str(), "limits.set" | "service.configure")
        {
            return Err("options are accepted only by limits.set and service.configure".to_string());
        }
        if (request.site_path.is_some() || request.site_content.is_some())
            && !matches!(request.op.as_str(), "site.read" | "site.write")
        {
            return Err("site fields are accepted only by site.read and site.write".to_string());
        }
        if request.port.is_some() && request.op != "limits.set" {
            return Err("port is accepted only by limits.set".to_string());
        }
        if request.op == "catalog.list" {
            let tenant = request.tenant_id.as_deref().unwrap_or_default();
            let policy = policy::load(&self.config)?;
            let catalog = policy.catalog(tenant, &self.config)?;
            return Ok(Response::catalog(&request.op, catalog));
        }
        let config = &self.config;
        let tenant = request.tenant_id.as_deref().unwrap_or_default();
        if request.op == "container.create" {
            let unit = self.unit(request)?;
            containers::validate_name(tenant, &unit)?;
            let spec = request.container.as_ref().ok_or("missing container specification")?;
            if request.kind.as_deref().is_some_and(|k| k != "container") {
                return Err("container.create requires container kind".to_string());
            }
            let policy = policy::load(config)?;
            spec.validate(&policy.allowed_registries)?;
            let derived = Request {
                tenant_id: request.tenant_id.clone(), plan_code: request.plan_code.clone(),
                kind: Some("container".to_string()), cpu_weight: request.cpu_weight,
                cpu_quota_percent: request.cpu_quota_percent, memory_high: request.memory_high.clone(),
                memory_max: request.memory_max.clone(), tasks_max: request.tasks_max,
                ..Request::default()
            };
            let envelope = policy.resolve(&derived)?;
            let reservations = host::committed_reservations(config)?;
            policy.admit_with_overcommit(
                &reservations,
                tenant,
                &unit,
                &envelope,
                config.admission_overcommit_percent,
            )?;
            capacity::admit_with_overcommit(
                capacity::policy(config)?,
                &reservations,
                tenant,
                &unit,
                envelope.memory_max,
                config.admission_overcommit_percent,
            )?;
            let info = containers::create(config, tenant, &unit, spec, &envelope)?;
            let mut response = Response::ok(&request.op, format!("{unit} start queued; query service.status for observed state"));
            response.container = Some(info);
            return Ok(response);
        }
        if request.op == "container.inspect" {
            let unit = self.unit(request)?;
            let mut response = Response::ok(&request.op, format!("{unit} configuration"));
            response.container = Some(containers::inspect(config, tenant, &unit)?);
            return Ok(response);
        }
        if matches!(request.op.as_str(), "service.inspect" | "service.status" | "logs.fetch") {
            let unit = self.unit(request)?;
            if containers::exists(config, &unit)? {
                let info = containers::inspect(config, tenant, &unit)?;
                let mut response = match request.op.as_str() {
                    "service.status" => Response::service_status(&request.op, host::service_status(config, tenant, &unit)?),
                    "logs.fetch" => Response::ok(&request.op, containers::redact_logs(config, tenant, &info,
                        host::logs(config, tenant, &unit, request.lines.unwrap_or(200))?)?),
                    _ => Response::ok(&request.op, format!("{unit} configuration")),
                };
                response.container = Some(info);
                return Ok(response);
            }
        }
        match request.op.as_str() {
            "service.status" => {
                let unit = self.unit(request)?;
                let status = host::service_status(config, tenant, &unit)?;
                return Ok(Response::service_status(&request.op, status));
            }
            "service.inspect" => {
                let unit = self.unit(request)?;
                let configuration = host::inspect_unit(config, tenant, &unit)?;
                return Ok(Response::configuration(
                    &request.op,
                    format!("{unit} configuration"),
                    configuration,
                ));
            }
            "limits.set" => {
                let unit = self.unit(request)?;
                return self.provision(request, request, &unit, service_config::WriteMode::Provision);
            }
            "site.read" => {
                let unit = self.unit(request)?;
                let path = request
                    .site_path
                    .as_deref()
                    .ok_or_else(|| "missing site_path".to_string())?;
                if request.site_content.is_some() {
                    return Err("site.read does not accept site_content".to_string());
                }
                let content = site::read(config, tenant, &unit, path)?;
                let mut response = Response::ok(&request.op, format!("{unit} site file"));
                response.site = Some(site::SiteContent {
                    path: path.to_string(),
                    content,
                });
                return Ok(response);
            }
            "site.write" => {
                let unit = self.unit(request)?;
                let path = request
                    .site_path
                    .as_deref()
                    .ok_or_else(|| "missing site_path".to_string())?;
                let content = request
                    .site_content
                    .as_deref()
                    .ok_or_else(|| "missing site_content".to_string())?;
                site::write(config, tenant, &unit, path, content)?;
                return Ok(Response::ok(
                    &request.op,
                    format!("{unit} site file written"),
                ));
            }
            "service.configure" => {
                let unit = self.unit(request)?;
                if request.plan_code.is_some()
                    || request.kind.is_some()
                    || request.cpu_weight.is_some()
                    || request.cpu_quota_percent.is_some()
                    || request.memory_high.is_some()
                    || request.memory_max.is_some()
                    || request.tasks_max.is_some()
                {
                    return Err(
                        "service.configure derives kind and plan from the committed unit; omit them"
                            .to_string(),
                    );
                }
                if request.options.is_none() {
                    return Err("service.configure requires options".to_string());
                }
                let (_, _, committed) = host::committed_unit(config, tenant, &unit)?;
                let kind = committed
                    .kind
                    .ok_or_else(|| "unit has no committed kind".to_string())?;
                if kind != host::ServiceKind::Storage {
                    return Err(
                        "service.configure supports only storage file-manager settings".to_string(),
                    );
                }
                let derived = Request {
                    op: request.op.clone(),
                    tenant_id: request.tenant_id.clone(),
                    service: request.service.clone(),
                    plan_code: committed.plan_code.clone(),
                    kind: Some(kind.as_str().to_string()),
                    ..Request::default()
                };
                return self.provision(&derived, request, &unit, service_config::WriteMode::Configure);
            }
            _ => {}
        }
        self.execute_detail(request)
            .map(|detail| Response::ok(&request.op, detail))
    }

    fn execute_detail(&self, request: &Request) -> Result<String, String> {
        let config = &self.config;
        let tenant = request.tenant_id.as_deref().unwrap_or_default();

        match request.op.as_str() {
            "ping" => Ok("pong".to_string()),
            "status" => Ok(format!(
                "socket={} tenants={}",
                config.socket.display(),
                self.tenants
                    .read()
                    .map(|list| list.join(","))
                    .unwrap_or_default()
            )),

            "tenant.create" => {
                policy::load(config)?.require_tenant(tenant)?;
                let detail = host::tenant_create(config, tenant)?;
                host::write_network(config, tenant)?;
                host::daemon_reload(tenant)?;
                if let Ok(mut list) = self.tenants.write() {
                    if !list.iter().any(|known| known == tenant) {
                        list.push(tenant.to_string());
                    }
                }
                Ok(detail)
            }
            "tenant.delete" => host::tenant_delete(config, tenant),


            "service.start" => {
                let unit = self.unit(request)?;
                host::activate_unit(config, tenant, &unit, "start")?;
                Ok(format!("{unit} start queued; query service.status for observed state"))
            }
            "service.stop" => {
                let unit = self.unit(request)?;
                host::stop_owned(config, tenant, &unit)?;
                Ok(format!("{unit} stopped"))
            }
            "service.restart" => {
                let unit = self.unit(request)?;
                host::activate_unit(config, tenant, &unit, "restart")?;
                Ok(format!("{unit} restart queued; query service.status for observed state"))
            }
            "service.suspend" => {
                let unit = self.unit(request)?;
                host::stop_owned(config, tenant, &unit)?;
                Ok(format!("{unit} suspended"))
            }
            "service.resume" => {
                let unit = self.unit(request)?;
                host::activate_unit(config, tenant, &unit, "start")?;
                Ok(format!("{unit} resume queued; query service.status for observed state"))
            }
            "service.delete" => {
                let unit = self.unit(request)?;
                let uid = host::user_uid(tenant)?;
                let path = config
                    .quadlet_users
                    .join(uid.to_string())
                    .join(format!("{unit}.container"));
                if path.exists() {
                    host::stop_owned(config, tenant, &unit)?;
                    fs::remove_file(&path)
                        .map_err(|error| format!("{}: {error}", path.display()))?;
                } else {
                    // Idempotent delete: a unit that was never written (failed
                    // provisioning) or already removed is stopped if loaded and
                    // reported as removed, so the tenant can clean up.
                    host::stop_if_loaded(tenant, &unit)?;
                }
                host::daemon_reload(tenant)?;
                Ok(format!("{unit} removed"))
            }

            "logs.fetch" => {
                let unit = self.unit(request)?;
                host::logs(config, tenant, &unit, request.lines.unwrap_or(200))
            }

            "edge.reload" => {
                let unit = self.unit(request)?;
                let hostname = request
                    .hostname
                    .as_deref()
                    .ok_or_else(|| "missing hostname".to_string())?;
                if containers::exists(config, &unit)? {
                    let info = containers::inspect(config, tenant, &unit)?;
                    let upstream = format!("127.0.0.1:{}", info.host_port);
                    if request.upstream.as_deref().is_some_and(|u| u != upstream) {
                        return Err("upstream does not match committed container".to_string());
                    }
                    return host::edge_reload(config, tenant, &unit, hostname, &upstream);
                }
                let upstream = request
                    .upstream
                    .as_deref()
                    .ok_or_else(|| "missing upstream".to_string())?;
                host::edge_reload(config, tenant, &unit, hostname, upstream)
            }

            "db.console" => Err(
                "not_implemented: the mediated console needs its own design before it ships"
                    .to_string(),
            ),

            other => Err(format!("unknown operation: {other}")),
        }
    }

    /// Shared `limits.set` / `service.configure` path. `limits` supplies the
    /// plan/kind (client request for limits.set, committed unit for configure);
    /// policy and capacity admission always run before options touch the host.
    fn provision(
        &self,
        limits: &Request,
        request: &Request,
        unit: &str,
        mode: service_config::WriteMode,
    ) -> Result<Response, String> {
        let config = &self.config;
        let tenant = request.tenant_id.as_deref().unwrap_or_default();
        let policy = policy::load(config)?;
        let envelope = policy.resolve(limits)?;
        let reservations = host::committed_reservations(config)?;
        policy.admit_with_overcommit(
            &reservations,
            tenant,
            unit,
            &envelope,
            config.admission_overcommit_percent,
        )?;
        let admission = capacity::admit_with_overcommit(
            capacity::policy(config)?,
            &reservations,
            tenant,
            unit,
            envelope.memory_max,
            config.admission_overcommit_percent,
        )?;
        // `limits.set` carries the app-allocated port; `service.configure`
        // reuses the port already committed in the existing unit.
        let port = request
            .port
            .or_else(|| host::published_port_for(config, tenant, unit).ok().flatten());
        let (path, configuration) = host::write_unit_with_options(
            config,
            tenant,
            unit,
            &envelope,
            request.options.as_ref(),
            mode,
            port,
        )?;
        host::daemon_reload(tenant)?;
        Ok(Response::configuration(
            &request.op,
            format!(
                "{} rendered; memory tenant={}/{} host={}/{}",
                path.display(),
                admission.tenant_committed,
                admission.tenant_max,
                admission.host_committed,
                admission.host_budget
            ),
            configuration,
        ))
    }

    fn unit(&self, request: &Request) -> Result<String, String> {
        let tenant = request.tenant_id.as_deref().unwrap_or_default();
        let service = request
            .service
            .as_deref()
            .ok_or_else(|| "missing service".to_string())?;

        validate_unit_name(tenant, service)?;
        Ok(service.to_string())
    }

    fn knows(&self, tenant: &str) -> bool {
        self.tenants
            .read()
            .map(|list| list.iter().any(|known| known.as_str() == tenant))
            .unwrap_or(false)
    }

    fn audit(&self, peer_uid: u32, op: &str, tenant: Option<&str>, outcome: &str, reason: &str) {
        tracing::info!(
            actor_type = "unix_peer",
            actor_id = peer_uid,
            action = op,
            tenant_id = tenant.unwrap_or("-"),
            outcome,
            reason,
            "broker request"
        );
    }
}

/// Parses one request line and zeroes it afterwards (it may hold a password).
/// Parse errors for requests carrying `options` are redacted because serde can
/// quote offending values.
fn parse_request(line: &mut String) -> Result<Request, String> {
    let parsed = serde_json::from_str::<Request>(line.trim());
    // Never expose serde diagnostics: even malformed/escaped field names can
    // otherwise cause serde to quote tenant-supplied secret values.
    let result = parsed.map_err(|_| "malformed request (details redacted)".to_string());
    service_config::zero_string(line);
    result
}

fn validate_unit_name(tenant: &str, service: &str) -> Result<(), String> {
    if service.is_empty()
        || service.len() > 64
        || !service
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err("invalid service name".to_string());
    }
    if !service.starts_with(&format!("{tenant}-")) {
        return Err("service does not belong to the tenant".to_string());
    }
    Ok(())
}

fn shape_is_valid(tenant: &str) -> bool {
    tenant.len() == 3
        && tenant.starts_with('t')
        && tenant[1..].bytes().all(|byte| byte.is_ascii_digit())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().json())
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_and_unit_names_reject_injection_and_cross_tenant_access() {
        assert!(shape_is_valid("t01"));
        assert!(shape_is_valid("t99"));
        assert!(!shape_is_valid("t1"));
        assert!(!shape_is_valid("t001"));
        assert!(!shape_is_valid("t100"));
        assert!(!shape_is_valid("../t01"));
        assert!(validate_unit_name("t01", "t01-site").is_ok());
        assert!(validate_unit_name("t01", "t02-site").is_err());
        assert!(validate_unit_name("t01", "t01-site\nExecStart=/bin/sh").is_err());
        assert!(validate_unit_name("t01", "t01-../site").is_err());
    }

    #[test]
    fn edge_reload_requires_explicit_tenant_and_its_service_before_host_access() {
        let broker = Broker {
            config: Config::from_env(),
            allowed: vec!["t01".to_string(), "t02".to_string()],
            tenants: RwLock::new(vec!["t01".to_string(), "t02".to_string()]),
            operations: Mutex::new(()),
        };
        for (value, expected) in [
            (
                serde_json::json!({"op":"edge.reload","hostname":"t02.tenants.example","upstream":"127.0.0.1:18002"}),
                "missing tenant_id",
            ),
            (
                serde_json::json!({"op":"edge.reload","tenant_id":"t01","service":"t02-site","hostname":"t02.tenants.example","upstream":"127.0.0.1:18002"}),
                "service does not belong to the tenant",
            ),
            (
                serde_json::json!({"op":"edge.reload","tenant_id":"t01","hostname":"t01.tenants.example","upstream":"127.0.0.1:18001"}),
                "missing service",
            ),
        ] {
            let response = broker.dispatch(serde_json::from_value(value).unwrap(), 1000);
            assert!(!response.ok);
            assert_eq!(response.error.as_deref(), Some(expected));
        }
    }

    #[test]
    fn correlation_metadata_is_typed_bounded_and_cannot_inject_logs() {
        let valid: Request = serde_json::from_str(
            r#"{"op":"ping","request_id":"req-123:abc","actor":{"kind":"tenant","id":"acct_123"}}"#,
        )
        .unwrap();
        assert_eq!(valid.request_id.unwrap().0, "req-123:abc");
        assert_eq!(valid.actor.unwrap().kind.as_str(), "tenant");
        for value in [
            "",
            "bad\nline",
            "has space",
            "quoted\"value",
            "é",
            &"x".repeat(65),
        ] {
            let request = serde_json::json!({"op":"ping", "request_id":value});
            assert!(serde_json::from_value::<Request>(request).is_err());
        }
        for actor in [
            serde_json::json!({"kind":"root", "id":"a"}),
            serde_json::json!({"kind":"operator", "id":"bad\rline"}),
            serde_json::json!({"kind":"system", "id":"x".repeat(129)}),
            serde_json::json!({"kind":"tenant", "id":"a", "trusted":true}),
        ] {
            assert!(serde_json::from_value::<Request>(
                serde_json::json!({"op":"ping","actor":actor})
            )
            .is_err());
        }
    }

    #[test]
    fn request_id_is_echoed_on_success_and_refusal_without_granting_actor_authority() {
        let broker = Broker {
            config: Config::from_env(),
            allowed: vec!["t01".to_string()],
            tenants: RwLock::new(vec!["t01".to_string()]),
            operations: Mutex::new(()),
        };
        for op in ["ping", "not.allowed"] {
            let request: Request = serde_json::from_value(serde_json::json!({
                "op":op, "request_id":"req-1", "actor":{"kind":"operator","id":"admin"}
            }))
            .unwrap();
            let value = serde_json::to_value(broker.dispatch(request, 1000)).unwrap();
            assert_eq!(value["request_id"], "req-1");
            assert_eq!(value["ok"], op == "ping");
        }
    }

    #[test]
    fn provisioning_fails_closed_without_policy_and_tenant_delete_is_disabled() {
        let mut config = Config::from_env();
        config.policy_file = std::path::PathBuf::from("/honed-policy-test-missing/policy.json");
        let broker = Broker {
            config,
            allowed: vec!["t01".to_string()],
            tenants: RwLock::new(vec!["t01".to_string()]),
            operations: Mutex::new(()),
        };
        let request: Request = serde_json::from_str(r#"{"op":"limits.set","tenant_id":"t01","service":"t01-site","kind":"web","plan_code":"starter","cpu_weight":100,"memory_high":"256M","memory_max":"384M","tasks_max":128}"#).unwrap();
        let refused = broker.dispatch(request, 1000);
        assert!(!refused.ok);
        assert!(refused.error.unwrap().contains("broker policy"));
        let request: Request = serde_json::from_value(
            serde_json::json!({"op":"tenant.create","tenant_id":"t01"}),
        )
        .unwrap();
        let refused = broker.dispatch(request, 1000);
        assert!(!refused.ok);
        assert!(refused.error.unwrap().contains("broker policy"));
        let request: Request = serde_json::from_str(
            r#"{"op":"tenant.delete","tenant_id":"t01","actor":{"kind":"operator","id":"root"}}"#,
        )
        .unwrap();
        let refused = broker.dispatch(request, 1000);
        assert!(!refused.ok);
        assert!(refused.error.unwrap().contains("disabled"));
    }

    #[test]
    fn service_status_wire_response_has_typed_optional_fields() {
        let request: Request = serde_json::from_str(
            r#"{"op":"service.status","tenant_id":"t01","service":"t01-site"}"#,
        )
        .unwrap();
        assert!(OPS_WITH_TENANT.contains(&request.op.as_str()));
        let response = Response::service_status(
            &request.op,
            host::ServiceStatus {
                uid: 1001,
                unit: "t01-site".to_string(),
                active_state: host::ActiveState::Failed,
            },
        );
        let value = serde_json::to_value(response).unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(value["op"], "service.status");
        assert_eq!(value["uid"], 1001);
        assert_eq!(value["unit"], "t01-site");
        assert_eq!(value["active_state"], "failed");
        assert!(value.get("error").is_none());
        let round_trip: Response = serde_json::from_value(value).unwrap();
        assert_eq!(round_trip.active_state, Some(host::ActiveState::Failed));
    }

    #[test]
    fn ordinary_and_failed_responses_omit_status_fields_and_accept_old_wire_format() {
        for response in [
            Response::ok("ping", "pong"),
            Response::refused("service.status", "not owned"),
        ] {
            let value = serde_json::to_value(response).unwrap();
            for field in ["uid", "unit", "active_state", "catalog"] {
                assert!(value.get(field).is_none());
            }
        }
        let old: Response =
            serde_json::from_str(r#"{"ok":true,"op":"ping","detail":"pong"}"#).unwrap();
        assert_eq!(old.uid, None);
        assert_eq!(old.unit, None);
        assert_eq!(old.active_state, None);
    }

    #[test]
    fn service_status_dispatch_rejects_cross_tenant_names_before_host_access() {
        let request: Request = serde_json::from_str(
            r#"{"op":"service.status","tenant_id":"t01","service":"t02-site"}"#,
        )
        .unwrap();
        let broker = Broker {
            config: Config::from_env(),
            allowed: vec!["t01".to_string()],
            tenants: RwLock::new(vec!["t01".to_string()]),
            operations: Mutex::new(()),
        };
        let response = broker.dispatch(request, 1000);
        assert!(!response.ok);
        assert_eq!(
            response.error.as_deref(),
            Some("service does not belong to the tenant")
        );
        assert!(response.uid.is_none());
    }

    #[test]
    fn catalog_response_has_the_typed_portal_contract() {
        let entry = policy::CatalogEntry {
            plan_code: "starter".to_string(),
            memory_high: "256M".to_string(),
            memory_max: "384M".to_string(),
            cpu_quota_percent: 25,
            tasks_max: 128,
            max_services: 1,
            service_days: 30,
            allowed_kinds: vec![host::ServiceKind::Web],
            registries: vec![],
            images: std::collections::BTreeMap::from([(
                "web".to_string(),
                "example/nginx:1".to_string(),
            )]),
        };
        let value = serde_json::to_value(Response::catalog("catalog.list", vec![entry])).unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(value["op"], "catalog.list");
        assert_eq!(value["catalog"][0]["plan_code"], "starter");
        assert_eq!(value["catalog"][0]["service_days"], 30);
        assert_eq!(value["catalog"][0]["allowed_kinds"][0], "web");
        assert_eq!(value["catalog"][0]["images"]["web"], "example/nginx:1");
        for field in [
            "plan_code",
            "memory_high",
            "memory_max",
            "cpu_quota_percent",
            "tasks_max",
            "max_services",
            "service_days",
            "allowed_kinds",
            "images",
        ] {
            assert!(value["catalog"][0].get(field).is_some(), "missing {field}");
        }
    }

    #[test]
    fn catalog_checks_policy_before_requiring_a_host_account() {
        let mut config = Config::from_env();
        config.policy_file = std::path::PathBuf::from("/honed-policy-test-missing/policy.json");
        let broker = Broker {
            config,
            allowed: vec!["t01".to_string()],
            tenants: RwLock::new(Vec::new()),
            operations: Mutex::new(()),
        };
        let request: Request = serde_json::from_value(
            serde_json::json!({"op":"catalog.list","tenant_id":"t01"}),
        )
        .unwrap();
        let response = broker.dispatch(request, 1000);
        assert!(!response.ok);
        assert!(response.error.unwrap().contains("broker policy"));
    }

    fn test_broker() -> Broker {
        Broker {
            config: Config::from_env(),
            allowed: vec!["t01".to_string()],
            tenants: RwLock::new(vec!["t01".to_string()]),
            operations: Mutex::new(()),
        }
    }

    #[test]
    fn options_wire_contract_is_typed_and_never_debug_prints_secrets() {
        let request: Request = serde_json::from_value(serde_json::json!({
            "op": "limits.set",
            "tenant_id": "t01",
            "service": "t01-files",
            "kind": "storage",
            "plan_code": "vault",
            "options": {
                "image": "docker.io/filebrowser/filebrowser:v2.63.23",
                "file_manager": {"username": "alice", "password": "correct horse battery", "read_only": true}
            }
        }))
        .unwrap();
        let options = request.options.as_ref().unwrap();
        assert_eq!(options.image.as_deref(), Some("docker.io/filebrowser/filebrowser:v2.63.23"));
        let file_manager = options.file_manager.as_ref().unwrap();
        assert_eq!(file_manager.username, "alice");
        assert!(file_manager.read_only);
        let debug = format!("{request:?}");
        assert!(debug.contains("REDACTED"));
        assert!(!debug.contains("correct horse"));
        for op in ["service.inspect", "service.configure"] {
            assert!(OPS_WITH_TENANT.contains(&op));
        }
        // Requests without options remain valid for every existing operation.
        let legacy: Request = serde_json::from_str(
            r#"{"op":"limits.set","tenant_id":"t01","service":"t01-site","kind":"web","plan_code":"starter"}"#,
        )
        .unwrap();
        assert!(legacy.options.is_none());
    }

    #[test]
    fn malformed_options_are_redacted_and_the_request_line_is_zeroed() {
        let mut line = r#"{"op":"limits.set","options":{"file_manager":{"username":5,"password":"supersecretvalue"}}}"#.to_string();
        let error = parse_request(&mut line).unwrap_err();
        assert!(error.contains("redacted"));
        assert!(!error.contains("supersecretvalue"));
        assert!(line.bytes().all(|byte| byte == 0));
        let mut line = r#"{"op":"ping","request_id":5}"#.to_string();
        assert!(parse_request(&mut line).unwrap_err().contains("malformed request"));
        let mut line = r#"{"op":"limits.set","options":{"image":"ok"}}"#.to_string();
        assert!(parse_request(&mut line).is_ok());
    }

    #[test]
    fn options_are_rejected_for_other_operations_before_host_access() {
        let broker = test_broker();
        for op in ["service.start", "service.status", "service.inspect", "catalog.list"] {
            let request: Request = serde_json::from_value(serde_json::json!({
                "op": op,
                "tenant_id": "t01",
                "service": "t01-files",
                "options": {"file_manager": {"username": "alice", "password": ""}}
            }))
            .unwrap();
            let response = broker.dispatch(request, 1000);
            assert!(!response.ok);
            assert_eq!(
                response.error.as_deref(),
                Some("options are accepted only by limits.set and service.configure")
            );
        }
    }

    #[test]
    fn configure_derives_plan_and_kind_and_requires_options_before_host_access() {
        let broker = test_broker();
        let file_manager = serde_json::json!({"file_manager":{"username":"alice","password":"","read_only":true}});
        let request: Request = serde_json::from_value(serde_json::json!({
            "op": "service.configure", "tenant_id": "t01", "service": "t01-files",
            "kind": "web", "plan_code": "starter", "options": file_manager.clone()
        }))
        .unwrap();
        let response = broker.dispatch(request, 1000);
        assert!(response.error.unwrap().contains("derives kind and plan"));
        let request: Request = serde_json::from_value(serde_json::json!({
            "op": "service.configure", "tenant_id": "t01", "service": "t01-files"
        }))
        .unwrap();
        let response = broker.dispatch(request, 1000);
        assert_eq!(response.error.as_deref(), Some("service.configure requires options"));
        for op in ["service.inspect", "service.configure"] {
            let mut value = serde_json::json!({
                "op": op, "tenant_id": "t01", "service": "t02-files"
            });
            if op == "service.configure" {
                value["options"] = file_manager.clone();
            }
            let request: Request = serde_json::from_value(value).unwrap();
            let response = broker.dispatch(request, 1000);
            assert_eq!(
                response.error.as_deref(),
                Some("service does not belong to the tenant")
            );
        }
    }

    #[test]
    fn configuration_response_is_sanitized_and_omitted_elsewhere() {
        let response = Response::configuration(
            "service.inspect",
            "t01-files configuration",
            service_config::ServiceConfiguration {
                kind: host::ServiceKind::Storage,
                image: "example/filebrowser:1".to_string(),
                username: Some("alice".to_string()),
                read_only: Some(true),
            },
        );
        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["configuration"]["kind"], "storage");
        assert_eq!(value["configuration"]["image"], "example/filebrowser:1");
        assert_eq!(value["configuration"]["username"], "alice");
        assert_eq!(value["configuration"]["read_only"], true);
        let text = value.to_string();
        for forbidden in ["password", "hash", "bcrypt", "$2a$", "/etc/hosting", "/srv/hosting"] {
            assert!(!text.contains(forbidden), "response leaked {forbidden}");
        }
        let web = service_config::ServiceConfiguration {
            kind: host::ServiceKind::Web,
            image: "example/nginx:1".to_string(),
            username: None,
            read_only: None,
        };
        let value = serde_json::to_value(web).unwrap();
        assert!(value.get("username").is_none());
        assert!(value.get("read_only").is_none());
        for response in [Response::ok("ping", "pong"), Response::refused("service.inspect", "no")] {
            assert!(serde_json::to_value(response).unwrap().get("configuration").is_none());
        }
        let old: Response = serde_json::from_str(r#"{"ok":true,"op":"ping","detail":"pong"}"#).unwrap();
        assert!(old.configuration.is_none());
    }

    #[test]
    fn current_limits_set_request_remains_wire_compatible() {
        let request = serde_json::from_str::<Request>(
            r#"{"op":"limits.set","tenant_id":"t01","service":"t01-site","kind":"web","cpu_weight":100,"memory_high":"256M","memory_max":"384M","tasks_max":128}"#,
        )
        .unwrap();
        assert_eq!(request.op, "limits.set");
        assert_eq!(request.tenant_id.as_deref(), Some("t01"));
        assert_eq!(request.memory_max.as_deref(), Some("384M"));
    }
    #[test]
    fn generic_wire_contract_redacts_even_malformed_escaped_fields() {
        let mut line = r#"{"op":"container.create","tenant_id":"t01","service":"t01-forge-one","plan_code":"pro","container":{"image":"codeberg.org/forgejo/forgejo:17-rootless","container_port":3000,"environment":{"PASSWORD":"secret-wire-value"}}}"#.to_string();
        let request = parse_request(&mut line).unwrap();
        assert!(request.container.is_some());
        assert!(!format!("{request:?}").contains("secret-wire-value"));
        assert!(line.is_empty());
        for mut line in [
            r#"{"op":"container.create","container":{"image":"codeberg.org/a:1","container_port":"secret-wire-value"}}"#.to_string(),
            r#"{"op":"container.create","cont\u0061iner":{"environment":{"K":{"secret-wire-value":1}}}}"#.to_string(),
            r#"{"op":"container.create","container":{"secret-wire-value":1}}"#.to_string(),
            r#"{"op":"container.create","container":{"environment":{"K":"secret-wire-value"},"container_port":70000}}"#.to_string(),
        ] {
            let error = parse_request(&mut line).unwrap_err();
            assert!(error.contains("redacted"));
            assert!(!error.contains("secret-wire-value"));
            assert!(line.is_empty());
        }
    }

    #[test]
    fn generic_dispatch_rejects_cross_tenant_names_and_wrong_operations_before_host_access() {
        for op in ["container.create", "container.inspect", "service.start", "service.restart", "service.resume", "service.status", "service.stop", "service.suspend", "service.delete", "service.inspect", "logs.fetch"] {
            let request: Request = serde_json::from_value(serde_json::json!({"op":op,"tenant_id":"t01","service":"t02-custom"})).unwrap();
            let response = test_broker().dispatch(request, 1000);
            assert_eq!(response.error.as_deref(), Some("service does not belong to the tenant"), "{op}");
        }
        let request: Request = serde_json::from_value(serde_json::json!({"op":"service.start","tenant_id":"t01","service":"t01-custom", "container":{"image":"codeberg.org/a:1","container_port":80}})).unwrap();
        assert_eq!(test_broker().dispatch(request, 1000).error.as_deref(), Some("container is accepted only by container.create"));
    }

    #[test]
    fn generic_response_is_safe_and_status_retains_observed_fields() {
        let info = containers::ContainerInfo { name:"t01-custom".into(), image:"codeberg.org/a:1".into(), host_port:30000, container_port:8080, user:1000, read_only:false, volumes:vec![], environment_keys:vec!["PASSWORD".into()] };
        for op in ["container.create", "container.inspect", "service.inspect", "service.status"] {
            let mut response = if op == "service.status" { Response::service_status(op, host::ServiceStatus {uid:1001,unit:info.name.clone(),active_state:host::ActiveState::Activating}) } else { Response::ok(op, "queued or inspected") };
            response.container = Some(info.clone());
            let value = serde_json::to_value(response).unwrap();
            assert_eq!(value["container"], serde_json::to_value(&info).unwrap());
            assert!(value.get("configuration").is_none());
            if op == "service.status" { assert_eq!(value["active_state"], "activating"); assert_eq!(value["uid"],1001); }
            let _: Response = serde_json::from_value(value).unwrap();
        }
        assert!(serde_json::to_value(Response::ok("ping", "pong")).unwrap().get("container").is_none());
    }

}
