use std::sync::Arc;

use sqlx::SqlitePool;
use time::OffsetDateTime;

use crate::broker;
use crate::config::Config;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: SqlitePool,
    pub broker: broker::Client,
    pub started_at: OffsetDateTime,
    // Serializes broker mutations with the expiry sweeper in this single-host process.
    pub mutations: Arc<tokio::sync::Mutex<()>>,
    pub login_kdf: Arc<tokio::sync::Semaphore>,
}

impl AppState {
    pub fn new(config: Config, db: SqlitePool) -> Self {
        let broker = broker::Client::new(config.broker_socket.clone());
        Self {
            config: Arc::new(config),
            db,
            broker,
            started_at: OffsetDateTime::now_utc(),
            mutations: Arc::new(tokio::sync::Mutex::new(())),
            login_kdf: Arc::new(tokio::sync::Semaphore::new(2)),
        }
    }

    pub fn uptime_seconds(&self) -> i64 {
        (OffsetDateTime::now_utc() - self.started_at).whole_seconds()
    }
}
