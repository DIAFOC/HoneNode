use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::error::{AppError, AppResult};
use crate::service_options::{ServiceConfiguration, ServiceOptions};

const CALL_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RESPONSE: u64 = 1024 * 1024;

// Not `Clone`: requests can carry a one-time file-manager password.
#[derive(Debug, Serialize)]
#[serde(tag = "op")]
pub enum Request {
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "status")]
    Status,
    #[serde(rename = "catalog.list")]
    CatalogList { tenant_id: String },
    #[serde(rename = "container.create")]
    ContainerCreate { tenant_id: String, service: String, plan_code: String, container: crate::containers::ContainerSpec },
    #[serde(rename = "container.inspect")]
    ContainerInspect { tenant_id: String, service: String },
    #[serde(rename = "tenant.create")]
    TenantCreate { tenant_id: String },
    #[serde(rename = "tenant.delete")]
    TenantDelete { tenant_id: String },
    #[serde(rename = "service.status")]
    ServiceStatus { tenant_id: String, service: String },
    #[serde(rename = "service.start")]
    ServiceStart { tenant_id: String, service: String },
    #[serde(rename = "service.stop")]
    ServiceStop { tenant_id: String, service: String },
    #[serde(rename = "service.restart")]
    ServiceRestart { tenant_id: String, service: String },
    #[serde(rename = "service.suspend")]
    ServiceSuspend { tenant_id: String, service: String },
    #[serde(rename = "service.resume")]
    ServiceResume { tenant_id: String, service: String },
    #[serde(rename = "service.delete")]
    ServiceDelete { tenant_id: String, service: String },
    #[serde(rename = "limits.set")]
    LimitsSet {
        tenant_id: String,
        service: String,
        kind: String,
        plan_code: String,
        /// Approved image and initial file-manager settings; `None` keeps the
        /// legacy web/database wire format unchanged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        options: Option<ServiceOptions>,
        /// App-allocated loopback host port. `None` keeps the legacy fixed
        /// allocator for the first instance of a kind.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
    },
    #[serde(rename = "service.inspect")]
    ServiceInspect { tenant_id: String, service: String },
    #[serde(rename = "service.configure")]
    ServiceConfigure {
        tenant_id: String,
        service: String,
        /// Kind and plan are derived by the broker from the committed unit.
        options: ServiceOptions,
    },
    #[serde(rename = "edge.reload")]
    EdgeReload {
        tenant_id: String,
        service: String,
        hostname: String,
        upstream: String,
    },
    #[serde(rename = "logs.fetch")]
    LogsFetch {
        tenant_id: String,
        service: String,
        lines: i64,
    },
    #[serde(rename = "site.read")]
    SiteRead {
        tenant_id: String,
        service: String,
        site_path: String,
    },
    #[serde(rename = "site.write")]
    SiteWrite {
        tenant_id: String,
        service: String,
        site_path: String,
        site_content: String,
    },
    #[serde(rename = "db.console")]
    DbConsole { tenant_id: String, service: String },
}

impl Request {
    pub fn op(&self) -> &'static str {
        match self {
            Request::ContainerCreate { .. } => "container.create",
            Request::ContainerInspect { .. } => "container.inspect",
            Request::Ping => "ping",
            Request::Status => "status",
            Request::CatalogList { .. } => "catalog.list",
            Request::TenantCreate { .. } => "tenant.create",
            Request::TenantDelete { .. } => "tenant.delete",
            Request::ServiceStatus { .. } => "service.status",
            Request::ServiceStart { .. } => "service.start",
            Request::ServiceStop { .. } => "service.stop",
            Request::ServiceRestart { .. } => "service.restart",
            Request::ServiceSuspend { .. } => "service.suspend",
            Request::ServiceResume { .. } => "service.resume",
            Request::ServiceDelete { .. } => "service.delete",
            Request::LimitsSet { .. } => "limits.set",
            Request::ServiceInspect { .. } => "service.inspect",
            Request::ServiceConfigure { .. } => "service.configure",
            Request::EdgeReload { .. } => "edge.reload",
            Request::LogsFetch { .. } => "logs.fetch",
            Request::SiteRead { .. } => "site.read",
            Request::SiteWrite { .. } => "site.write",
            Request::DbConsole { .. } => "db.console",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ActorKind {
    Operator,
    Tenant,
    System,
}

#[derive(Debug, Clone, Serialize)]
struct AssertedActor {
    kind: ActorKind,
    id: String,
}

// App-asserted audit metadata only: not broker authentication or an idempotency key.
#[derive(Debug, Clone, Serialize)]
pub struct CallContext {
    request_id: String,
    actor: AssertedActor,
}

impl CallContext {
    pub fn new(request_id: &str, kind: ActorKind, actor_id: &str) -> AppResult<Self> {
        for (value, maximum) in [(request_id, 64), (actor_id, 128)] {
            if value.is_empty()
                || value.len() > maximum
                || !value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.:@".contains(&b))
            {
                return Err(AppError::BadRequest(
                    "invalid broker correlation metadata".to_string(),
                ));
            }
        }
        Ok(Self {
            request_id: request_id.to_string(),
            actor: AssertedActor {
                kind,
                id: actor_id.to_string(),
            },
        })
    }

    pub fn system(actor_id: &str) -> AppResult<Self> {
        Self::new(&crate::auth::generate_token()?, ActorKind::System, actor_id)
    }

    pub fn for_principal(
        request_id: &str,
        principal: &crate::models::Principal,
    ) -> AppResult<Self> {
        let kind = match principal.role() {
            crate::models::Role::Operator => ActorKind::Operator,
            crate::models::Role::Tenant => ActorKind::Tenant,
            crate::models::Role::Support => return Err(AppError::Forbidden),
        };
        Self::new(request_id, kind, &principal.account.number)
    }
}

#[derive(Serialize)]
struct CorrelatedRequest<'a> {
    #[serde(flatten)]
    request: &'a Request,
    #[serde(flatten)]
    context: &'a CallContext,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default)]
    pub op: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub uid: Option<u32>,
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default)]
    pub active_state: Option<ActiveState>,
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default)]
    pub catalog: Option<serde_json::Value>,
    #[serde(default)]
    pub configuration: Option<ServiceConfiguration>,
    #[serde(default)]
    pub container: Option<crate::containers::ContainerInfo>,
    #[serde(default)]
    pub site: Option<SiteContent>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SiteContent {
    pub path: String,
    pub content: String,
}

impl Response {
    fn require_ok(self) -> AppResult<Self> {
        if self.ok && self.error.is_none() {
            Ok(self)
        } else {
            Err(AppError::Broker(
                self.error
                    .unwrap_or_else(|| "refused without a reason".to_string()),
            ))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ActiveState {
    Active,
    Reloading,
    Inactive,
    Failed,
    Activating,
    Deactivating,
    Maintenance,
    Refreshing,
}

impl ActiveState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Reloading => "reloading",
            Self::Inactive => "inactive",
            Self::Failed => "failed",
            Self::Activating => "activating",
            Self::Deactivating => "deactivating",
            Self::Maintenance => "maintenance",
            Self::Refreshing => "refreshing",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Observation {
    uid: u32,
    unit: String,
    active_state: ActiveState,
}

impl Observation {
    pub fn from_response(response: Response, tenant: &str, service: &str) -> AppResult<Self> {
        validate_service_identity(tenant, service)?;
        if !response.ok || response.op != "service.status" || response.error.is_some() {
            return Err(AppError::Broker(
                "service.status refused or contradictory response".to_string(),
            ));
        }
        let uid = response
            .uid
            .filter(|uid| *uid != 0 && *uid != u32::MAX)
            .ok_or_else(|| AppError::Broker("service.status missing or invalid uid".to_string()))?;
        let unit = response
            .unit
            .filter(|unit| unit.as_str() == service)
            .ok_or_else(|| {
                AppError::Broker("service.status unit mismatch or missing unit".to_string())
            })?;
        let active_state = response
            .active_state
            .ok_or_else(|| AppError::Broker("service.status missing active_state".to_string()))?;
        Ok(Self {
            uid,
            unit,
            active_state,
        })
    }

    pub fn uid(&self) -> u32 {
        self.uid
    }
    pub fn unit(&self) -> &str {
        &self.unit
    }
    pub fn active_state(&self) -> ActiveState {
        self.active_state
    }
}

fn validate_service_identity(tenant: &str, service: &str) -> AppResult<()> {
    if tenant.len() != 3
        || !tenant.starts_with('t')
        || !tenant.as_bytes()[1..].iter().all(u8::is_ascii_digit)
        || service.len() > 64
        || service.len() <= tenant.len() + 1
        || !service.starts_with(&format!("{tenant}-"))
        || !service
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(AppError::Broker(
            "invalid tenant-bound service identity".to_string(),
        ));
    }
    Ok(())
}

#[derive(Clone)]
pub struct Client {
    socket: Arc<PathBuf>,
}

impl Client {
    pub fn new(socket: PathBuf) -> Self {
        Self {
            socket: Arc::new(socket),
        }
    }

    pub fn socket(&self) -> &Path {
        self.socket.as_path()
    }

    pub async fn call(&self, request: &Request) -> AppResult<Response> {
        let context = CallContext::system("honenode")?;
        self.call_with_context(request, &context).await
    }

    pub async fn call_with_context(
        &self,
        request: &Request,
        context: &CallContext,
    ) -> AppResult<Response> {
        tracing::debug!(request_id = %context.request_id, action = request.op(),
            asserted_actor_kind = ?context.actor.kind, asserted_actor_id = %context.actor.id, "calling broker");
        let exchange = async {
            let stream = UnixStream::connect(self.socket.as_path())
                .await
                .map_err(|error| {
                    AppError::Broker(format!("{} unreachable: {error}", self.socket.display()))
                })?;

            let (read_half, mut write_half) = stream.into_split();
            let mut payload = serde_json::to_vec(&CorrelatedRequest { request, context })
                .map_err(|error| AppError::Internal(anyhow::Error::new(error)))?;
            payload.push(b'\n');

            write_half
                .write_all(&payload)
                .await
                .map_err(|error| AppError::Broker(format!("write failed: {error}")))?;
            write_half
                .flush()
                .await
                .map_err(|error| AppError::Broker(format!("flush failed: {error}")))?;

            let mut line = String::new();
            BufReader::new(read_half.take(MAX_RESPONSE + 1))
                .read_line(&mut line)
                .await
                .map_err(|error| AppError::Broker(format!("read failed: {error}")))?;

            if line.len() as u64 > MAX_RESPONSE || !line.ends_with('\n') {
                return Err(AppError::Broker(
                    "oversized or unterminated broker answer".to_string(),
                ));
            }
            let response = serde_json::from_str::<Response>(line.trim())
                .map_err(|error| AppError::Broker(format!("malformed answer: {error}")))?;
            if response.op != request.op() {
                return Err(AppError::Broker("broker operation mismatch".to_string()));
            }
            if response.request_id.as_deref() != Some(context.request_id.as_str()) {
                return Err(AppError::Broker(
                    "broker correlation ID missing or mismatched".to_string(),
                ));
            }
            Ok(response)
        };

        match tokio::time::timeout(CALL_TIMEOUT, exchange).await {
            Ok(result) => result,
            Err(_) => Err(AppError::Broker(format!(
                "{} did not answer within {}s",
                request.op(),
                CALL_TIMEOUT.as_secs()
            ))),
        }
    }

    pub async fn call_ok(&self, request: &Request) -> AppResult<Response> {
        self.call(request).await?.require_ok()
    }

    pub async fn call_ok_with_context(
        &self,
        request: &Request,
        context: &CallContext,
    ) -> AppResult<Response> {
        self.call_with_context(request, context).await?.require_ok()
    }

    pub async fn service_status(
        &self,
        tenant: &str,
        service: &str,
        context: Option<&CallContext>,
    ) -> AppResult<Observation> {
        validate_service_identity(tenant, service)?;
        let request = Request::ServiceStatus {
            tenant_id: tenant.to_string(),
            service: service.to_string(),
        };
        // Read-only observation must not hold the mutation lock for a host-action timeout.
        let response = tokio::time::timeout(Duration::from_secs(5), async {
            match context {
                Some(context) => self.call_ok_with_context(&request, context).await,
                None => self.call_ok(&request).await,
            }
        })
        .await
        .map_err(|_| AppError::Broker("service.status timed out".to_string()))??;
        Observation::from_response(response, tenant, service)
    }

    /// Sanitized committed configuration; never secrets, hashes, or host paths.
    pub async fn inspect(
        &self,
        tenant: &str,
        service: &str,
        context: &CallContext,
    ) -> AppResult<ServiceConfiguration> {
        validate_service_identity(tenant, service)?;
        let response = self
            .call_ok_with_context(
                &Request::ServiceInspect {
                    tenant_id: tenant.to_string(),
                    service: service.to_string(),
                },
                context,
            )
            .await?;
        response
            .configuration
            .ok_or_else(|| AppError::Broker("service.inspect missing configuration".to_string()))
    }

    pub async fn inspect_container(&self, tenant: &str, service: &str, context: &CallContext) -> AppResult<crate::containers::ContainerInfo> {
        validate_service_identity(tenant, service)?;
        let response = self.call_ok_with_context(&Request::ContainerInspect {tenant_id:tenant.into(),service:service.into()}, context).await
            .map_err(|_| AppError::Broker("container inspection unavailable (details redacted)".into()))?;
        let info = response.container.ok_or_else(|| AppError::Broker("missing container info".into()))?;
        info.validate(service)?;
        Ok(info)
    }

    pub async fn reachable(&self, context: &CallContext) -> bool {
        self.call_ok_with_context(&Request::Ping, context)
            .await
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    async fn exchange(answer: Vec<u8>) -> AppResult<Response> {
        let path = std::env::temp_dir().join(format!(
            "hone-{}.sock",
            crate::auth::generate_token().unwrap()
        ));
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut request = String::new();
            BufReader::new(reader)
                .read_line(&mut request)
                .await
                .unwrap();
            let _ = writer.write_all(&answer).await;
        });
        let context = CallContext::new("test-framing", ActorKind::System, "test").unwrap();
        let result = Client::new(path.clone())
            .call_with_context(&Request::Ping, &context)
            .await;
        server.await.unwrap();
        std::fs::remove_file(path).unwrap();
        result
    }

    #[test]
    fn correlation_is_bounded_flat_metadata_and_edge_reload_is_service_scoped() {
        let context =
            CallContext::new("req-_.:@123", ActorKind::Operator, "account-_.:@123").unwrap();
        let request = Request::EdgeReload {
            tenant_id: "t01".to_string(),
            service: "t01-site".to_string(),
            hostname: "t01.example.org".to_string(),
            upstream: "127.0.0.1:18001".to_string(),
        };
        let wire = serde_json::to_value(CorrelatedRequest {
            request: &request,
            context: &context,
        })
        .unwrap();
        assert_eq!(
            wire,
            serde_json::json!({
                "op":"edge.reload", "tenant_id":"t01", "service":"t01-site",
                "hostname":"t01.example.org", "upstream":"127.0.0.1:18001",
                "request_id":"req-_.:@123", "actor":{"kind":"operator","id":"account-_.:@123"}
            })
        );
        for invalid in ["", "has space", "line\nbreak", "quoted\"", "é"] {
            assert!(CallContext::new(invalid, ActorKind::System, "valid").is_err());
            assert!(CallContext::new("valid", ActorKind::System, invalid).is_err());
        }
        assert!(CallContext::new(&"r".repeat(64), ActorKind::System, &"a".repeat(128)).is_ok());
        assert!(CallContext::new(&"r".repeat(65), ActorKind::System, "a").is_err());
        assert!(CallContext::new("r", ActorKind::System, &"a".repeat(129)).is_err());
        let system = CallContext::system("honenode").unwrap();
        assert_eq!(system.actor.kind, ActorKind::System);
        assert_eq!(system.request_id.len(), 64);
        assert!(system
            .request_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn requires_matching_correlation_on_success_and_refusal() {
        assert!(exchange(b"{\"ok\":true,\"op\":\"ping\"}\n".to_vec())
            .await
            .is_err());
        assert!(
            exchange(b"{\"ok\":true,\"op\":\"ping\",\"request_id\":\"wrong\"}\n".to_vec())
                .await
                .is_err()
        );
        assert!(exchange(
            b"{\"ok\":false,\"op\":\"ping\",\"error\":\"denied\",\"request_id\":\"wrong\"}\n"
                .to_vec()
        )
        .await
        .is_err());
        let refusal = exchange(b"{\"ok\":false,\"op\":\"ping\",\"error\":\"denied\",\"request_id\":\"test-framing\"}\n".to_vec()).await.unwrap();
        assert!(!refusal.ok);
        assert!(refusal.require_ok().is_err());
    }

    #[test]
    fn options_and_configuration_follow_the_broker_wire_contract() {
        let legacy = Request::LimitsSet {
            tenant_id: "t01".to_string(),
            service: "t01-site".to_string(),
            kind: "web".to_string(),
            plan_code: "starter".to_string(),
            options: None,
            port: None,
        };
        let wire = serde_json::to_value(&legacy).unwrap();
        assert_eq!(wire["op"], "limits.set");
        assert!(wire.get("options").is_none());

        let options = || ServiceOptions {
            image: Some("docker.io/filebrowser/filebrowser:v2.63.23".to_string()),
            file_manager: Some(crate::service_options::FileManagerOptions {
                username: "alice".to_string(),
                password: crate::service_options::Secret::new("correct horse battery"),
                read_only: true,
            }),
            database: None,
        };
        let provision = Request::LimitsSet {
            tenant_id: "t01".to_string(),
            service: "t01-vault".to_string(),
            kind: "storage".to_string(),
            plan_code: "vault".to_string(),
            options: Some(options()),
            port: None,
        };
        let wire = serde_json::to_value(&provision).unwrap();
        assert_eq!(wire["options"]["image"], "docker.io/filebrowser/filebrowser:v2.63.23");
        assert_eq!(wire["options"]["file_manager"]["username"], "alice");
        assert_eq!(wire["options"]["file_manager"]["read_only"], true);
        let debug = format!("{provision:?}");
        assert!(debug.contains("REDACTED") && !debug.contains("correct horse"));

        let database = Request::LimitsSet {
            tenant_id: "t01".to_string(),
            service: "t01-db".to_string(),
            kind: "database".to_string(),
            plan_code: "pro".to_string(),
            options: Some(ServiceOptions {
                image: Some("docker.io/library/postgres:17-alpine".to_string()),
                file_manager: None,
                database: Some(crate::service_options::DatabaseOptions {
                    password: crate::service_options::Secret::new("tenant database password"),
                }),
            }),
            port: Some(15431),
        };
        let wire = serde_json::to_value(&database).unwrap();
        assert_eq!(
            wire["options"]["database"]["password"],
            "tenant database password"
        );
        let debug = format!("{database:?}");
        assert!(debug.contains("REDACTED") && !debug.contains("tenant database password"));

        let inspect = Request::ServiceInspect {
            tenant_id: "t01".to_string(),
            service: "t01-vault".to_string(),
        };
        assert_eq!(inspect.op(), "service.inspect");
        assert_eq!(
            serde_json::to_value(&inspect).unwrap(),
            serde_json::json!({"op":"service.inspect","tenant_id":"t01","service":"t01-vault"})
        );
        let configure = Request::ServiceConfigure {
            tenant_id: "t01".to_string(),
            service: "t01-vault".to_string(),
            options: options(),
        };
        assert_eq!(configure.op(), "service.configure");
        let wire = serde_json::to_value(&configure).unwrap();
        assert!(wire.get("kind").is_none() && wire.get("plan_code").is_none());
        assert_eq!(wire["options"]["file_manager"]["username"], "alice");

        let response: Response = serde_json::from_value(serde_json::json!({
            "ok": true, "op": "service.inspect", "request_id": "req-1",
            "configuration": {
                "kind": "storage", "image": "docker.io/filebrowser/filebrowser:v2.63.23",
                "username": "alice", "read_only": false
            }
        }))
        .unwrap();
        let configuration = response.configuration.unwrap();
        assert_eq!(configuration.username.as_deref(), Some("alice"));
        assert_eq!(configuration.read_only, Some(false));
        let text = serde_json::to_string(&configuration).unwrap();
        for forbidden in ["password", "hash", "bcrypt", "/etc/hosting", "/srv/hosting"] {
            assert!(!text.contains(forbidden), "configuration leaked {forbidden}");
        }
        let old: Response =
            serde_json::from_str(r#"{"ok":true,"op":"ping","detail":"pong"}"#).unwrap();
        assert!(old.configuration.is_none());
    }

    #[tokio::test]
    async fn validates_broker_framing_and_operation() {
        assert!(
            exchange(b"{\"ok\":true,\"op\":\"ping\",\"request_id\":\"test-framing\"}\n".to_vec())
                .await
                .unwrap()
                .ok
        );
        assert!(exchange(
            b"{\"ok\":true,\"op\":\"status\",\"request_id\":\"test-framing\"}\n".to_vec()
        )
        .await
        .is_err());
        assert!(exchange(
            b"{\"ok\":true,\"op\":\"ping\",\"request_id\":\"test-framing\"}".to_vec()
        )
        .await
        .is_err());
        assert!(exchange(vec![b'x'; MAX_RESPONSE as usize + 2])
            .await
            .is_err());
    }
}
