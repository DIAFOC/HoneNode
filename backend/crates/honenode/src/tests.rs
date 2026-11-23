use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{header, Request, StatusCode};
use sqlx::sqlite::SqlitePoolOptions;
use time::OffsetDateTime;
use tower::ServiceExt;

use crate::{auth, config, models::*, registry, state::AppState, topup, web};
use crate::web::middleware;

pub(super) async fn state() -> AppState {
    let db = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    registry::migrate(&db).await.unwrap();
    let config = config::Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        broker_socket: "/nonexistent/honenode-test.sock".into(),
        registry_path: ":memory:".into(),
        tenant_data_root: "/srv/test".into(),
        tenants: vec!["t01".into()],
        panel_domain: "panel.example.test".into(),
        tenant_domain: "content.example.org".into(),
        service_access: config::ServiceAccess::Public,
        caddy_admin: "127.0.0.1:2019".into(),
        static_dir: "static".into(),
        log_format: config::LogFormat::Plain,
        theme: config::Theme::default(),
        operator_account: None,
        operator_password: None,
        session_ttl: Duration::from_secs(3600),
        secure_cookies: true,
        api_tokens: vec!["test-secret".into()],
        grace_days: 7,
        retention_days: 30,
        trial_days: 7,
        bootstrap_file: None,
        sweep_interval: Duration::from_secs(300),
        sample_interval: Duration::from_secs(60),
    };
    AppState::new(config, db)
}

pub(super) async fn tenant(state: &AppState) -> (Account, Tenant) {
    let account = auth::create_account(&state.db, Role::Tenant, "test-password", None)
        .await
        .unwrap();
    let tenant = registry::create_tenant(&state.db, account.id, "t01", "Test")
        .await
        .unwrap();
    (account, tenant)
}

pub(super) async fn reserve(state: &AppState, tenant: &Tenant) -> Service {
    let plan = registry::plan_by_code(&state.db, "pro")
        .await
        .unwrap()
        .unwrap();
    registry::create_service(
        &state.db,
        tenant.id,
        ServiceKind::Web,
        plan.id,
        &format!("{}-web", tenant.slug),
        None,
        "{}",
        "{}",
        OffsetDateTime::now_utc() + time::Duration::days(30),
    )
    .await
    .unwrap()
}

fn service_view(slug: &str, kind: ServiceKind) -> ServiceView {
    ServiceView {
        id: 1,
        tenant_id: 1,
        tenant_slug: slug.to_string(),
        tenant_name: "Test".to_string(),
        kind,
        plan_id: 1,
        plan_code: "pro".to_string(),
        state: ServiceState::Running,
        observed_state: None,
        observed_at: None,
        unit_name: format!("{slug}-web"),
        ports_json: match kind {
            ServiceKind::Database => serde_json::json!({"database": 15_521}).to_string(),
            ServiceKind::Storage => serde_json::json!({"web": 19_091}).to_string(),
            ServiceKind::Git => serde_json::json!({"web": 20_091, "git_ssh": 22_091}).to_string(),
            ServiceKind::Web => serde_json::json!({"web": 18_091}).to_string(),
            ServiceKind::Container => serde_json::json!({"web": 30_000}).to_string(),
        },
        created_at: OffsetDateTime::now_utc(),
        expires_at: None,
    }
}

#[tokio::test]
async fn sensitive_api_requires_token_and_never_caches() {
    let state = state().await;
    let app = web::router(state);
    for uri in [
        "/api/v1/status",
        "/api/v1/broker",
        "/api/v1/tenants",
        "/api/v1/audit",
    ] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/status")
                .header(header::AUTHORIZATION, "Bearer test-secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn login_form_has_bound_one_use_challenge() {
    let state = state().await;
    let app = web::router(state.clone());
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    assert!(cookie.starts_with("__Host-hone_login="));
    let token = cookie.split_once('=').unwrap().1;
    let html = String::from_utf8(
        to_bytes(response.into_body(), 100_000)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(html.contains(&format!("name=\"csrf\" value=\"{token}\"")));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "number=000000000000&password=wrong&csrf={token}"
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "a token without its cookie is not sufficient"
    );
    auth::consume_login_challenge(&state.db, token)
        .await
        .unwrap();
    assert!(auth::consume_login_challenge(&state.db, token)
        .await
        .is_err());
}

#[tokio::test]
async fn login_throttling_and_password_reset_revoke_sessions() {
    let state = state().await;
    let (account, _) = tenant(&state).await;
    for _ in 0..10 {
        auth::limit_login(&state.db, &account.number).await.unwrap();
    }
    assert!(matches!(
        auth::limit_login(&state.db, &account.number).await,
        Err(crate::error::AppError::RateLimited)
    ));
    let session = auth::create_session(&state.db, account.id, Duration::from_secs(60), None, None)
        .await
        .unwrap();
    auth::set_password(&state.db, account.id, "new-password")
        .await
        .unwrap();
    assert!(auth::load_session(&state.db, &session.id)
        .await
        .unwrap()
        .is_none());
    assert!(auth::authenticate(
        &state.db,
        &account.number,
        "new-password",
        state.login_kdf.clone().acquire_owned().await.unwrap()
    )
    .await
    .unwrap()
    .is_some());
    assert!(!serde_json::to_string(&account)
        .unwrap()
        .contains("password_hash"));
}

#[tokio::test]
async fn reservations_are_atomic_and_extensions_additive() {
    let state = state().await;
    let (_, tenant) = tenant(&state).await;
    let service = reserve(&state, &tenant).await;
    let plan = registry::plan_by_code(&state.db, "pro")
        .await
        .unwrap()
        .unwrap();
    let second_kind = registry::create_service(
        &state.db,
        tenant.id,
        ServiceKind::Git,
        plan.id,
        "t01-git",
        None,
        "{}",
        "{}",
        OffsetDateTime::now_utc(),
    )
    .await
    .unwrap();
    assert_eq!(second_kind.kind, ServiceKind::Git);
    assert_eq!(registry::count_services(&state.db, tenant.id).await.unwrap(), 2);
    // A second web instance is allowed; the same unit name is still refused.
    let second_web = registry::create_service(
        &state.db,
        tenant.id,
        ServiceKind::Web,
        plan.id,
        "t01-another-web",
        None,
        "{}",
        "{}",
        OffsetDateTime::now_utc(),
    )
    .await
    .unwrap();
    assert_eq!(second_web.kind, ServiceKind::Web);
    let duplicate = registry::create_service(
        &state.db,
        tenant.id,
        ServiceKind::Web,
        plan.id,
        "t01-another-web",
        None,
        "{}",
        "{}",
        OffsetDateTime::now_utc(),
    )
    .await;
    assert!(matches!(
        duplicate,
        Err(crate::error::AppError::Conflict(_))
    ));
    let (a, b) = tokio::join!(
        registry::extend_service(&state.db, service.id, 5),
        registry::extend_service(&state.db, service.id, 7)
    );
    a.unwrap();
    b.unwrap();
    let extended = registry::service_by_id(&state.db, service.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        extended.expires_at.unwrap(),
        service.expires_at.unwrap() + time::Duration::days(12)
    );
    registry::set_service_state(&state.db, service.id, ServiceState::Deleted, None)
        .await
        .unwrap();
    assert!(registry::extend_service(&state.db, service.id, 1)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn tenant_cannot_bypass_suspension_or_expiry() {
    let state = state().await;
    let (account, mut tenant) = tenant(&state).await;
    let mut service = reserve(&state, &tenant).await;
    let now = OffsetDateTime::now_utc();
    service.state = ServiceState::Running;
    assert!(service.tenant_actions_allowed(&tenant, 7, now));
    tenant.status = TenantStatus::Suspended;
    assert!(!service.tenant_actions_allowed(&tenant, 7, now));
    tenant.status = TenantStatus::Active;
    service.expires_at = Some(now - time::Duration::days(8));
    assert!(!service.tenant_actions_allowed(&tenant, 7, now));
    registry::set_service_state(&state.db, service.id, ServiceState::Suspended, None)
        .await
        .unwrap();
    let session = auth::create_session(&state.db, account.id, Duration::from_secs(60), None, None)
        .await
        .unwrap();
    let app = web::router(state.clone());
    for action in ["start", "restart", "stop"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/portal/services/{}/{action}", service.id))
                    .header(
                        header::COOKIE,
                        format!("__Host-hone_session={}", session.id),
                    )
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!("csrf={}", session.csrf_token)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    let mut principal = Principal {
        account,
        session,
        tenant: Some(tenant),
    };
    principal.account.role = Role::Support;
    assert!(principal.require_tenant().is_err());
}

#[tokio::test]
async fn csrf_and_ownership_are_checked_before_broker_and_failures_preserve_state() {
    let state = state().await;
    let (account, tenant) = tenant(&state).await;
    let service = reserve(&state, &tenant).await;
    registry::set_service_state(&state.db, service.id, ServiceState::Running, None)
        .await
        .unwrap();
    let session = auth::create_session(&state.db, account.id, Duration::from_secs(60), None, None)
        .await
        .unwrap();
    let app = web::router(state.clone());
    for (csrf, expected) in [
        ("wrong", StatusCode::FORBIDDEN),
        (session.csrf_token.as_str(), StatusCode::BAD_GATEWAY),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/portal/services/{}/restart", service.id))
                    .header(
                        header::COOKIE,
                        format!("__Host-hone_session={}", session.id),
                    )
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!("csrf={csrf}")))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        let saved = registry::service_by_id(&state.db, service.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.state, ServiceState::Running);
        assert!(saved.observed_state.is_none());
    }
    let other_account = auth::create_account(&state.db, Role::Tenant, "other-password", None)
        .await
        .unwrap();
    let other_tenant = registry::create_tenant(&state.db, other_account.id, "t02", "Other")
        .await
        .unwrap();
    let other_service = reserve(&state, &other_tenant).await;
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/portal/services/{}/restart", other_service.id))
                .header(
                    header::COOKIE,
                    format!("__Host-hone_session={}", session.id),
                )
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!("csrf={}", session.csrf_token)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn operator_actions_do_not_require_a_billing_days_field() {
    let state = state().await;
    let (_, tenant) = tenant(&state).await;
    let service = reserve(&state, &tenant).await;
    let account = auth::create_account(&state.db, Role::Operator, "operator-password", None)
        .await
        .unwrap();
    let session = auth::create_session(&state.db, account.id, Duration::from_secs(60), None, None)
        .await
        .unwrap();
    let response = web::router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/operator/services/{}/restart", service.id))
                .header(
                    header::COOKIE,
                    format!("__Host-hone_session={}", session.id),
                )
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!("csrf={}", session.csrf_token)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::BAD_GATEWAY,
        "form accepted; test broker intentionally absent"
    );
}

#[tokio::test]
async fn successful_login_consumes_challenge_and_disabled_accounts_lose_access() {
    let state = state().await;
    let (account, _) = tenant(&state).await;
    let app = web::router(state.clone());
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let challenge_cookie = response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let csrf = challenge_cookie.split_once('=').unwrap().1;
    let login_request = || {
        Request::builder()
            .method("POST")
            .uri("/login")
            .header(header::COOKIE, challenge_cookie.as_str())
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!(
                "number={}&password=test-password&csrf={csrf}",
                account.number
            )))
            .unwrap()
    };
    let response = app.clone().oneshot(login_request()).await.unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], "/portal");
    let session_cookie = response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    assert!(session_cookie.starts_with("__Host-hone_session="));
    let replay = app.clone().oneshot(login_request()).await.unwrap();
    assert_eq!(replay.status(), StatusCode::FORBIDDEN);
    let portal_request = || {
        Request::builder()
            .uri("/portal")
            .header(header::COOKIE, session_cookie.as_str())
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        app.clone()
            .oneshot(portal_request())
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    sqlx::query("UPDATE accounts SET disabled_at = ? WHERE id = ?")
        .bind(OffsetDateTime::now_utc())
        .bind(account.id)
        .execute(&state.db)
        .await
        .unwrap();
    assert_eq!(
        app.oneshot(portal_request()).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn both_themes_render_login_and_authenticated_surfaces() {
    let state = state().await;
    let (account, tenant) = tenant(&state).await;
    let service = reserve(&state, &tenant).await;
    let operator = auth::create_account(&state.db, Role::Operator, "operator-password", None)
        .await
        .unwrap();
    let tenant_session =
        auth::create_session(&state.db, account.id, Duration::from_secs(60), None, None)
            .await
            .unwrap();
    let operator_session =
        auth::create_session(&state.db, operator.id, Duration::from_secs(60), None, None)
            .await
            .unwrap();
    let routes = [
        ("/login".to_string(), None),
        ("/portal".to_string(), Some(&tenant_session)),
        (
            format!("/portal/services/{}", service.id),
            Some(&tenant_session),
        ),
        ("/portal/audit".to_string(), Some(&tenant_session)),
        ("/operator".to_string(), Some(&operator_session)),
        ("/operator/tenants".to_string(), Some(&operator_session)),
        (
            format!("/operator/tenants/{}", tenant.id),
            Some(&operator_session),
        ),
        ("/operator/services".to_string(), Some(&operator_session)),
        (
            format!("/operator/services/{}", service.id),
            Some(&operator_session),
        ),
        ("/operator/catalog".to_string(), Some(&operator_session)),
        ("/operator/jobs".to_string(), Some(&operator_session)),
        ("/operator/audit".to_string(), Some(&operator_session)),
    ];
    for theme in [config::Theme::Neutral, config::Theme::SharpNode] {
        let mut themed = state.clone();
        std::sync::Arc::make_mut(&mut themed.config).theme = theme;
        let app = web::router(themed);
        for (uri, session) in &routes {
            let mut request = Request::builder().uri(uri.as_str());
            if let Some(session) = session {
                request = request.header(
                    header::COOKIE,
                    format!("__Host-hone_session={}", session.id),
                );
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{theme:?} {uri}");
            let html = String::from_utf8(
                to_bytes(response.into_body(), 100_000)
                    .await
                    .unwrap()
                    .to_vec(),
            )
            .unwrap();
            let expected_theme = if uri.starts_with("/portal") {
                theme
            } else {
                config::Theme::Neutral
            };
            assert!(
                html.contains(&format!(
                    "<html lang=\"es\" class=\"{}\">",
                    expected_theme.css_class()
                )),
                "{uri}"
            );
            assert_eq!(
                html.contains("SharpNode"),
                expected_theme == config::Theme::SharpNode,
                "{uri}"
            );
            assert!(html.contains("<script src=\"/static/hone.js\" defer></script>"));
            assert!(!html.replace("<script src=\"/static/hone.js\" defer></script>", "").contains("<script"));
        }
    }
}

#[tokio::test]
async fn missing_catalog_and_local_plans_disable_provisioning_and_render_empty_state() {
    let state = state().await;
    let (_, tenant) = tenant(&state).await;
    let operator = auth::create_account(&state.db, Role::Operator, "operator-password", None)
        .await
        .unwrap();
    let session = auth::create_session(&state.db, operator.id, Duration::from_secs(60), None, None)
        .await
        .unwrap();
    sqlx::query("DELETE FROM plans")
        .execute(&state.db)
        .await
        .unwrap();
    let app = web::router(state);
    for uri in [
        format!("/operator/tenants/{}", tenant.id),
        "/operator/catalog".to_string(),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri.as_str())
                    .header(
                        header::COOKIE,
                        format!("__Host-hone_session={}", session.id),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let html = String::from_utf8(
            to_bytes(response.into_body(), 100_000)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        if uri.starts_with("/operator/tenants/") {
            assert!(html.contains("No hay planes autorizados disponibles para este inquilino."));
        } else {
            assert!(html.contains("No hay planes disponibles."));
        }
        assert!(!html.contains("name=\"plan_code\""));
        assert!(!html.contains("name=\"plan\""));
        assert!(!html.contains("name=\"days\""));
    }
}

#[tokio::test]
async fn loopback_service_access_uses_fixed_broker_ports_and_rejects_bad_slugs() {
    let mut state = state().await;
    std::sync::Arc::make_mut(&mut state.config).service_access = config::ServiceAccess::Loopback;

    for (kind, expected) in [
        (ServiceKind::Web, "http://127.0.0.1:18091/"),
        (ServiceKind::Storage, "http://127.0.0.1:19091/"),
        (ServiceKind::Git, "http://127.0.0.1:20091/"),
    ] {
        let service = service_view("t91", kind);
        assert_eq!(
            web::layout::public_url(&state.config, &service).as_deref(),
            Some(expected)
        );
        let rows = web::layout::service_rows(
            std::slice::from_ref(&service),
            OffsetDateTime::now_utc(),
            &state.config,
        );
        assert!(rows[0].has_public_url);
        assert_eq!(rows[0].public_url, expected);
    }

    let database = service_view("t91", ServiceKind::Database);
    assert_eq!(web::layout::public_url(&state.config, &database), None);
    let rows = web::layout::service_rows(&[database], OffsetDateTime::now_utc(), &state.config);
    assert!(!rows[0].has_public_url);
    assert!(rows[0].public_url.is_empty());

    for slug in ["t1", "t001", "t9x", "T91", "t91.example.test"] {
        let mut service = service_view(slug, ServiceKind::Web);
        // The URL now comes from the persisted allocation; without one there
        // is nothing safe to publish for an unexpected slug either.
        service.ports_json = "{}".to_string();
        assert_eq!(web::layout::public_url(&state.config, &service), None);
        let rows = web::layout::service_rows(&[service], OffsetDateTime::now_utc(), &state.config);
        assert!(!rows[0].has_public_url);
        assert!(!rows[0].public_url.contains("localhost"));
        assert!(!rows[0].public_url.contains("127.0.0.1"));
    }
}

#[tokio::test]
async fn public_service_access_preserves_tenant_domain_urls() {
    let state = state().await;
    assert_eq!(state.config.service_access, config::ServiceAccess::Public);
    let service = service_view("t91", ServiceKind::Web);
    assert_eq!(
        web::layout::public_url(&state.config, &service).as_deref(),
        Some("https://t91.content.example.org/")
    );
}

#[test]
fn loopback_service_access_requires_a_loopback_bind_ip() {
    let access = config::ServiceAccess::parse("loopback").unwrap();
    assert!(access
        .validate_bind("127.0.0.1:18091".parse().unwrap())
        .is_ok());
    assert!(access.validate_bind("[::1]:18091".parse().unwrap()).is_ok());
    for bind in ["0.0.0.0:18091", "192.0.2.10:18091", "[::]:18091"] {
        assert!(
            access.validate_bind(bind.parse().unwrap()).is_err(),
            "{bind}"
        );
    }
    assert_eq!(
        config::ServiceAccess::default(),
        config::ServiceAccess::Public
    );
    assert!(config::ServiceAccess::parse("invalid").is_err());
}

#[test]
fn theme_configuration_is_explicit_and_sharpnode_by_default() {
    assert_eq!(config::Theme::default(), config::Theme::SharpNode);
    assert_eq!(
        config::Theme::parse("neutral").unwrap(),
        config::Theme::Neutral
    );
    assert_eq!(
        config::Theme::parse("sharpnode").unwrap(),
        config::Theme::SharpNode
    );
    assert!(config::Theme::parse("unknown").is_err());
    assert!(config::Theme::parse("<script>").is_err());
}

#[test]
fn cookie_parser_rejects_duplicates_and_prefix_confusion() {
    let token = "a".repeat(64);
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        header::COOKIE,
        format!("hone_session_extra={token}; hone_session={token}")
            .parse()
            .unwrap(),
    );
    assert_eq!(
        web::middleware::session_token(&headers, false),
        Some(token.clone())
    );
    assert!(web::middleware::session_token(&headers, true).is_none());
    headers.append(
        header::COOKIE,
        format!("hone_session={token}").parse().unwrap(),
    );
    assert!(web::middleware::session_token(&headers, false).is_none());
    let cookie = web::middleware::session_cookie(&token, Duration::from_secs(60), true);
    assert!(cookie.starts_with("__Host-"));
    assert!(
        cookie.contains("HttpOnly") && cookie.contains("Secure") && !cookie.contains("Domain=")
    );
}

async fn form_challenge(app: &axum::Router) -> (String, String) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/signup")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let token = cookie.split_once('=').unwrap().1.to_string();
    let html = String::from_utf8(
        to_bytes(response.into_body(), 100_000)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(html.contains(&format!("name=\"csrf\" value=\"{token}\"")));
    (cookie, token)
}

#[tokio::test]
async fn signup_issues_one_time_credentials_and_a_trial_subscription() {
    let state = state().await;
    let app = web::router(state.clone());
    let (cookie, token) = form_challenge(&app).await;

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/signup")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, &cookie)
                .body(Body::from(format!("name=Velas+SA&csrf={token}")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = String::from_utf8(
        to_bytes(response.into_body(), 200_000)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(html.contains("una sola vez"), "credentials page shown once");

    // The credentials page carries the generated number and password.
    let number = html
        .split("Número de cuenta</dt><dd class=\"mono\">")
        .nth(1)
        .and_then(|rest| rest.split('<').next())
        .unwrap_or("")
        .trim()
        .to_string();
    assert!(auth::account_number_is_valid(&number), "number: {number}");
    let account = sqlx::query_as::<_, Account>(
        "SELECT id, number, role, password_hash, created_at, last_login_at, disabled_at, notes
         FROM accounts WHERE number = ?",
    )
    .bind(&number)
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(account.role, Role::Tenant);

    // The tenant row and the trial subscription exist, paid through in ~7 days.
    let tenant = registry::tenant_by_account(&state.db, account.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tenant.slug, "t01");
    let paid_through: Option<OffsetDateTime> =
        sqlx::query_scalar("SELECT paid_through FROM subscriptions WHERE tenant_id = ?")
            .bind(tenant.id)
            .fetch_one(&state.db)
            .await
            .unwrap();
    let paid_through = paid_through.expect("trial subscription");
    let days = (paid_through - OffsetDateTime::now_utc()).whole_days();
    assert!((6..=7).contains(&days), "trial is ~7 days, got {days}");

    // The generated credentials authenticate, and the challenge is one-time.
    let permit = state.login_kdf.clone().acquire_owned().await.unwrap();
    assert!(auth::authenticate(&state.db, &number, "ignored", permit)
        .await
        .unwrap()
        .is_none());
    assert!(auth::consume_login_challenge(&state.db, &token).await.is_err());
}

#[tokio::test]
async fn signup_refuses_when_the_node_allowlist_is_exhausted() {
    let state = state().await;
    // The only allowlisted slot (t01) is already taken.
    let (_, _) = tenant(&state).await;
    let app = web::router(state.clone());
    let (cookie, token) = form_challenge(&app).await;
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/signup")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, &cookie)
                .body(Body::from(format!("name=X&csrf={token}")))
                .unwrap(),
        )
        .await
        .unwrap();
    let html = String::from_utf8(
        to_bytes(response.into_body(), 200_000)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(html.contains("No quedan espacios"));
    // No account was created for the refused signup.
    let accounts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accounts")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(accounts, 1);
}

#[tokio::test]
async fn topup_keys_redeem_additively_and_refuse_reuse() {
    let state = state().await;
    let codes = topup::generate(&state.db, "pro", 30, 3, None)
        .await
        .unwrap();
    assert_eq!(codes.len(), 3);

    let (_, tenant) = tenant(&state).await;
    let service = reserve(&state, &tenant).await;
    let original_expiry = service.expires_at.unwrap();

    let redemption = topup::redeem(&state.db, tenant.id, &codes[0])
        .await
        .unwrap();
    assert_eq!(redemption.plan_code, "pro");
    assert_eq!(redemption.days, 30);

    // The service expiry moved additively from its own base.
    let expires: Option<OffsetDateTime> =
        sqlx::query_scalar("SELECT expires_at FROM services WHERE id = ?")
            .bind(service.id)
            .fetch_one(&state.db)
            .await
            .unwrap();
    let expected = original_expiry.max(OffsetDateTime::now_utc()) + time::Duration::days(30);
    assert_eq!(expires.unwrap(), expected);

    // The subscription moved to the key's plan with additive paid_through.
    let plan = topup::subscription_plan_code(&state.db, tenant.id)
        .await
        .unwrap();
    assert_eq!(plan.as_deref(), Some("pro"));
    let paid_through = topup::subscription_paid_through(&state.db, tenant.id)
        .await
        .unwrap()
        .unwrap();
    assert!(paid_through > OffsetDateTime::now_utc());

    // Reuse, garbage and unknown codes are refused.
    assert!(matches!(
        topup::redeem(&state.db, tenant.id, &codes[0]).await,
        Err(crate::error::AppError::Conflict(_))
    ));
    assert!(matches!(
        topup::redeem(&state.db, tenant.id, "hn-abcd-efgh-jkmn").await,
        Err(crate::error::AppError::BadRequest(_))
    ));
    assert!(matches!(
        topup::redeem(&state.db, tenant.id, "HN-AAAA-BBBB-CCCC-DDDD").await,
        Err(crate::error::AppError::NotFound)
    ));

    // The operator listing never exposes a full code.
    let rows = topup::list(&state.db, 10).await.unwrap();
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|row| row.code_prefix.len() == 4));
    for code in &codes {
        let normalized = topup::normalize(code).unwrap();
        assert!(!rows.iter().any(|row| row.code_prefix == *code), "full code leaked");
        assert!(rows
            .iter()
            .any(|row| row.code_prefix == normalized[..4]));
    }
}

#[tokio::test]
async fn subscription_caps_provisioning_time_without_binding_the_plan() {
    let state = state().await;
    let (_, tenant) = tenant(&state).await;

    // Without a subscription nothing changes and nothing is restricted.
    let expires = OffsetDateTime::now_utc() + time::Duration::days(30);
    assert_eq!(
        crate::provisioning::subscription_cap(&state.db, tenant.id, expires)
            .await
            .unwrap(),
        expires
    );

    // A future subscription caps the expiry at paid_through. The plan code is
    // display/bookkeeping only: any entitled plan may be used for any kind.
    let plan_id = registry::ensure_plan_row(&state.db, "starter").await.unwrap();
    let paid_through = OffsetDateTime::now_utc() + time::Duration::days(7);
    sqlx::query(
        "INSERT INTO subscriptions (tenant_id, plan_id, started_at, paid_through, auto_renew)
         VALUES (?, ?, ?, ?, 0)",
    )
    .bind(tenant.id)
    .bind(plan_id)
    .bind(OffsetDateTime::now_utc())
    .bind(paid_through)
    .execute(&state.db)
    .await
    .unwrap();
    let capped = crate::provisioning::subscription_cap(
        &state.db,
        tenant.id,
        OffsetDateTime::now_utc() + time::Duration::days(30),
    )
    .await
    .unwrap();
    assert!(capped <= paid_through);

    // An expired subscription refuses new services until a key is redeemed.
    sqlx::query("UPDATE subscriptions SET paid_through = ? WHERE tenant_id = ?")
        .bind(OffsetDateTime::now_utc() - time::Duration::days(1))
        .bind(tenant.id)
        .execute(&state.db)
        .await
        .unwrap();
    assert!(matches!(
        crate::provisioning::subscription_cap(
            &state.db,
            tenant.id,
            OffsetDateTime::now_utc() + time::Duration::days(30)
        )
        .await,
        Err(crate::error::AppError::BadRequest(_))
    ));
}

#[tokio::test]
async fn deleted_services_free_kind_and_reservations_for_reprovisioning() {
    let state = state().await;
    let (_, tenant) = tenant(&state).await;
    let service = reserve(&state, &tenant).await;
    let plan = registry::plan_by_code(&state.db, "pro").await.unwrap().unwrap();
    let expires = OffsetDateTime::now_utc() + time::Duration::days(30);

    // A second web instance is allowed; the unit name remains exclusive.
    let second = registry::create_service(
        &state.db, tenant.id, ServiceKind::Web, plan.id, "t01-web-2", None, "{}", "{}", expires
    )
    .await
    .unwrap();
    assert_eq!(second.kind, ServiceKind::Web);
    assert!(registry::create_service(
        &state.db, tenant.id, ServiceKind::Web, plan.id, "t01-web-2", None, "{}", "{}", expires
    )
    .await
    .is_err());

    // Deleting the service frees its kind, unit name and reservations.
    registry::set_service_state(&state.db, service.id, ServiceState::Deleted, None)
        .await
        .unwrap();
    let replacement = registry::create_service(
        &state.db, tenant.id, ServiceKind::Web, plan.id, "t01-web", None, "{}", "{}", expires
    )
    .await
    .unwrap();
    assert_eq!(replacement.unit_name, "t01-web");
    // The second web instance survives the first one's deletion.
    assert_eq!(registry::count_services(&state.db, tenant.id).await.unwrap(), 2);
}

#[tokio::test]
async fn operator_tenant_delete_requires_broker_removal_first() {
    let state = state().await;
    let (account, tenant) = tenant(&state).await;
    let service = reserve(&state, &tenant).await;
    let operator = auth::create_account(&state.db, Role::Operator, "test-password", None)
        .await
        .unwrap();
    let session = auth::create_session(&state.db, operator.id, Duration::from_secs(600), None, None)
        .await
        .unwrap();
    let app = web::router(state.clone());

    // The broker socket is absent: the service removal fails, so the tenant and
    // its account must survive.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/operator/tenants/{}/delete", tenant.id))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(
                    header::COOKIE,
                    middleware::session_cookie(&session.id, Duration::from_secs(600), true),
                )
                .body(Body::from(format!("csrf={}", session.csrf_token)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(registry::tenant_by_id(&state.db, tenant.id).await.unwrap().is_some());
    assert!(registry::service_by_id(&state.db, service.id).await.unwrap().is_some());

    // With no live services the deletion is local and removes the account row.
    registry::set_service_state(&state.db, service.id, ServiceState::Deleted, None)
        .await
        .unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/operator/tenants/{}/delete", tenant.id))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(
                    header::COOKIE,
                    middleware::session_cookie(&session.id, Duration::from_secs(600), true),
                )
                .body(Body::from(format!("csrf={}", session.csrf_token)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert!(registry::tenant_by_id(&state.db, tenant.id).await.unwrap().is_none());
    assert!(registry::account_by_id(&state.db, account.id).await.unwrap().is_none());
}

#[test]
fn bootstrap_credentials_file_is_written_once_with_private_mode() {
    let directory = std::env::temp_dir().join(format!(
        "honenode-bootstrap-{}",
        auth::generate_token().unwrap()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("bootstrap-credentials.txt");
    crate::write_bootstrap_file(&path, "123456789012", "test-password").unwrap();
    let metadata = std::fs::metadata(&path).unwrap();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("HONE_OPERATOR_ACCOUNT=123456789012"));
    assert!(body.contains("HONE_OPERATOR_PASSWORD=test-password"));
    // O_EXCL: a second write never overwrites the first credentials.
    assert!(crate::write_bootstrap_file(&path, "999999999999", "other").is_err());
    std::fs::remove_dir_all(&directory).unwrap();
}
