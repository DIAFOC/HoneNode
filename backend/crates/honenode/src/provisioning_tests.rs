use std::collections::BTreeSet;
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

use crate::models::{Principal, Role, ServiceKind, ServiceState, TenantStatus};
use crate::tests::{state, tenant};
use crate::{auth, broker, portal_catalog, provisioning, registry, web};

fn catalog(max: i64) -> Value {
    json!([{
        "plan_code": "root-plan", "label": "Root plan", "memory_high": "256M",
        "memory_max": "512M", "cpu_quota_percent": 75, "tasks_max": 128,
        "max_services": max, "service_days": 17,
        "allowed_kinds": ["web", "database", "storage", "git"],
        "images": {"web": "approved.example/nginx:1", "storage": "approved.example/filebrowser:1", "git": null}
    }])
}

struct FakeBroker {
    path: PathBuf,
    requests: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeBroker {
    async fn new(catalog: Value, refuse: Option<&'static str>) -> Self {
        let path = std::env::temp_dir().join(format!(
            "hone-prov-{}.sock",
            auth::generate_token().unwrap()
        ));
        let listener = UnixListener::bind(&path).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
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
                } else if op == "catalog.list" {
                    response["catalog"] = catalog.clone();
                } else if op == "service.status" {
                    response["unit"] = request["service"].clone();
                    response["uid"] = json!(1001);
                    response["active_state"] = json!("active");
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
}

impl Drop for FakeBroker {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn principal(state: &crate::state::AppState) -> Principal {
    let (account, tenant) = tenant(state).await;
    let session =
        auth::create_session(&state.db, account.id, Duration::from_secs(3600), None, None)
            .await
            .unwrap();
    Principal {
        account,
        tenant: Some(tenant),
        session,
    }
}

async fn post(
    state: &crate::state::AppState,
    principal: &Principal,
    uri: &str,
    form: String,
) -> axum::response::Response {
    web::router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header(
                    header::COOKIE,
                    format!("__Host-hone_session={}", principal.session.id),
                )
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(form))
                .unwrap(),
        )
        .await
        .unwrap()
}

#[test]
fn catalog_is_root_schema_and_selection_fails_closed() {
    let parsed = portal_catalog::parse(catalog(4)).unwrap();
    assert_eq!(
        portal_catalog::select(&parsed, "root-plan", ServiceKind::Web)
            .unwrap()
            .service_days,
        17
    );
    assert!(portal_catalog::select(&parsed, "local-only", ServiceKind::Web).is_err());
    for (key, value) in [
        ("service_days", json!(0)),
        ("max_services", json!(-1)),
        ("cpu_quota_percent", json!(0)),
        ("allowed_kinds", json!(["shell"])),
    ] {
        let mut invalid = catalog(4);
        invalid[0][key] = value;
        assert!(portal_catalog::parse(invalid).is_err(), "{key}");
    }
    let mut restricted = catalog(4);
    restricted[0]["allowed_kinds"] = json!(["web"]);
    let restricted = portal_catalog::parse(restricted).unwrap();
    assert!(portal_catalog::select(&restricted, "root-plan", ServiceKind::Git).is_err());
    assert!(portal_catalog::parse(json!([catalog(4)[0], catalog(4)[0]])).is_err());

    // Honed emits string-valued images and no label; the UI also supports absent image values.
    let mut root_wire = catalog(4);
    root_wire[0].as_object_mut().unwrap().remove("label");
    root_wire[0]["images"] = json!({"web":"approved.example/nginx:1"});
    let response: broker::Response = serde_json::from_value(json!({
        "ok":true,"op":"catalog.list","catalog":root_wire
    }))
    .unwrap();
    let parsed = portal_catalog::parse(response.catalog.unwrap()).unwrap();
    assert_eq!(parsed[0].label, "root-plan");
    assert_eq!(
        parsed[0].images["web"].as_deref(),
        Some("approved.example/nginx:1")
    );
    let parsed = portal_catalog::parse(catalog(4)).unwrap();
    assert_eq!(parsed[0].images["git"], None);
}

#[test]
fn allocations_are_unique_and_match_fixed_broker_contract() {
    let mut ports = BTreeSet::new();
    for number in 1..=999 {
        for kind in ServiceKind::all().into_iter().filter(|k| *k != ServiceKind::Container) {
            let slug = format!("t{number:02}");
            let (_, encoded, _) = provisioning::allocation(
                &slug,
                kind,
                std::path::Path::new("/srv/test"),
                1,
                None,
            )
            .unwrap();
            let encoded: Value = serde_json::from_str(&encoded).unwrap();
            for port in encoded.as_object().unwrap().values() {
                assert!(ports.insert(port.as_u64().unwrap()));
            }
        }
    }
    for (kind, suffix, port) in [
        (ServiceKind::Web, "site", 18001),
        (ServiceKind::Database, "db", 15431),
        (ServiceKind::Storage, "vault", 19001),
        (ServiceKind::Git, "git", 20001),
    ] {
        let (unit, ports, _) = provisioning::allocation(
            "t01",
            kind,
            std::path::Path::new("/srv/test"),
            1,
            None,
        )
        .unwrap();
        assert_eq!(unit, format!("t01-{suffix}"));
        assert!(ports.contains(&port.to_string()));
    }
    // A second and third instance of the same kind get distinct names, ports
    // and data directories.
    let mut second = BTreeSet::new();
    for instance in 2..=3 {
        for kind in [ServiceKind::Web, ServiceKind::Database] {
            let (unit, ports, volumes) = provisioning::allocation(
                "t01",
                kind,
                std::path::Path::new("/srv/test"),
                instance,
                None,
            )
            .unwrap();
            assert!(second.insert(unit.clone()), "{unit} reused");
            let encoded: Value = serde_json::from_str(&ports).unwrap();
            for port in encoded.as_object().unwrap().values() {
                assert!(second.insert(port.as_u64().unwrap().to_string()), "port reused");
            }
            assert!(volumes.contains(&format!("-{instance}")));
        }
    }
    for slug in ["t0", "t1000", "bad", "../t01"] {
        assert!(provisioning::allocation(
            slug,
            ServiceKind::Web,
            std::path::Path::new("/srv/test"),
            1,
            None,
        )
        .is_err());
    }
}

#[tokio::test]
async fn tenant_provisions_all_four_kinds_with_fresh_root_days_jobs_audit_and_observations() {
    let mut state = state().await;
    let principal = principal(&state).await;
    let fake = FakeBroker::new(catalog(4), None).await;
    state.broker = broker::Client::new(fake.path.clone());
    let started = OffsetDateTime::now_utc();
    for kind in ["web", "database", "storage", "git"] {
        let mut form = format!(
            "csrf={}&plan_code=root-plan&kind={kind}",
            principal.session.csrf_token,
        );
        if kind == "storage" {
            form.push_str("&image=approved.example/filebrowser:1&file_manager_username=alice&file_manager_password=correct%20horse%20battery&file_manager_read_only=on");
        }
        let response = post(&state, &principal, "/portal/services", form).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
    }
    let services = registry::services_of_tenant(&state.db, principal.tenant_id().unwrap())
        .await
        .unwrap();
    assert_eq!(services.len(), 4);
    for service in services {
        assert_eq!(service.state, ServiceState::Running);
        assert_eq!(service.observed_state.as_deref(), Some("active"));
        assert!(service.expires_at.unwrap() >= started + time::Duration::days(17));
        assert!(service.expires_at.unwrap() < OffsetDateTime::now_utc() + time::Duration::days(17));
    }
    let jobs = registry::recent_jobs(&state.db, 10).await.unwrap();
    assert_eq!(jobs.len(), 4);
    assert!(jobs
        .iter()
        .all(|job| matches!(job.state, crate::models::JobState::Done)));
    for job in &jobs {
        assert!(!job.payload_json.contains("correct horse"));
        assert!(!job.payload_json.contains("password"));
        assert!(job.error.is_none());
    }
    let storage_job = jobs
        .iter()
        .find(|job| job.payload_json.contains("\"kind\":\"storage\""))
        .unwrap();
    assert!(storage_job.payload_json.contains("\"username\":\"alice\""));
    assert!(storage_job.payload_json.contains("\"read_only\":true"));
    let events = registry::audit_of_tenant(&state.db, principal.tenant_id().unwrap(), 10)
        .await
        .unwrap();
    assert_eq!(events.len(), 4);
    assert!(events.iter().all(|event| event.outcome == "ok"
        && event.actor_id.as_deref() == Some(principal.account.number.as_str())));
    assert!(events.iter().all(|event| event
        .reason
        .as_deref()
        .map(|reason| !reason.contains("correct horse"))
        .unwrap_or(true)));
    let requests = fake.requests.lock().unwrap();
    assert_eq!(requests.len(), 20);
    for sequence in requests.chunks(5) {
        assert_eq!(
            sequence
                .iter()
                .map(|request| request["op"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec![
                "catalog.list",
                "tenant.create",
                "limits.set",
                "service.start",
                "service.status"
            ]
        );
        assert!(sequence
            .iter()
            .all(|request| request["request_id"] == sequence[0]["request_id"]));
        assert_eq!(sequence[2]["plan_code"], "root-plan");
        assert!(sequence[2].get("image").is_none());
        if sequence[2]["kind"] == "storage" {
            assert_eq!(
                sequence[2]["options"]["image"],
                "approved.example/filebrowser:1"
            );
            assert_eq!(sequence[2]["options"]["file_manager"]["username"], "alice");
            assert_eq!(sequence[2]["options"]["file_manager"]["read_only"], true);
        } else {
            assert!(sequence[2].get("options").is_none());
        }
    }
}

#[tokio::test]
async fn creation_rejects_csrf_inactive_and_arbitrary_inputs_before_host_effects() {
    let state = state().await;
    let principal = principal(&state).await;
    let valid = format!(
        "csrf={}&plan_code=root-plan&kind=web",
        principal.session.csrf_token
    );
    let response = web::router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/portal/services")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(valid.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        post(
            &state,
            &principal,
            "/portal/services",
            "csrf=wrong&plan_code=root-plan&kind=web".to_string()
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    for extra in [
        "days=1",
        "service_days=1",
        "env=x",
        "path=/etc",
        "tenant_id=2",
        "plan=pro",
        "username=alice",
        "password=whatever",
        "read_only=on",
    ] {
        assert_eq!(
            post(
                &state,
                &principal,
                "/portal/services",
                format!("{valid}&{extra}")
            )
            .await
            .status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    // Incomplete or tampered file-manager fields fail before any broker call.
    for extra in [
        "file_manager_username=alice",
        "file_manager_password=correct%20horse%20battery",
        "file_manager_read_only=perhaps",
    ] {
        assert_eq!(
            post(
                &state,
                &principal,
                "/portal/services",
                format!("{valid}&{extra}")
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST,
            "{extra}"
        );
    }
    for status in [
        TenantStatus::Grace,
        TenantStatus::Suspended,
        TenantStatus::Purged,
    ] {
        registry::set_tenant_status(&state.db, principal.tenant_id().unwrap(), status, None)
            .await
            .unwrap();
        assert_eq!(
            post(&state, &principal, "/portal/services", valid.clone())
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        registry::count_services(&state.db, principal.tenant_id().unwrap())
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn catalog_route_requires_active_tenant_and_uses_correlated_root_catalog() {
    let mut state = state().await;
    let principal = principal(&state).await;
    let fake = FakeBroker::new(catalog(4), None).await;
    state.broker = broker::Client::new(fake.path.clone());
    let app = web::router(state.clone());
    let anonymous = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/portal/catalog")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/portal/catalog")
                .header(
                    header::COOKIE,
                    format!("__Host-hone_session={}", principal.session.id),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    {
        let requests = fake.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["op"], "catalog.list");
        assert_eq!(requests[0]["tenant_id"], "t01");
    }
    registry::set_tenant_status(
        &state.db,
        principal.tenant_id().unwrap(),
        TenantStatus::Grace,
        None,
    )
    .await
    .unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .uri("/portal/catalog")
                .header(
                    header::COOKIE,
                    format!("__Host-hone_session={}", principal.session.id),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(fake.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn fresh_catalog_rejects_local_only_plans_and_disallowed_kinds_without_reservation() {
    let mut state = state().await;
    let principal = principal(&state).await;
    let mut policy = catalog(4);
    policy[0]["allowed_kinds"] = json!(["web"]);
    let fake = FakeBroker::new(policy, None).await;
    state.broker = broker::Client::new(fake.path.clone());
    for (plan, kind) in [("pro", "web"), ("root-plan", "git")] {
        let response = post(
            &state,
            &principal,
            "/portal/services",
            format!(
                "csrf={}&plan_code={plan}&kind={kind}",
                principal.session.csrf_token,
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    assert_eq!(
        registry::count_services(&state.db, principal.tenant_id().unwrap())
            .await
            .unwrap(),
        0
    );
    assert!(fake
        .requests
        .lock()
        .unwrap()
        .iter()
        .all(|request| request["op"] == "catalog.list"));
}

#[tokio::test]
async fn reservation_quota_and_multiple_instances_are_atomic_and_tombstones_remain_reserved() {
    let state = state().await;
    let (_, tenant) = tenant(&state).await;
    let plan = registry::plan_by_code(&state.db, "pro")
        .await
        .unwrap()
        .unwrap();
    let reserve = |kind| {
        registry::reserve_service(
            &state.db,
            tenant.id,
            kind,
            plan.id,
            if kind == ServiceKind::Web {"test-web"} else {"test-git"},
            None,
            "{}",
            "{}",
            OffsetDateTime::now_utc() + time::Duration::days(1),
            1,
        )
    };
    let (first, second) = tokio::join!(reserve(ServiceKind::Web), reserve(ServiceKind::Git));
    assert_ne!(first.is_ok(), second.is_ok());
    let saved = first.or(second).unwrap();
    registry::set_service_state(&state.db, saved.id, ServiceState::Deleted, None)
        .await
        .unwrap();
    // A deleted service frees its kind and unit name for re-provisioning...
    assert!(reserve(saved.kind).await.is_ok());
    // ...while the tenant-wide quota still bounds the tenant.
    let other = if saved.kind == ServiceKind::Web {
        ServiceKind::Git
    } else {
        ServiceKind::Web
    };
    assert!(reserve(other).await.is_err());
    // The database no longer has a one-per-kind index; the plan quota and the
    // unique unit/port/volume reservations are the real limits, so a raw insert
    // of a second unit of the same kind is allowed.
    let duplicate = sqlx::query("INSERT INTO services (tenant_id, kind, plan_id, unit_name, created_at) VALUES (?, ?, ?, ?, ?)")
        .bind(tenant.id).bind(saved.kind).bind(plan.id).bind("raw").bind(OffsetDateTime::now_utc()).execute(&state.db).await;
    assert!(duplicate.is_ok());
}

#[tokio::test]
async fn broker_failure_records_job_audit_and_keeps_failed_kind_reserved() {
    let mut state = state().await;
    let principal = principal(&state).await;
    let fake = FakeBroker::new(catalog(4), Some("limits.set")).await;
    state.broker = broker::Client::new(fake.path.clone());
    let form = format!(
        "csrf={}&plan_code=root-plan&kind=web",
        principal.session.csrf_token
    );
    let response = post(&state, &principal, "/portal/services", form.clone()).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let destination = response.headers()[header::LOCATION]
        .to_str()
        .unwrap()
        .to_string();
    // A second instance is allocated with its own unit name, port and volume;
    // the failed reservation still keeps its allocation.
    assert_eq!(
        post(&state, &principal, "/portal/services", form)
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    let saved = registry::services_of_tenant(&state.db, principal.tenant_id().unwrap())
        .await
        .unwrap();
    assert_eq!(saved.len(), 2);
    assert!(saved.iter().all(|service| service.state == ServiceState::Failed));
    assert_eq!(destination, format!("/portal/services/{}", saved[0].id));
    let detail = web::router(state.clone())
        .oneshot(
            Request::builder()
                .uri(&destination)
                .header(
                    header::COOKIE,
                    format!("__Host-hone_session={}", principal.session.id),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(detail.status(), StatusCode::OK);
    let html = String::from_utf8(
        to_bytes(detail.into_body(), 100_000)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(html.contains("Fallo"));
    assert!(html.contains("service.provision"));
    for action in ["start", "stop", "restart"] {
        assert!(!html.contains(&format!("action=\"{destination}/{action}\"")));
    }
    let jobs = registry::recent_jobs(&state.db, 10).await.unwrap();
    assert!(matches!(jobs[0].state, crate::models::JobState::Failed));
    let events = registry::audit_of_tenant(&state.db, principal.tenant_id().unwrap(), 10)
        .await
        .unwrap();
    assert_eq!(events[0].outcome, "failed");
    assert!(!fake
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|request| request["op"] == "service.start"));
}

#[tokio::test]
async fn operator_uses_the_same_root_validated_provisioning_path() {
    let mut state = state().await;
    let (_, tenant) = tenant(&state).await;
    let account = auth::create_account(&state.db, Role::Operator, "operator-password", None)
        .await
        .unwrap();
    let session = auth::create_session(&state.db, account.id, Duration::from_secs(60), None, None)
        .await
        .unwrap();
    let principal = Principal {
        account,
        session,
        tenant: None,
    };
    let fake = FakeBroker::new(catalog(1), None).await;
    state.broker = broker::Client::new(fake.path.clone());
    let uri = format!("/operator/tenants/{}/services", tenant.id);
    assert_eq!(
        post(
            &state,
            &principal,
            &uri,
            format!(
                "csrf={}&plan_code=root-plan&kind=git",
                principal.session.csrf_token
            )
        )
        .await
        .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        post(
            &state,
            &principal,
            &uri,
            format!(
                "csrf={}&plan_code=root-plan&kind=web",
                principal.session.csrf_token
            )
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    let services = registry::services_of_tenant(&state.db, tenant.id)
        .await
        .unwrap();
    assert_eq!(services[0].unit_name, "t01-git");
}

#[tokio::test]
async fn operator_broker_failure_redirects_to_the_persisted_failed_service() {
    let mut state = state().await;
    let (_, tenant) = tenant(&state).await;
    let account = auth::create_account(&state.db, Role::Operator, "operator-password", None)
        .await
        .unwrap();
    let session = auth::create_session(&state.db, account.id, Duration::from_secs(60), None, None)
        .await
        .unwrap();
    let principal = Principal {
        account,
        session,
        tenant: None,
    };
    let fake = FakeBroker::new(catalog(4), Some("service.start")).await;
    state.broker = broker::Client::new(fake.path.clone());
    let response = post(
        &state,
        &principal,
        &format!("/operator/tenants/{}/services", tenant.id),
        format!(
            "csrf={}&plan_code=root-plan&kind=git",
            principal.session.csrf_token,
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let saved = registry::services_of_tenant(&state.db, tenant.id)
        .await
        .unwrap();
    assert_eq!(saved[0].state, ServiceState::Failed);
    assert_eq!(
        response.headers()[header::LOCATION],
        format!("/operator/services/{}", saved[0].id)
    );
    assert!(saved[0].observed_state.is_none());
    assert!(!fake
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|request| request["op"] == "service.status"));
}

#[tokio::test]
async fn broker_failure_before_reservation_remains_an_error_without_a_service() {
    let mut state = state().await;
    let principal = principal(&state).await;
    let fake = FakeBroker::new(catalog(4), Some("catalog.list")).await;
    state.broker = broker::Client::new(fake.path.clone());
    let response = post(
        &state,
        &principal,
        "/portal/services",
        format!(
            "csrf={}&plan_code=root-plan&kind=web",
            principal.session.csrf_token,
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(response.headers().get(header::LOCATION).is_none());
    assert_eq!(
        registry::count_services(&state.db, principal.tenant_id().unwrap())
            .await
            .unwrap(),
        0
    );
    assert!(registry::recent_jobs(&state.db, 10)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn selected_plan_ceiling_counts_all_tenant_plans_and_matches_catalog_availability() {
    let mut state = state().await;
    let principal = principal(&state).await;
    let tenant_id = principal.tenant_id().unwrap();
    let pro = registry::plan_by_code(&state.db, "pro")
        .await
        .unwrap()
        .unwrap();
    let starter = registry::plan_by_code(&state.db, "starter")
        .await
        .unwrap()
        .unwrap();
    let expiry = OffsetDateTime::now_utc() + time::Duration::days(1);
    registry::reserve_service(
        &state.db,
        tenant_id,
        ServiceKind::Web,
        pro.id,
        "t01-site",
        None,
        "{}",
        "{}",
        expiry,
        4,
    )
    .await
    .unwrap();
    assert!(registry::reserve_service(
        &state.db,
        tenant_id,
        ServiceKind::Database,
        starter.id,
        "t01-db",
        None,
        "{}",
        "{}",
        expiry,
        1
    )
    .await
    .is_err());
    let second = registry::reserve_service(
        &state.db,
        tenant_id,
        ServiceKind::Database,
        starter.id,
        "t01-db",
        None,
        "{}",
        "{}",
        expiry,
        2,
    )
    .await
    .unwrap();
    registry::set_service_state(&state.db, second.id, ServiceState::Failed, None)
        .await
        .unwrap();
    assert_eq!(
        registry::count_services(&state.db, tenant_id)
            .await
            .unwrap(),
        2
    );
    assert!(registry::reserve_service(
        &state.db,
        tenant_id,
        ServiceKind::Storage,
        pro.id,
        "t01-vault",
        None,
        "{}",
        "{}",
        expiry,
        2
    )
    .await
    .is_err());

    let fake = FakeBroker::new(catalog(2), None).await;
    state.broker = broker::Client::new(fake.path.clone());
    let page = web::router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/portal/catalog")
                .header(
                    header::COOKIE,
                    format!("__Host-hone_session={}", principal.session.id),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    let html =
        String::from_utf8(to_bytes(page.into_body(), 100_000).await.unwrap().to_vec()).unwrap();
    assert!(html.contains("No disponible"));
    assert!(!html.contains("action=\"/portal/services\""));

    registry::set_service_state(&state.db, second.id, ServiceState::Deleted, None)
        .await
        .unwrap();
    assert_eq!(
        registry::count_services(&state.db, tenant_id)
            .await
            .unwrap(),
        1
    );
    // Deleting the database frees its kind and unit name: the tenant can
    // re-provision it, and the ceiling still counts only live services.
    assert!(registry::reserve_service(
        &state.db,
        tenant_id,
        ServiceKind::Database,
        starter.id,
        "t01-db",
        None,
        "{}",
        "{}",
        expiry,
        2
    )
    .await
    .is_ok());
    assert!(registry::reserve_service(
        &state.db,
        tenant_id,
        ServiceKind::Storage,
        pro.id,
        "t01-vault",
        None,
        "{}",
        "{}",
        expiry,
        2
    )
    .await
    .is_err());
}

#[tokio::test]
async fn legacy_port_and_volume_reservations_block_reallocation() {
    let state = state().await;
    let (_, tenant) = tenant(&state).await;
    let plan = registry::plan_by_code(&state.db, "pro")
        .await
        .unwrap()
        .unwrap();
    let expiry = OffsetDateTime::now_utc() + time::Duration::days(1);
    registry::reserve_service(
        &state.db,
        tenant.id,
        ServiceKind::Git,
        plan.id,
        "legacy-git",
        None,
        r#"{"web":18001}"#,
        r#"{"database":"/srv/test/t01/pgdata"}"#,
        expiry,
        4,
    )
    .await
    .unwrap();
    for kind in [ServiceKind::Web, ServiceKind::Database] {
        let (unit, ports, volumes) = provisioning::allocation(
            "t01",
            kind,
            std::path::Path::new("/srv/test"),
            1,
            None,
        )
        .unwrap();
        assert!(registry::reserve_service(
            &state.db, tenant.id, kind, plan.id, &unit, None, &ports, &volumes, expiry, 4
        )
        .await
        .is_err());
    }
}
