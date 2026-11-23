mod api;
mod audit;
mod auth;
mod billing;
mod broker;
mod catalog;
mod config;
mod containers;
mod error;
mod images;
mod lifecycle;
mod models;
mod observation;
mod portal_catalog;
mod provisioning;
mod registry;
mod service_options;
mod state;
mod telemetry;
mod topup;
mod trusted_registry;
mod web;

#[cfg(test)]
mod observation_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod provisioning_tests;
#[cfg(test)]
mod portal_service_tests;

use anyhow::{Context, Result};
use sqlx::SqlitePool;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::config::{Config, LogFormat};
use crate::models::Role;
use crate::state::AppState;

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::from_env().context("invalid configuration")?;
    init_tracing(config.log_format);

    let pool = registry::connect(&config.registry_path)
        .await
        .context("cannot open the registry")?;
    registry::migrate(&pool)
        .await
        .context("cannot apply migrations")?;

    bootstrap_operator(&pool, &config).await?;

    let state = AppState::new(config, pool);
    lifecycle::spawn(state.clone());

    let listener = tokio::net::TcpListener::bind(state.config.bind)
        .await
        .with_context(|| format!("cannot bind {}", state.config.bind))?;

    tracing::info!(
        bind = %state.config.bind,
        version = env!("CARGO_PKG_VERSION"),
        registry = %state.config.registry_path.display(),
        broker = %state.broker.socket().display(),
        "honenode listening"
    );

    axum::serve(listener, web::router(state.clone()))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server failed")?;

    tracing::info!("honenode stopped");
    Ok(())
}

async fn bootstrap_operator(pool: &SqlitePool, config: &Config) -> Result<()> {
    if auth::operator_exists(pool).await? {
        return Ok(());
    }

    // First run without an imported secret: the app generates the operator
    // credentials itself and surfaces them exactly once — on the log and in a
    // mode-0600 bootstrap file next to the registry.
    let generated;
    let password = match config.operator_password.as_deref() {
        Some(password) => password,
        None => {
            generated = auth::generate_password(24)?;
            &generated
        }
    };

    let account = auth::create_account(
        pool,
        Role::Operator,
        password,
        config.operator_account.as_deref(),
    )
    .await?;

    if config.operator_password.is_none() {
        let path = config.bootstrap_credentials_path();
        match write_bootstrap_file(&path, &account.number, password) {
            Ok(()) => tracing::warn!(
                number = %account.number,
                password = %password,
                bootstrap_file = %path.display(),
                "first-run operator credentials — save them now; delete the mode-0600 bootstrap file after saving"
            ),
            Err(error) => tracing::warn!(
                number = %account.number,
                password = %password,
                error = %error,
                "first-run operator credentials — save them now; this journal entry is the only copy"
            ),
        }
    } else {
        tracing::info!(
            number = %account.number,
            "operator account created — record the number, it is the sign-in identifier"
        );
    }
    Ok(())
}

fn write_bootstrap_file(path: &std::path::Path, number: &str, password: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    // O_EXCL: never overwrite or truncate an existing bootstrap file.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    writeln!(file, "HONE_OPERATOR_ACCOUNT={number}")?;
    writeln!(file, "HONE_OPERATOR_PASSWORD={password}")?;
    writeln!(file)?;
    writeln!(file, "# Credenciales del panel de operador generadas en el primer arranque.")?;
    writeln!(file, "# Guárdalas en un entorno seguro; no se vuelven a mostrar.")?;
    Ok(())
}

fn init_tracing(format: LogFormat) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let registry = tracing_subscriber::registry().with(filter);

    match format {
        LogFormat::Json => registry
            .with(tracing_subscriber::fmt::layer().json())
            .init(),
        LogFormat::Plain => registry.with(tracing_subscriber::fmt::layer()).init(),
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };

    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
