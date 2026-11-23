use askama::Template;
use axum::extract::State;
use axum::http::{header::SET_COOKIE, HeaderMap};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use serde::Deserialize;
use time::OffsetDateTime;

use crate::audit::{self, Event};
use crate::broker::{self, ActorKind, CallContext};
use crate::error::{AppError, AppResult};
use crate::models::{JobState, Role};
use crate::state::AppState;
use crate::web::layout::{principal_view, render, PrincipalView, GOLD};
use crate::web::middleware::{self, Current};

#[derive(Debug, Deserialize)]
pub struct LoginForm {
    pub number: String,
    pub password: String,
    pub csrf: String,
}

#[derive(Debug, Deserialize)]
pub struct LogoutForm {
    pub csrf: String,
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    title: &'static str,
    accent_class: &'static str,
    principal: PrincipalView,
    has_error: bool,
    error: String,
    number: String,
    csrf: String,
}

pub async fn login_form(State(state): State<AppState>, current: Current) -> AppResult<Response> {
    if current.0.is_some() {
        return Ok(Redirect::to("/").into_response());
    }
    login_response(&state, String::new(), false).await
}

async fn login_response(state: &AppState, number: String, has_error: bool) -> AppResult<Response> {
    let csrf = crate::auth::login_challenge(&state.db).await?;
    let cookie = format!(
        "{}={csrf}; Path=/; HttpOnly; SameSite=Strict; Max-Age=600{}",
        login_cookie_name(state.config.secure_cookies),
        if state.config.secure_cookies {
            "; Secure"
        } else {
            ""
        }
    );
    let page = render(&LoginTemplate {
        title: "Entrar",
        accent_class: GOLD,
        principal: principal_view(None, state.config.theme),
        has_error,
        error: "Número de cuenta o contraseña incorrectos.".to_string(),
        number,
        csrf,
    })?;
    Ok(([(SET_COOKIE, cookie)], page).into_response())
}

fn login_cookie_name(secure: bool) -> &'static str {
    if secure {
        "__Host-hone_login"
    } else {
        "hone_login"
    }
}

pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> AppResult<Response> {
    let context = audit::Context::from_headers(&headers);
    let number = form.number.trim();
    let cookie = middleware::cookie_token(&headers, login_cookie_name(state.config.secure_cookies))
        .ok_or(AppError::Forbidden)?;
    if !crate::auth::constant_time_eq(cookie.as_bytes(), form.csrf.as_bytes()) {
        return Err(AppError::Forbidden);
    }
    crate::auth::consume_login_challenge(&state.db, &cookie).await?;
    if !crate::auth::account_number_is_valid(number) || form.password.len() > 1024 {
        return Err(AppError::BadRequest(
            "invalid credentials format".to_string(),
        ));
    }
    crate::auth::limit_login(&state.db, number).await?;

    let permit = state
        .login_kdf
        .clone()
        .try_acquire_owned()
        .map_err(|_| AppError::RateLimited)?;
    let account = crate::auth::authenticate(&state.db, number, &form.password, permit).await?;

    let Some(account) = account else {
        context
            .record(
                &state.db,
                Event {
                    tenant_id: None,
                    actor_type: "anonymous",
                    actor_id: Some(number),
                    action: "auth.login",
                    resource_type: Some("account"),
                    resource_id: Some(number),
                    outcome: "denied",
                    reason: Some("invalid credentials"),
                },
            )
            .await?;

        return login_response(&state, number.to_string(), true).await;
    };

    let session = crate::auth::create_session(
        &state.db,
        account.id,
        state.config.session_ttl,
        middleware::client_ip(&headers).as_deref(),
        middleware::user_agent(&headers).as_deref(),
    )
    .await?;

    let tenant = crate::registry::tenant_by_account(&state.db, account.id).await?;

    context
        .record(
            &state.db,
            Event {
                tenant_id: tenant.as_ref().map(|tenant| tenant.id),
                actor_type: "account",
                actor_id: Some(&account.number),
                action: "auth.login",
                resource_type: Some("account"),
                resource_id: Some(&account.number),
                outcome: "ok",
                reason: None,
            },
        )
        .await?;

    let cookie = middleware::session_cookie(
        &session.id,
        state.config.session_ttl,
        state.config.secure_cookies,
    );
    let target = if account.role.is_operator() {
        "/operator"
    } else {
        "/portal"
    };

    Ok(([(SET_COOKIE, cookie)], Redirect::to(target)).into_response())
}

pub async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
    current: Current,
    Form(form): Form<LogoutForm>,
) -> AppResult<Response> {
    if let Some(principal) = current.0 {
        middleware::check_csrf(&principal, &form.csrf)?;
        crate::auth::delete_session(&state.db, &principal.session.id).await?;
        let context = audit::Context::from_headers(&headers);
        context
            .record(
                &state.db,
                Event {
                    tenant_id: principal.tenant_id(),
                    actor_type: "account",
                    actor_id: Some(&principal.account.number),
                    action: "auth.logout",
                    resource_type: Some("account"),
                    resource_id: Some(&principal.account.number),
                    outcome: "ok",
                    reason: None,
                },
            )
            .await?;
    }

    Ok((
        [(
            SET_COOKIE,
            middleware::cleared_cookie(state.config.secure_cookies),
        )],
        Redirect::to("/login"),
    )
        .into_response())
}

#[derive(Debug, Deserialize)]
pub struct SignupForm {
    pub name: String,
    pub csrf: String,
}

#[derive(Template)]
#[template(path = "signup.html")]
struct SignupTemplate {
    title: &'static str,
    accent_class: &'static str,
    principal: PrincipalView,
    has_error: bool,
    error: String,
    name: String,
    csrf: String,
}

#[derive(Template)]
#[template(path = "signup_created.html")]
struct SignupCreatedTemplate {
    title: &'static str,
    accent_class: &'static str,
    principal: PrincipalView,
    name: String,
    account_number: String,
    password: String,
    trial_days: i64,
    plan_code: String,
    outcome_ok: bool,
    broker_note: String,
    has_broker_note: bool,
}

pub async fn signup_form(State(state): State<AppState>, current: Current) -> AppResult<Response> {
    if current.0.is_some() {
        return Ok(Redirect::to("/").into_response());
    }
    signup_response(&state, String::new(), None).await
}

async fn signup_response(
    state: &AppState,
    name: String,
    error: Option<String>,
) -> AppResult<Response> {
    let csrf = crate::auth::login_challenge(&state.db).await?;
    let cookie = format!(
        "{}={csrf}; Path=/; HttpOnly; SameSite=Strict; Max-Age=600{}",
        login_cookie_name(state.config.secure_cookies),
        if state.config.secure_cookies {
            "; Secure"
        } else {
            ""
        }
    );
    let (has_error, error) = match error {
        Some(error) => (true, error),
        None => (false, String::new()),
    };
    let page = render(&SignupTemplate {
        title: "Crear cuenta",
        accent_class: GOLD,
        principal: principal_view(None, state.config.theme),
        has_error,
        error,
        name,
        csrf,
    })?;
    Ok(([(SET_COOKIE, cookie)], page).into_response())
}

/// Self-service sign-up: the shared login panel issues a tenant account whose
/// credentials are displayed exactly once, stored as an argon2id hash in the
/// registry, and bound to a trial subscription on the tenant's entitled plan.
pub async fn signup(
    State(state): State<AppState>,
    headers: HeaderMap,
    current: Current,
    Form(form): Form<SignupForm>,
) -> AppResult<Response> {
    if current.0.is_some() {
        return Ok(Redirect::to("/").into_response());
    }
    let cookie = middleware::cookie_token(&headers, login_cookie_name(state.config.secure_cookies))
        .ok_or(AppError::Forbidden)?;
    if !crate::auth::constant_time_eq(cookie.as_bytes(), form.csrf.as_bytes()) {
        return Err(AppError::Forbidden);
    }
    crate::auth::consume_login_challenge(&state.db, &cookie).await?;
    crate::auth::limit_signup(&state.db, middleware::client_ip(&headers).as_deref()).await?;

    let name = form.name.trim().to_string();
    if name.is_empty() || name.len() > 60 {
        return signup_response(
            &state,
            name,
            Some("El nombre debe tener entre 1 y 60 caracteres.".to_string()),
        )
        .await;
    }

    let _guard = state.mutations.lock().await;
    let slug = match crate::registry::next_tenant_slug(&state.db, &state.config.tenants).await {
        Ok(slug) => slug,
        Err(_) => {
            return signup_response(
                &state,
                name,
                Some(
                    "No quedan espacios de inquilino en este nodo; contacta con el operador."
                        .to_string(),
                ),
            )
            .await
        }
    };
    let password = crate::auth::generate_password(24)?;
    let account = crate::auth::create_account(&state.db, Role::Tenant, &password, None).await?;
    let tenant = crate::registry::create_tenant(&state.db, account.id, &slug, &name).await?;

    let context = audit::Context::from_headers(&headers);
    let broker_context =
        CallContext::new(&context.request_id, ActorKind::Tenant, &account.number)?;

    // The subscription binds the tenant to the first plan their live root
    // policy entitles: the plan code is operator policy, never app-chosen.
    // The trial length stays HONE_TRIAL_DAYS; the plan only names the envelope.
    let plan_code = crate::portal_catalog::load(&state.broker, &tenant.slug, &broker_context)
        .await
        .ok()
        .and_then(|entries| entries.first().map(|entry| entry.plan_code.clone()))
        .unwrap_or_else(|| "starter".to_string());
    let plan_id = crate::registry::ensure_plan_row(&state.db, &plan_code).await?;
    let now = OffsetDateTime::now_utc();
    let paid_through = now
        .checked_add(time::Duration::days(state.config.trial_days))
        .ok_or_else(|| AppError::BadRequest("trial out of range".to_string()))?;
    sqlx::query(
        "INSERT INTO subscriptions (tenant_id, plan_id, started_at, paid_through, auto_renew)
         VALUES (?, ?, ?, ?, 0)",
    )
    .bind(tenant.id)
    .bind(plan_id)
    .bind(now)
    .bind(paid_through)
    .execute(&state.db)
    .await?;

    let job = crate::registry::create_job(
        &state.db,
        Some(tenant.id),
        "tenant.create",
        &serde_json::json!({ "slug": slug, "origin": "signup", "correlation": context.request_id })
            .to_string(),
        Some(account.id),
    )
    .await?;

    let outcome = state
        .broker
        .call_ok_with_context(
            &broker::Request::TenantCreate {
                tenant_id: slug.clone(),
            },
            &broker_context,
        )
        .await;

    let (job_state, reason) = match &outcome {
        Ok(_) => (JobState::Done, None),
        Err(error) => (JobState::Failed, Some(error.to_string())),
    };
    crate::registry::finish_job(&state.db, job, job_state, reason.as_deref()).await?;

    context
        .record(
            &state.db,
            Event {
                tenant_id: Some(tenant.id),
                actor_type: "account",
                actor_id: Some(&account.number),
                action: "auth.signup",
                resource_type: Some("tenant"),
                resource_id: Some(&slug),
                outcome: if outcome.is_ok() { "ok" } else { "failed" },
                reason: reason.as_deref(),
            },
        )
        .await?;

    let (job_state, reason) = match &outcome {
        Ok(_) => (JobState::Done, None),
        Err(error) => (JobState::Failed, Some(error.to_string())),
    };
    crate::registry::finish_job(&state.db, job, job_state, reason.as_deref()).await?;

    context
        .record(
            &state.db,
            Event {
                tenant_id: Some(tenant.id),
                actor_type: "account",
                actor_id: Some(&account.number),
                action: "auth.signup",
                resource_type: Some("tenant"),
                resource_id: Some(&slug),
                outcome: if outcome.is_ok() { "ok" } else { "failed" },
                reason: reason.as_deref(),
            },
        )
        .await?;

    let has_broker_note = reason.is_some();
    Ok(render(&SignupCreatedTemplate {
        title: "Cuenta creada",
        accent_class: GOLD,
        principal: principal_view(None, state.config.theme),
        name,
        account_number: account.number,
        password,
        trial_days: state.config.trial_days,
        plan_code,
        outcome_ok: outcome.is_ok(),
        broker_note: reason.unwrap_or_default(),
        has_broker_note,
    })?
    .into_response())
}
