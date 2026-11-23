pub mod auth_pages;
pub mod layout;
pub mod middleware;
pub mod operator;
pub mod tenant;
pub mod container_pages;

use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use tower_http::services::ServeDir;

use crate::state::AppState;
use middleware::Current;

pub fn router(state: AppState) -> Router {
    let static_files = ServeDir::new(state.config.static_dir.clone());

    Router::new()
        .route("/", get(root))
        .nest_service("/static", static_files)
        .route(
            "/login",
            get(auth_pages::login_form).post(auth_pages::login),
        )
        .route(
            "/signup",
            get(auth_pages::signup_form).post(auth_pages::signup),
        )
        .route("/logout", post(auth_pages::logout))
        .route("/operator", get(operator::dashboard))
        .route(
            "/operator/tenants",
            get(operator::tenants_page).post(operator::create_tenant),
        )
        .route("/operator/tenants/{id}", get(operator::tenant_detail))
        .route("/operator/tenants/{id}/delete", post(operator::delete_tenant))
        .route("/operator/tenants/{id}/services", post(operator::provision))
        .route("/operator/services", get(operator::services_page))
        .route("/operator/catalog", get(operator::catalog_page))
        .route("/operator/services/{id}", get(operator::service_detail))
        .route("/operator/services/{id}/time", post(operator::add_time))
        .route(
            "/operator/services/{id}/{action}",
            post(operator::service_action),
        )
        .route("/operator/jobs", get(operator::jobs_page))
        .route("/operator/audit", get(operator::audit_page))
        .route(
            "/operator/keys",
            get(operator::keys_page).post(operator::generate_keys),
        )
        .route("/portal/containers/new", get(container_pages::new_form).post(container_pages::create))
        .route("/portal", get(tenant::portal))
        .route("/portal/redeem", post(tenant::redeem))
        .route("/portal/registry", get(tenant::registry_page))
        .route("/portal/catalog", get(tenant::catalog_page))
        .route("/portal/images", get(tenant::images_page))
        .route("/portal/deploy", get(tenant::deploy_form))
        .route("/portal/services", post(tenant::provision))
        .route("/portal/services/{id}", get(tenant::service_detail))
        .route(
            "/portal/services/{id}/configuration",
            get(tenant::service_configuration).post(tenant::configure_service),
        )
        .route("/portal/services/{id}/logs", get(tenant::service_logs))
        .route(
            "/portal/services/{id}/site",
            get(tenant::site_page).post(tenant::site_write),
        )
        .route(
            "/portal/services/{id}/{action}",
            post(tenant::service_action),
        )
        .route("/portal/audit", get(tenant::audit_page))
        .merge(crate::api::router())
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::load_principal,
        ))
        .layer(axum::middleware::from_fn(middleware::security_headers))
        .with_state(state)
}

async fn root(current: Current) -> Response {
    match current.0 {
        Some(principal) if principal.role().is_operator() => {
            Redirect::to("/operator").into_response()
        }
        Some(_) => Redirect::to("/portal").into_response(),
        None => Redirect::to("/login").into_response(),
    }
}
