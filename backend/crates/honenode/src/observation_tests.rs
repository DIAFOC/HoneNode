use std::path::PathBuf;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tower::ServiceExt;

use crate::broker::{ActiveState, Observation, Response};
use crate::models::{Role, ServiceState};
use crate::tests::{reserve, state, tenant};
use crate::{auth, broker, observation, registry, web};

fn report(unit: &str, active_state: &str) -> Value {
    json!({"op": "service.status", "ok": true, "uid": 1001, "unit": unit, "active_state": active_state})
}

fn parsed(value: Value, tenant: &str, unit: &str) -> crate::error::AppResult<Observation> {
    let response: Response = serde_json::from_value(value)
        .map_err(|error| crate::error::AppError::Broker(error.to_string()))?;
    Observation::from_response(response, tenant, unit)
}

#[test]
fn status_contract_rejects_missing_contradictory_and_unbound_data() {
    let wire = serde_json::to_value(broker::Request::ServiceStatus {
        tenant_id: "t01".to_string(),
        service: "t01-site".to_string(),
    })
    .unwrap();
    assert_eq!(
        wire,
        json!({"op":"service.status","tenant_id":"t01","service":"t01-site"})
    );
    let limits = serde_json::to_value(broker::Request::LimitsSet {
        tenant_id: "t01".to_string(),
        service: "t01-site".to_string(),
        kind: "web".to_string(),
        plan_code: "starter".to_string(),
        options: None,
        port: None,
    })
    .unwrap();
    assert_eq!(
        limits,
        json!({"op":"limits.set","tenant_id":"t01","service":"t01-site","kind":"web","plan_code":"starter"})
    );
    let valid = report("t01-site", "active");
    let observation = parsed(valid.clone(), "t01", "t01-site").unwrap();
    assert_eq!(observation.uid(), 1001);
    assert_eq!(observation.unit(), "t01-site");
    assert_eq!(observation.active_state(), ActiveState::Active);
    for key in ["uid", "unit", "active_state"] {
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove(key);
        assert!(parsed(missing, "t01", "t01-site").is_err(), "missing {key}");
        let mut null = valid.clone();
        null[key] = Value::Null;
        assert!(parsed(null, "t01", "t01-site").is_err(), "null {key}");
    }
    for (key, value) in [
        ("ok", json!(false)),
        ("error", json!("refused")),
        ("op", json!("status")),
        ("uid", json!(0)),
        ("uid", json!(u32::MAX)),
        ("uid", json!(-1)),
        ("uid", json!(4294967296u64)),
        ("uid", json!(1.5)),
        ("uid", json!("1001")),
        ("unit", json!("t01-site.service")),
        ("unit", json!("t02-site")),
        ("unit", json!("t01-site\n")),
        ("unit", json!("../t01-site")),
        ("active_state", json!("running")),
        ("active_state", json!("unknown")),
    ] {
        let mut invalid = valid.clone();
        invalid[key] = value;
        assert!(parsed(invalid, "t01", "t01-site").is_err(), "{key}");
    }
    assert!(parsed(valid.clone(), "t02", "t01-site").is_err());
    assert!(parsed(valid, "t01", "t01-site.service").is_err());
    assert!(serde_json::from_str::<Response>(
        r#"{"ok":true,"op":"service.status","uid":1001,"uid":0}"#
    )
    .is_err());
}

#[tokio::test]
async fn stores_real_uid_and_states_without_clearing_policy_or_resurrecting_deletions() {
    let state = state().await;
    let (_, tenant) = tenant(&state).await;
    let service = reserve(&state, &tenant).await;
    for active in [
        "active",
        "reloading",
        "inactive",
        "failed",
        "activating",
        "deactivating",
        "maintenance",
        "refreshing",
    ] {
        registry::set_service_state(&state.db, service.id, ServiceState::Pending, None)
            .await
            .unwrap();
        let observation = parsed(
            report(&service.unit_name, active),
            &tenant.slug,
            &service.unit_name,
        )
        .unwrap();
        registry::record_observation(&state.db, &service, &observation)
            .await
            .unwrap();
        let saved = registry::service_by_id(&state.db, service.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.uid, Some(1001));
        assert_eq!(saved.observed_state.as_deref(), Some(active));
        assert!(saved.observed_at.is_some());
        assert_eq!(
            saved.state,
            if active == "active" {
                ServiceState::Running
            } else {
                ServiceState::Pending
            }
        );
    }
    let active = parsed(
        report(&service.unit_name, "active"),
        &tenant.slug,
        &service.unit_name,
    )
    .unwrap();
    // Suspended and stopped are operator/tenant intents and survive an active
    // report; a failed service that reports active is running again.
    for policy in [ServiceState::Suspended, ServiceState::Stopped] {
        registry::set_service_state(&state.db, service.id, policy, None)
            .await
            .unwrap();
        registry::record_observation(&state.db, &service, &active)
            .await
            .unwrap();
        assert_eq!(
            registry::service_by_id(&state.db, service.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            policy
        );
    }
    registry::set_service_state(&state.db, service.id, ServiceState::Failed, None)
        .await
        .unwrap();
    registry::record_observation(&state.db, &service, &active)
        .await
        .unwrap();
    assert_eq!(
        registry::service_by_id(&state.db, service.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        ServiceState::Running
    );
    let wrong_unit = parsed(report("t02-web", "inactive"), "t02", "t02-web").unwrap();
    assert!(
        registry::record_observation(&state.db, &service, &wrong_unit)
            .await
            .is_err()
    );
    assert_eq!(
        registry::service_by_id(&state.db, service.id)
            .await
            .unwrap()
            .unwrap()
            .observed_state
            .as_deref(),
        Some("active")
    );
    registry::set_service_state(&state.db, service.id, ServiceState::Deleted, None)
        .await
        .unwrap();
    assert!(registry::record_observation(&state.db, &service, &active)
        .await
        .is_err());
    // The tombstone is renamed on deletion; reload it before clearing.
    let deleted = registry::service_by_id(&state.db, service.id)
        .await
        .unwrap()
        .unwrap();
    registry::clear_service_observation(&state.db, &deleted)
        .await
        .unwrap();
    let deleted = registry::service_by_id(&state.db, service.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(deleted.state, ServiceState::Deleted);
    assert!(deleted.observed_state.is_none() && deleted.observed_at.is_none());
}

#[tokio::test]
async fn legacy_and_stale_observations_are_not_displayed_as_current_active() {
    let state = state().await;
    let (_, tenant) = tenant(&state).await;
    let service = reserve(&state, &tenant).await;
    let mut view = registry::service_view(&state.db, service.id)
        .await
        .unwrap()
        .unwrap();
    view.observed_state = Some("running".to_string());
    assert!(crate::web::layout::observed_text(&view).starts_with("desconocido"));
    let now = time::OffsetDateTime::now_utc();
    view.observed_state = Some("active".to_string());
    view.observed_at = Some(now);
    assert!(crate::web::layout::observation_is_active(
        &view,
        now,
        Duration::from_secs(60)
    ));
    view.observed_at = Some(now - time::Duration::minutes(3));
    assert!(!crate::web::layout::observation_is_active(
        &view,
        now,
        Duration::from_secs(60)
    ));
    view.observed_at = Some(now + time::Duration::minutes(1));
    assert!(!crate::web::layout::observation_is_active(
        &view,
        now,
        Duration::from_secs(60)
    ));
}

// Binding Unix sockets needs the same sandbox exception as the existing broker framing test.
struct SocketFile(PathBuf);
impl Drop for SocketFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn scripted_broker(
    state: &mut crate::state::AppState,
    unit: &'static str,
    replies: Vec<(&'static str, Value)>,
) -> (SocketFile, tokio::task::JoinHandle<Vec<Value>>) {
    let path = std::env::temp_dir().join(format!(
        "hone-observe-{}.sock",
        auth::generate_token().unwrap()
    ));
    let listener = UnixListener::bind(&path).expect("fake broker requires Unix-socket permission");
    state.broker = broker::Client::new(path.clone());
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (op, mut answer) in replies {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut line = String::new();
            BufReader::new(reader).read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["op"], op);
            assert_eq!(request["tenant_id"], "t01");
            if op != "tenant.create" && op != "catalog.list" {
                assert_eq!(request["service"], unit);
            }
            assert!(request["request_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty() && id.len() <= 64));
            assert!(request["actor"]["id"].is_string());
            answer["request_id"] = request["request_id"].clone();
            requests.push(request);
            let mut payload = serde_json::to_vec(&answer).unwrap();
            payload.push(b'\n');
            writer.write_all(&payload).await.unwrap();
        }
        requests
    });
    (SocketFile(path), task)
}

#[tokio::test]
async fn provisioning_reads_status_and_background_observation_only_polls() {
    let mut state = state().await;
    let (_, tenant) = tenant(&state).await;
    let operator = auth::create_account(&state.db, Role::Operator, "operator-password", None)
        .await
        .unwrap();
    let session = auth::create_session(&state.db, operator.id, Duration::from_secs(60), None, None)
        .await
        .unwrap();
    // App-side display metadata must not become privileged broker limits.
    sqlx::query(
        "UPDATE plans SET cpu_weight=1000000, memory_max='infinity', tasks_max=0 WHERE code='pro'",
    )
    .execute(&state.db)
    .await
    .unwrap();
    let (_socket, server) = scripted_broker(
        &mut state,
        "t01-site",
        vec![
            ("catalog.list", json!({"ok":true,"op":"catalog.list","catalog":[{
                "plan_code":"pro","memory_high":"256M","memory_max":"512M",
                "cpu_quota_percent":75,"tasks_max":128,"max_services":4,"service_days":30,
                "allowed_kinds":["web"],"images":{"web":"approved.example/nginx:1"}
            }]})),
            ("tenant.create", json!({"ok":true,"op":"tenant.create"})),
            ("limits.set", json!({"ok":true,"op":"limits.set"})),
            ("service.start", json!({"ok":true,"op":"service.start"})),
            ("service.status", report("t01-site", "activating")),
            ("service.status", report("t01-site", "active")),
        ],
    );
    let response = web::router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/operator/tenants/{}/services", tenant.id))
                .header(
                    header::COOKIE,
                    format!("__Host-hone_session={}", session.id),
                )
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "csrf={}&kind=web&plan_code=pro",
                    session.csrf_token
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let view = registry::services_of_tenant(&state.db, tenant.id)
        .await
        .unwrap()
        .remove(0);
    let saved = registry::service_by_id(&state.db, view.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.state, ServiceState::Pending);
    assert_eq!(saved.uid, Some(1001));
    assert_eq!(saved.observed_state.as_deref(), Some("activating"));
    observation::reconcile(&state).await.unwrap();
    let saved = registry::service_by_id(&state.db, view.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.state, ServiceState::Running);
    assert_eq!(saved.observed_state.as_deref(), Some("active"));
    let requests = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap();
    let correlation_id = &requests[0]["request_id"];
    let actor = json!({"kind":"operator", "id":operator.number});
    for request in &requests[..5] {
        assert_eq!(&request["request_id"], correlation_id);
        assert_eq!(request["actor"], actor);
    }
    let mut limits = requests[2].clone();
    limits.as_object_mut().unwrap().remove("request_id");
    limits.as_object_mut().unwrap().remove("actor");
    assert_eq!(
        limits,
        json!({"op":"limits.set","tenant_id":"t01","service":"t01-site","kind":"web","plan_code":"pro","port":18001})
    );
    assert_eq!(requests[5]["actor"]["kind"], "system");
    assert_ne!(&requests[5]["request_id"], correlation_id);
    let job = registry::recent_jobs(&state.db, 1).await.unwrap().remove(0);
    let payload: Value = serde_json::from_str(&job.payload_json).unwrap();
    assert_eq!(&payload["correlation"]["request_id"], correlation_id);
    assert_eq!(payload["correlation"]["actor"], actor);
    assert_eq!(job.requested_by, Some(operator.id));
    let event = registry::recent_audit(&state.db, 1)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(event.action, "service.provision");
    assert_eq!(event.request_id, correlation_id.as_str().unwrap());
    assert_eq!(requests.len(), 6);
}

#[tokio::test]
async fn tenant_correlation_uses_principal_not_headers_and_matches_job_and_audit() {
    let mut state = state().await;
    let (account, tenant) = tenant(&state).await;
    let service = reserve(&state, &tenant).await;
    let active = parsed(
        report(&service.unit_name, "active"),
        &tenant.slug,
        &service.unit_name,
    )
    .unwrap();
    registry::record_observation(&state.db, &service, &active)
        .await
        .unwrap();
    let session = auth::create_session(&state.db, account.id, Duration::from_secs(60), None, None)
        .await
        .unwrap();
    let principal = crate::models::Principal {
        account: account.clone(),
        session: session.clone(),
        tenant: Some(tenant),
    };
    let unavailable = crate::audit::Context {
        request_id: "unavailable".to_string(),
        src_ip: None,
        user_agent: None,
    };
    assert!(unavailable.broker_context(&principal).is_err());
    let (_socket, server) = scripted_broker(
        &mut state,
        "t01-web",
        vec![
            ("service.stop", json!({"ok":true,"op":"service.stop"})),
            ("service.status", report("t01-web", "inactive")),
        ],
    );
    let response = web::router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/portal/services/{}/stop", service.id))
                .header(
                    header::COOKIE,
                    format!("__Host-hone_session={}", session.id),
                )
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header("x-request-id", "spoofed-id")
                .header("x-actor-kind", "operator")
                .header("x-actor-id", "admin")
                .body(Body::from(format!("csrf={}", session.csrf_token)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let requests = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap();
    let actor = json!({"kind":"tenant", "id":account.number});
    assert_ne!(requests[0]["request_id"], "spoofed-id");
    for request in &requests {
        assert_eq!(request["actor"], actor);
        assert_eq!(request["request_id"], requests[0]["request_id"]);
    }
    let job = registry::recent_jobs(&state.db, 1).await.unwrap().remove(0);
    let payload: Value = serde_json::from_str(&job.payload_json).unwrap();
    assert_eq!(
        payload["correlation"]["request_id"],
        requests[0]["request_id"]
    );
    assert_eq!(payload["correlation"]["actor"], actor);
    assert_eq!(job.requested_by, Some(account.id));
    let event = registry::recent_audit(&state.db, 1)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(event.action, "service.stop");
    assert_eq!(
        event.request_id,
        requests[0]["request_id"].as_str().unwrap()
    );
}

#[tokio::test]
async fn failed_command_preserves_policy_but_invalidates_old_observation() {
    let mut state = state().await;
    let (_, tenant) = tenant(&state).await;
    let service = reserve(&state, &tenant).await;
    let active = parsed(
        report(&service.unit_name, "active"),
        &tenant.slug,
        &service.unit_name,
    )
    .unwrap();
    registry::record_observation(&state.db, &service, &active)
        .await
        .unwrap();
    let (_socket, server) = scripted_broker(
        &mut state,
        "t01-web",
        vec![(
            "service.stop",
            json!({"ok":false,"op":"service.stop","error":"timeout"}),
        )],
    );
    let _guard = state.mutations.lock().await;
    assert!(observation::execute(
        &state,
        &service,
        &tenant.slug,
        &broker::Request::ServiceStop {
            tenant_id: tenant.slug.clone(),
            service: service.unit_name.clone()
        },
        ServiceState::Stopped,
        None
    )
    .await
    .is_err());
    let saved = registry::service_by_id(&state.db, service.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.state, ServiceState::Running);
    assert!(saved.observed_state.is_none() && saved.observed_at.is_none());
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn failed_readback_is_unknown_and_periodic_poll_never_unsuspends() {
    let mut state = state().await;
    let (_, tenant) = tenant(&state).await;
    let service = reserve(&state, &tenant).await;
    let (_socket, server) = scripted_broker(
        &mut state,
        "t01-web",
        vec![
            ("service.start", json!({"ok":true,"op":"service.start"})),
            (
                "service.status",
                json!({"ok":false,"op":"service.status","error":"unavailable"}),
            ),
            ("service.status", report("t01-web", "active")),
        ],
    );
    let _guard = state.mutations.lock().await;
    let result = observation::execute(
        &state,
        &service,
        &tenant.slug,
        &broker::Request::ServiceStart {
            tenant_id: tenant.slug.clone(),
            service: service.unit_name.clone(),
        },
        ServiceState::Running,
        None,
    )
    .await;
    assert!(result.is_err());
    let saved = registry::service_by_id(&state.db, service.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.state, ServiceState::Pending);
    assert!(saved.observed_state.is_none() && saved.observed_at.is_none());
    registry::set_service_state(&state.db, service.id, ServiceState::Suspended, None)
        .await
        .unwrap();
    drop(_guard);
    observation::reconcile(&state).await.unwrap();
    let saved = registry::service_by_id(&state.db, service.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.state, ServiceState::Suspended);
    assert_eq!(saved.observed_state.as_deref(), Some("active"));
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap();
    // Listener is gone: a transport failure must invalidate the old "active" report.
    observation::reconcile(&state).await.unwrap();
    let saved = registry::service_by_id(&state.db, service.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.state, ServiceState::Suspended);
    assert!(saved.observed_state.is_none() && saved.observed_at.is_none());
}
