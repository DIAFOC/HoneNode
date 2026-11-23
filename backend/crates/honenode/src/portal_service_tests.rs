//! Image catalogue, storage provisioning secrets, and service configuration/logs.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{header, Request, StatusCode};
use serde_json::{json, Value};
use time::OffsetDateTime;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tower::ServiceExt;

use crate::models::{Principal, Role, Service, ServiceKind, ServiceState, Tenant};
use crate::tests::{state, tenant};
use crate::{auth, broker, registry, web};

const PASSWORD: &str = "correct horse battery";
const IMAGE: &str = "docker.io/filebrowser/filebrowser:v2.63.23";

fn catalog() -> Value {
    json!([{
        "plan_code": "root-plan", "label": "Root plan", "memory_high": "256M",
        "memory_max": "512M", "cpu_quota_percent": 75, "tasks_max": 128,
        "max_services": 4, "service_days": 17,
        "allowed_kinds": ["web", "database", "storage", "container"],
        "images": {
            "web": "approved.example/nginx:1",
            "database": "approved.example/postgres:1",
            "storage": IMAGE,
        }
    }])
}

fn storage_configuration() -> Value {
    json!({"kind": "storage", "image": IMAGE, "username": "alice", "read_only": false})
}

struct FakeBroker {
    path: PathBuf,
    requests: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeBroker {
    async fn new(catalog: Value, configuration: Value, logs: &str, refuse: Option<&'static str>) -> Self {
        let path = std::env::temp_dir().join(format!(
            "hone-portal-{}.sock",
            auth::generate_token().unwrap()
        ));
        let listener = UnixListener::bind(&path).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let logs = logs.to_string();
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                captured.lock().unwrap().push(request.clone());
                let op = request["op"].as_str().unwrap();
                let mut response =
                    json!({"op": op, "ok": true, "request_id": request["request_id"]});
                if refuse == Some(op) {
                    response["ok"] = json!(false);
                    response["error"] = json!("root policy refused");
                } else {
                    match op {
                        "catalog.list" => response["catalog"] = catalog.clone(),
                        "container.create" => response["container"] = json!({"name":request["service"],"image":request["container"]["image"],"host_port":30001,"container_port":8080,"user":1000,"read_only":false,"volumes":[],"environment_keys":["APP_SECRET"]}),
                        "container.inspect" => response["container"] = configuration.clone(),
                        "service.inspect" => response["configuration"] = configuration.clone(),
                        "service.configure" => {
                            let mut updated = configuration.clone();
                            updated["read_only"] =
                                request["options"]["file_manager"]["read_only"].clone();
                            response["configuration"] = updated;
                        }
                        "logs.fetch" => response["detail"] = json!(logs),
                        "site.read" => response["site"] = json!({"path": request["site_path"], "content": "<h1>anterior</h1>"}),
                        "site.write" => {}
                        "service.status" => {
                            response["unit"] = request["service"].clone();
                            response["uid"] = json!(1001);
                            response["active_state"] = json!("active");
                        }
                        _ => {}
                    }
                }
                let mut bytes = serde_json::to_vec(&response).unwrap();
                bytes.push(b'\n');
                stream.get_mut().write_all(&bytes).await.unwrap();
            }
        });
        Self {
            path,
            requests,
            task,
        }
    }

    fn ops(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| request["op"].as_str().unwrap().to_string())
            .collect()
    }
}

impl Drop for FakeBroker {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn principal(state: &crate::state::AppState) -> Principal {
    let (account, tenant) = tenant(state).await;
    let session = auth::create_session(&state.db, account.id, Duration::from_secs(3600), None, None)
        .await
        .unwrap();
    Principal {
        account,
        tenant: Some(tenant),
        session,
    }
}

async fn service(
    state: &crate::state::AppState,
    tenant: &Tenant,
    kind: ServiceKind,
    target: ServiceState,
) -> Service {
    let plan = registry::plan_by_code(&state.db, "pro")
        .await
        .unwrap()
        .unwrap();
    let suffix = match kind {
        ServiceKind::Storage => "vault",
        ServiceKind::Database => "db",
        ServiceKind::Git => "git",
        _ => "site",
    };
    let service = registry::create_service(
        &state.db,
        tenant.id,
        kind,
        plan.id,
        &format!("{}-{suffix}", tenant.slug),
        None,
        "{}",
        "{}",
        OffsetDateTime::now_utc() + time::Duration::days(30),
    )
    .await
    .unwrap();
    registry::set_service_state(&state.db, service.id, target, None)
        .await
        .unwrap();
    service
}

async fn request(
    state: &crate::state::AppState,
    principal: Option<&Principal>,
    method: &str,
    uri: &str,
    form: Option<String>,
) -> axum::response::Response {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(principal) = principal {
        builder = builder.header(
            header::COOKIE,
            format!("__Host-hone_session={}", principal.session.id),
        );
    }
    let body = match form {
        Some(form) => {
            builder = builder.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
            Body::from(form)
        }
        None => Body::empty(),
    };
    web::router(state.clone())
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap()
}

async fn get(state: &crate::state::AppState, principal: &Principal, uri: &str) -> axum::response::Response {
    request(state, Some(principal), "GET", uri, None).await
}

async fn post(
    state: &crate::state::AppState,
    principal: &Principal,
    uri: &str,
    form: String,
) -> axum::response::Response {
    request(state, Some(principal), "POST", uri, Some(form)).await
}

async fn text(response: axum::response::Response) -> String {
    String::from_utf8(
        to_bytes(response.into_body(), 400_000)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

fn storage_form(principal: &Principal) -> String {
    format!(
        "csrf={}&plan_code=root-plan&kind=storage&image={IMAGE}&file_manager_username=alice&file_manager_password=correct%20horse%20battery",
        principal.session.csrf_token
    )
}

fn two_plan_catalog() -> Value {
    json!([
        {"plan_code":"starter","label":"Starter","memory_high":"128M","memory_max":"256M",
         "cpu_quota_percent":25,"tasks_max":64,"max_services":1,"service_days":7,
         "allowed_kinds":["web","database"],"images":{"web":"approved.example/nginx:1"}},
        {"plan_code":"pro","label":"Pro","memory_high":"256M","memory_max":"512M",
         "cpu_quota_percent":75,"tasks_max":128,"max_services":4,"service_days":30,
         "allowed_kinds":["web","database","container"],"images":{"web":"approved.example/nginx:1"}}
    ])
}

#[tokio::test]
async fn images_page_falls_back_to_an_available_plan_when_the_first_is_full() {
    let mut state = state().await;
    let principal = principal(&state).await;
    let fake = FakeBroker::new(two_plan_catalog(), json!({}), "", None).await;
    state.broker = broker::Client::new(fake.path.clone());
    service(
        &state,
        principal.tenant.as_ref().unwrap(),
        ServiceKind::Web,
        ServiceState::Running,
    )
    .await;
    let response = get(&state, &principal, "/portal/images").await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = text(response).await;
    assert!(html.contains("/portal/deploy?plan_code=pro&amp;kind=web"));
    assert!(html.contains("Disponible"));
}

#[tokio::test]
async fn images_page_offers_a_second_instance_of_a_live_kind() {
    let mut state = state().await;
    let principal = principal(&state).await;
    let fake = FakeBroker::new(catalog(), json!({}), "", None).await;
    state.broker = broker::Client::new(fake.path.clone());
    service(
        &state,
        principal.tenant.as_ref().unwrap(),
        ServiceKind::Web,
        ServiceState::Running,
    )
    .await;
    let response = get(&state, &principal, "/portal/images").await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = text(response).await;
    assert!(html.contains("Sitio web"));
    assert!(html.contains("/portal/deploy?plan_code=root-plan&amp;kind=web"));
    assert!(html.contains("Disponible"));
}

#[tokio::test]
async fn web_site_tab_reads_and_writes_text_files_through_the_broker() {
    let mut state = state().await;
    let principal = principal(&state).await;
    let fake = FakeBroker::new(catalog(), json!({}), "", None).await;
    state.broker = broker::Client::new(fake.path.clone());
    let web = service(
        &state,
        principal.tenant.as_ref().unwrap(),
        ServiceKind::Web,
        ServiceState::Running,
    )
    .await;

    let response = get(&state, &principal, &format!("/portal/services/{}/site", web.id)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = text(response).await;
    assert!(html.contains("Contenido del sitio"));
    assert!(html.contains("&lt;h1&gt;anterior&lt;/h1&gt;"));

    let form = format!(
        "csrf={}&site_path=index.html&site_content=%3Ch1%3Ehola%3C%2Fh1%3E",
        principal.session.csrf_token
    );
    let response = post(
        &state,
        &principal,
        &format!("/portal/services/{}/site", web.id),
        form,
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let requests = fake.requests.lock().unwrap();
    let write = requests
        .iter()
        .find(|request| request["op"] == "site.write")
        .unwrap();
    assert_eq!(write["service"], web.unit_name);
    assert_eq!(write["site_path"], "index.html");
    assert_eq!(write["site_content"], "<h1>hola</h1>");

    // The tab is web-only; a database service has no content editor.
    let database = service(
        &state,
        principal.tenant.as_ref().unwrap(),
        ServiceKind::Database,
        ServiceState::Running,
    )
    .await;
    let response = get(
        &state,
        &principal,
        &format!("/portal/services/{}/site", database.id),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn storage_deploy_validates_settings_and_never_persists_the_password() {
    let mut state = state().await;
    let principal = principal(&state).await;
    let fake = FakeBroker::new(catalog(), storage_configuration(), "", None).await;
    state.broker = broker::Client::new(fake.path.clone());

    let response = post(&state, &principal, "/portal/services", storage_form(&principal)).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let limits = fake
        .requests
        .lock()
        .unwrap()
        .iter()
        .find(|request| request["op"] == "limits.set")
        .unwrap()
        .clone();
    assert_eq!(limits["options"]["image"], IMAGE);
    assert_eq!(limits["options"]["file_manager"]["username"], "alice");
    assert_eq!(limits["options"]["file_manager"]["read_only"], false);
    for job in registry::recent_jobs(&state.db, 10).await.unwrap() {
        assert!(!job.payload_json.contains(PASSWORD));
        assert!(!job.payload_json.contains("password"));
        assert!(job.payload_json.contains("\"username\":\"alice\""));
    }
    for event in registry::audit_of_tenant(&state.db, principal.tenant_id().unwrap(), 10)
        .await
        .unwrap()
    {
        assert!(event
            .reason
            .as_deref()
            .map(|reason| !reason.contains(PASSWORD))
            .unwrap_or(true));
    }

    // Validation errors re-render the wizard with a blank password and no new service.
    let base = storage_form(&principal);
    for (form, expected) in [
        (base.replace("correct%20horse%20battery", "short"), "12 y 72 bytes"),
        (base.replace("alice", "bad%20user"), "1-32 caracteres"),
        (base.replace("correct%20horse%20battery", ""), "no puede estar vacía"),
    ] {
        let response = post(&state, &principal, "/portal/services", form.clone()).await;
        assert_eq!(response.status(), StatusCode::OK, "{form}");
        let html = text(response).await;
        assert!(html.contains(expected), "{form}: {html}");
        assert!(html.contains("name=\"file_manager_password\""));
        assert!(!html.contains("name=\"file_manager_password\" value="));
        assert!(!html.contains(PASSWORD));
    }
    // A legacy storage post without settings is refused before any reservation.
    let response = post(
        &state,
        &principal,
        "/portal/services",
        format!(
            "csrf={}&plan_code=root-plan&kind=storage",
            principal.session.csrf_token
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        registry::count_services(&state.db, principal.tenant_id().unwrap())
            .await
            .unwrap(),
        1
    );
    assert!(!fake.ops().iter().any(|op| op == "service.configure"));
}

#[tokio::test]
async fn image_and_kind_options_are_validated_against_the_fresh_catalogue() {
    let mut state = state().await;
    let principal = principal(&state).await;
    let fake = FakeBroker::new(catalog(), storage_configuration(), "", None).await;
    state.broker = broker::Client::new(fake.path.clone());

    let response = post(
        &state,
        &principal,
        "/portal/services",
        storage_form(&principal).replace(IMAGE, "evil.example/unknown:1"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = text(response).await;
    assert!(html.contains("debe coincidir exactamente"));
    assert!(!html.contains(PASSWORD));
    assert!(fake.ops().iter().all(|op| op == "catalog.list"));

    // File-manager credentials are storage-only, even with a valid image.
    let response = post(
        &state,
        &principal,
        "/portal/services",
        format!(
            "csrf={}&plan_code=root-plan&kind=web&image=approved.example/nginx:1&file_manager_username=alice&file_manager_password=correct%20horse%20battery",
            principal.session.csrf_token
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(text(response).await.contains("solo aplican al almacenamiento"));
    assert_eq!(
        registry::count_services(&state.db, principal.tenant_id().unwrap())
            .await
            .unwrap(),
        0
    );
    assert!(fake.ops().iter().all(|op| op == "catalog.list"));

    // Web/database wizard posts carry the approved image and no credentials.
    let response = post(
        &state,
        &principal,
        "/portal/services",
        format!(
            "csrf={}&plan_code=root-plan&kind=web&image=approved.example/nginx:1",
            principal.session.csrf_token
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let limits = fake
        .requests
        .lock()
        .unwrap()
        .iter()
        .find(|request| request["op"] == "limits.set")
        .unwrap()
        .clone();
    assert_eq!(limits["options"]["image"], "approved.example/nginx:1");
    assert!(limits["options"].get("file_manager").is_none());
}

#[tokio::test]
async fn image_catalogue_and_deploy_wizard_render_approved_references_safely() {
    let mut state = state().await;
    let principal = principal(&state).await;
    let mut policy = catalog();
    policy[0]["images"]["database"] = json!("evil\"><script>alert(1)</script>");
    let fake = FakeBroker::new(policy, storage_configuration(), "", None).await;
    state.broker = broker::Client::new(fake.path.clone());

    let anonymous = request(&state, None, "GET", "/portal/images", None).await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    let response = get(&state, &principal, "/portal/images").await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = text(response).await;
    assert!(html.contains("/portal/deploy?plan_code=root-plan&amp;kind=storage"));
    assert!(html.contains("https://hub.docker.com/r/filebrowser/filebrowser"));
    assert!(html.contains("archivado"));
    assert!(html.contains("approved.example/nginx:1"));
    // Unknown registries get no fabricated Docker Hub search link.
    assert!(!html.contains("hub.docker.com/search"));
    // Unsafe references are escaped and cannot be deployed.
    assert!(html.contains("&lt;script&gt;"));
    assert!(!html.contains("<script>alert(1)</script>"));
    assert!(!html.contains("kind=database"));
    assert!(html.contains("Referencia no válida"));

    let response = get(
        &state,
        &principal,
        "/portal/deploy?plan_code=root-plan&kind=storage",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = text(response).await;
    assert!(html.contains(&format!("name=\"image\" value=\"{IMAGE}\"")));
    assert!(html.contains("name=\"file_manager_username\""));
    assert!(html.contains("name=\"file_manager_password\""));
    assert!(!html.contains("name=\"file_manager_password\" value="));
    assert!(html.contains("name=\"file_manager_read_only\""));
    assert!(html.contains("19001"));
    assert!(html.contains("/srv/test/t01/vault"));
    assert!(html.contains("archivado"));
    assert!(html.contains("<script src=\"/static/hone.js\" defer></script>"));
    assert!(!html.replace("<script src=\"/static/hone.js\" defer></script>", "").contains("<script"));

    for uri in [
        "/portal/deploy?plan_code=local-only&kind=storage",
        "/portal/deploy?plan_code=root-plan&kind=git",
        "/portal/deploy?plan_code=root-plan&kind=shell",
        "/portal/deploy?plan_code=root-plan&kind=storage&image=evil",
        "/portal/deploy?plan_code=root-plan",
    ] {
        let response = get(&state, &principal, uri).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
    }
    let response = get(&state, &principal, "/portal/deploy?plan_code=root-plan&kind=database").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn configuration_checks_csrf_ownership_and_state_before_any_broker_call() {
    let mut state = state().await;
    let principal = principal(&state).await;
    let tenant = principal.tenant.clone().unwrap();
    let storage = service(&state, &tenant, ServiceKind::Storage, ServiceState::Stopped).await;
    let web = service(&state, &tenant, ServiceKind::Web, ServiceState::Stopped).await;
    let fake = FakeBroker::new(catalog(), storage_configuration(), "", None).await;
    state.broker = broker::Client::new(fake.path.clone());
    let configuration_uri = format!("/portal/services/{}/configuration", storage.id);

    assert_eq!(
        request(&state, None, "GET", &configuration_uri, None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let other = auth::create_account(&state.db, Role::Tenant, "other-password", None)
        .await
        .unwrap();
    let other_tenant = registry::create_tenant(&state.db, other.id, "t02", "Other")
        .await
        .unwrap();
    let other_service = service(&state, &other_tenant, ServiceKind::Storage, ServiceState::Stopped).await;
    assert_eq!(
        get(&state, &principal, &format!("/portal/services/{}/configuration", other_service.id))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        post(
            &state,
            &principal,
            &configuration_uri,
            "csrf=wrong&read_only=on".to_string()
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert!(fake.requests.lock().unwrap().is_empty());

    // Only storage, only while stopped; the broker stays untouched otherwise.
    assert_eq!(
        post(
            &state,
            &principal,
            &format!("/portal/services/{}/configuration", web.id),
            format!("csrf={}&read_only=on", principal.session.csrf_token)
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    registry::set_service_state(&state.db, storage.id, ServiceState::Running, None)
        .await
        .unwrap();
    assert_eq!(
        post(
            &state,
            &principal,
            &configuration_uri,
            format!("csrf={}&read_only=on", principal.session.csrf_token)
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    // Tampered extra fields are rejected by the strict form schema.
    assert_eq!(
        post(
            &state,
            &principal,
            &configuration_uri,
            format!("csrf={}&username=mallory&read_only=on", principal.session.csrf_token)
        )
        .await
        .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert!(fake.requests.lock().unwrap().is_empty());

    // Expired services lose access before inspection.
    registry::set_service_state(&state.db, storage.id, ServiceState::Stopped, None)
        .await
        .unwrap();
    sqlx::query("UPDATE services SET expires_at = ? WHERE id = ?")
        .bind(OffsetDateTime::now_utc() - time::Duration::days(30))
        .bind(storage.id)
        .execute(&state.db)
        .await
        .unwrap();
    assert_eq!(
        get(&state, &principal, &configuration_uri).await.status(),
        StatusCode::FORBIDDEN
    );
    assert!(fake.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn inspect_and_configure_use_trusted_username_and_keep_records_without_secrets() {
    let mut state = state().await;
    let principal = principal(&state).await;
    let tenant = principal.tenant.clone().unwrap();
    let storage = service(&state, &tenant, ServiceKind::Storage, ServiceState::Stopped).await;
    let fake = FakeBroker::new(catalog(), storage_configuration(), "", None).await;
    state.broker = broker::Client::new(fake.path.clone());
    let uri = format!("/portal/services/{}/configuration", storage.id);

    let response = get(&state, &principal, &uri).await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = text(response).await;
    assert!(html.contains("alice"));
    assert!(html.contains(IMAGE));
    assert!(html.contains("Resumen"));
    assert!(html.contains("Registros"));
    assert!(!html.contains("type=\"password\""));
    assert!(html.contains(&format!("action=\"{uri}\"")));

    let response = post(
        &state,
        &principal,
        &uri,
        format!("csrf={}&read_only=on", principal.session.csrf_token),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers()[header::LOCATION],
        format!("{uri}?updated=1")
    );
    let configure = fake
        .requests
        .lock()
        .unwrap()
        .iter()
        .find(|request| request["op"] == "service.configure")
        .unwrap()
        .clone();
    assert_eq!(configure["tenant_id"], "t01");
    assert_eq!(configure["service"], "t01-vault");
    assert!(configure.get("kind").is_none() && configure.get("plan_code").is_none());
    assert_eq!(configure["options"]["file_manager"]["username"], "alice");
    assert_eq!(configure["options"]["file_manager"]["password"], "");
    assert_eq!(configure["options"]["file_manager"]["read_only"], true);
    assert!(configure["options"].get("image").is_none());
    let jobs = registry::recent_jobs(&state.db, 5).await.unwrap();
    assert_eq!(jobs[0].action, "service.configure");
    assert!(matches!(jobs[0].state, crate::models::JobState::Done));
    assert!(!jobs[0].payload_json.contains("password"));
    let events = registry::audit_of_tenant(&state.db, tenant.id, 5).await.unwrap();
    assert_eq!(events[0].action, "service.configure");
    assert_eq!(events[0].outcome, "ok");

    let response = get(&state, &principal, &format!("{uri}?updated=1")).await;
    let html = text(response).await;
    assert!(html.contains("Configuración guardada"));
}

#[tokio::test]
async fn refused_configure_preserves_the_service_and_records_the_failure() {
    let mut state = state().await;
    let principal = principal(&state).await;
    let tenant = principal.tenant.clone().unwrap();
    let storage = service(&state, &tenant, ServiceKind::Storage, ServiceState::Stopped).await;
    let fake = FakeBroker::new(
        catalog(),
        storage_configuration(),
        "",
        Some("service.configure"),
    )
    .await;
    state.broker = broker::Client::new(fake.path.clone());
    let uri = format!("/portal/services/{}/configuration", storage.id);
    let response = post(
        &state,
        &principal,
        &uri,
        format!("csrf={}&read_only=on", principal.session.csrf_token),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let html = text(response).await;
    assert!(html.contains("root policy refused"));
    assert!(html.contains("alice"));
    let saved = registry::service_by_id(&state.db, storage.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.state, ServiceState::Stopped);
    assert!(saved.deleted_at.is_none());
    let jobs = registry::recent_jobs(&state.db, 5).await.unwrap();
    assert!(matches!(jobs[0].state, crate::models::JobState::Failed));
    assert!(!jobs[0].payload_json.contains("password"));
    let events = registry::audit_of_tenant(&state.db, tenant.id, 5).await.unwrap();
    assert_eq!(events[0].outcome, "failed");
}

#[tokio::test]
async fn inspect_is_read_only_for_non_storage_and_refuses_unsanitized_answers() {
    let mut state = state().await;
    let principal = principal(&state).await;
    let tenant = principal.tenant.clone().unwrap();
    let web_service = service(&state, &tenant, ServiceKind::Web, ServiceState::Running).await;
    let fake = FakeBroker::new(
        catalog(),
        json!({"kind": "web", "image": "approved.example/nginx:1"}),
        "",
        None,
    )
    .await;
    state.broker = broker::Client::new(fake.path.clone());
    let response = get(
        &state,
        &principal,
        &format!("/portal/services/{}/configuration", web_service.id),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = text(response).await;
    assert!(body.contains("approved.example/nginx:1"));
    assert!(body.contains("Sin ajustes editables"));
    assert!(!body.contains("type=\"password\""));
    assert!(!body.contains("Guardar configuración"));

    // A broker answer carrying secrets or unknown fields fails closed, not rendered.
    let storage = service(&state, &tenant, ServiceKind::Storage, ServiceState::Stopped).await;
    let hostile = FakeBroker::new(
        catalog(),
        json!({"kind": "storage", "image": IMAGE, "username": "alice", "read_only": false, "password": "leak"}),
        "",
        None,
    )
    .await;
    state.broker = broker::Client::new(hostile.path.clone());
    let response = get(
        &state,
        &principal,
        &format!("/portal/services/{}/configuration", storage.id),
    )
    .await;
    // The tab degrades to an error notice instead of a 500, and the hostile
    // answer is never rendered.
    assert_eq!(response.status(), StatusCode::OK);
    let body = text(response).await;
    assert!(!body.contains("leak"));
    assert!(body.contains("no está disponible"));
}

#[tokio::test]
async fn logs_are_bounded_tenant_scoped_and_escaped() {
    let mut state = state().await;
    let principal = principal(&state).await;
    let tenant = principal.tenant.clone().unwrap();
    let storage = service(&state, &tenant, ServiceKind::Storage, ServiceState::Running).await;
    let fake = FakeBroker::new(
        catalog(),
        storage_configuration(),
        "2026-01-01 boot\n<script>alert(1)</script>\nready",
        None,
    )
    .await;
    state.broker = broker::Client::new(fake.path.clone());

    let response = get(
        &state,
        &principal,
        &format!("/portal/services/{}/logs?lines=5000&unit=other&path=/etc", storage.id),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = text(response).await;
    assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(!html.contains("<script>alert(1)</script>"));
    assert!(html.contains("500"));
    let requests = fake.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["op"], "logs.fetch");
    assert_eq!(requests[0]["lines"], 500);
    assert_eq!(requests[0]["service"], "t01-vault");
    assert_eq!(requests[0]["tenant_id"], "t01");
    drop(requests);

    let other = auth::create_account(&state.db, Role::Tenant, "other-password", None)
        .await
        .unwrap();
    let other_tenant = registry::create_tenant(&state.db, other.id, "t02", "Other")
        .await
        .unwrap();
    let other_service = service(&state, &other_tenant, ServiceKind::Storage, ServiceState::Running).await;
    assert_eq!(
        get(&state, &principal, &format!("/portal/services/{}/logs", other_service.id))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(fake.requests.lock().unwrap().len(), 1);
}


#[tokio::test]
async fn generic_form_errors_retain_nonsecret_fields_without_host_effects() {
    let mut state = state().await;
    let principal = principal(&state).await;
    let mut policy = catalog();
    policy[0]["allowed_kinds"] = json!(["container"]);
    policy[0]["registries"] = json!(["approved.example"]);
    let fake = FakeBroker::new(policy, json!({}), "", None).await;
    state.broker = broker::Client::new(fake.path.clone());
    let original = format!("csrf={}&name=forge-one&plan_code=root-plan&image=approved.example/forgejo:1&container_port=3000&user=1001&environment=PASSWORD%3Dnever-echo-this&volumes=data%3D/var/lib/gitea%0Aconfig%3D/etc/gitea&read_only=true",principal.session.csrf_token);
    for form in [
        original.replace("approved.example/forgejo:1", "denied.example/forgejo:1"),
        original.replace("container_port=3000", "container_port=70000"),
        original.replace("plan_code=root-plan", "plan_code=revoked"),
        original.replace("PASSWORD%3Dnever-echo-this", "PASSWORD%3Dnever-echo-this%0APASSWORD%3Dduplicate-secret"),
    ] {
        let response = post(&state,&principal,"/portal/containers/new",form.clone()).await;
        assert_eq!(response.status(),StatusCode::BAD_REQUEST);
        let html = text(response).await;
        for expected in ["value=\"forge-one\"", "value=\"1001\"", "data=/var/lib/gitea\nconfig=/etc/gitea", "value=\"true\" checked"] {
            assert!(html.contains(expected),"missing: {expected}");
        }
        let plan = if form.contains("plan_code=revoked") { "revoked" } else { "root-plan" };
        assert!(html.contains(&format!("value=\"{plan}\" selected")));
        let port = if form.contains("container_port=70000") { "70000" } else { "3000" };
        assert!(html.contains(&format!("value=\"{port}\"")));
        let image = if form.contains("denied.example") { "denied.example/forgejo:1" } else { "approved.example/forgejo:1" };
        assert!(html.contains(&format!("value=\"{image}\"")));
        assert!(!html.contains("never-echo-this"));
        assert!(!html.contains("duplicate-secret"));
        assert!(html.split("id=\"environment\"").nth(1).unwrap().split_once('>').unwrap().1.starts_with("</textarea>"));
    }
    assert!(fake.ops().iter().all(|op|op=="catalog.list"));
    assert_eq!(registry::count_services(&state.db,principal.tenant_id().unwrap()).await.unwrap(),0);
}

#[tokio::test]
async fn optional_browser_script_is_served_with_self_only_csp() {
    let mut state = state().await;
    Arc::make_mut(&mut state.config).static_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../static");
    let response = request(&state,None,"GET","/static/hone.js",None).await;
    assert_eq!(response.status(),StatusCode::OK);
    assert!(response.headers()[header::CONTENT_TYPE].to_str().unwrap().contains("javascript"));
    let csp = response.headers()[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
    assert!(csp.contains("script-src 'self'"));
    assert!(!csp.contains("unsafe-inline"));
    assert!(!csp.contains("unsafe-eval"));
    assert!(text(response).await.contains("crypto.getRandomValues"));
}

#[tokio::test]
async fn generic_images_multiple_instances_secrets_and_safe_detail() {
    let mut state = state().await;
    let principal = principal(&state).await;
    let mut policy = catalog();
    policy[0]["allowed_kinds"] = json!(["container"]);
    policy[0]["registries"] = json!(["approved.example"]);
    let info = json!({"name":"t01-one","image":"approved.example/arbitrary:2","host_port":30001,"container_port":8080,"user":1000,"read_only":false,"volumes":[],"environment_keys":["APP_SECRET"]});
    let fake = FakeBroker::new(policy.clone(),info.clone(),"safe logs",None).await;
    state.broker = broker::Client::new(fake.path.clone());
    let form = |name:&str,image:&str| format!("csrf={}&name={name}&plan_code=root-plan&image={image}&container_port=8080&user=1000&environment=APP_SECRET%3Dunique-secret-value",principal.session.csrf_token);
    assert_eq!(post(&state,&principal,"/portal/containers/new",form("one","approved.example/arbitrary:2")).await.status(),StatusCode::SEE_OTHER);
    assert_eq!(post(&state,&principal,"/portal/containers/new",form("two","approved.example/unrelated:9")).await.status(),StatusCode::SEE_OTHER);
    assert_eq!(registry::count_services(&state.db,principal.tenant_id().unwrap()).await.unwrap(),2);
    let records = registry::services_of_tenant(&state.db,principal.tenant_id().unwrap()).await.unwrap();
    let html = text(get(&state,&principal,&format!("/portal/services/{}",records[0].id)).await).await;
    assert!(html.contains("APP_SECRET"));
    assert!(html.contains("approved.example/arbitrary:2"));
    assert!(!html.contains("unique-secret-value"));
    let before = fake.ops().len();
    let missing = get(&state,&principal,"/portal/services/99999/configuration").await;
    assert_eq!(missing.status(),StatusCode::NOT_FOUND);
    assert_eq!(fake.ops().len(),before);
    // A service owned by another tenant is hidden before container.inspect.
    sqlx::query("INSERT INTO accounts (id,number,role,password_hash,created_at) VALUES (999,'999999999999','tenant','unused','2026-01-01')").execute(&state.db).await.unwrap();
    sqlx::query("INSERT INTO tenants (id,account_id,slug,name,created_at) VALUES (999,999,'t99','other','2026-01-01')").execute(&state.db).await.unwrap();
    sqlx::query("UPDATE services SET tenant_id=999 WHERE id=?").bind(records[1].id).execute(&state.db).await.unwrap();
    for suffix in ["", "/configuration", "/logs"] {
        assert_eq!(get(&state,&principal,&format!("/portal/services/{}{suffix}",records[1].id)).await.status(),StatusCode::NOT_FOUND);
    }
    assert_eq!(fake.ops().len(),before);
    sqlx::query("UPDATE services SET tenant_id=? WHERE id=?").bind(principal.tenant_id().unwrap()).bind(records[1].id).execute(&state.db).await.unwrap();
    let bad = form("three","approved.example/app:2").replace(&principal.session.csrf_token,"invalid");
    assert_eq!(post(&state,&principal,"/portal/containers/new",bad).await.status(),StatusCode::FORBIDDEN);
    assert_eq!(fake.ops().len(),before);
    for image in ["denied.example/app:2","nginx:latest"] {
        assert_eq!(post(&state,&principal,"/portal/containers/new",form("three",image)).await.status(),StatusCode::BAD_REQUEST);
    }
    let failed = FakeBroker::new(policy,info,"",Some("container.create")).await;
    state.broker = broker::Client::new(failed.path.clone());
    assert_eq!(post(&state,&principal,"/portal/containers/new",form("three","approved.example/app:2")).await.status(),StatusCode::SEE_OTHER);
    for job in registry::recent_jobs(&state.db,20).await.unwrap() {
        assert!(!format!("{job:?}").contains("unique-secret-value"));
    }
    for event in registry::recent_audit(&state.db,20).await.unwrap() {
        assert!(!format!("{event:?}").contains("unique-secret-value"));
    }
    let metadata:Vec<(String,String)> = sqlx::query_as("SELECT ports_json,volume_json FROM services").fetch_all(&state.db).await.unwrap();
    assert!(!format!("{metadata:?}").contains("unique-secret-value"));
    let html = text(get(&state,&principal,"/portal/containers/new").await).await;
    assert!(html.contains("name=\"environment\""));
    assert!(!html.contains("unique-secret-value"));
}
