use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("not found")]
    NotFound,
    #[error("authentication required")]
    Unauthorized,
    #[error("not permitted")]
    Forbidden,
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Conflict(String),
    #[error("too many login attempts; try again later")]
    RateLimited,
    #[error("broker refused the request: {0}")]
    Broker(String),
    #[error("registry: {0}")]
    Database(#[from] sqlx::Error),
    #[error("internal: {0}")]
    Internal(#[from] anyhow::Error),
}

pub type AppResult<T> = Result<T, AppError>;

impl AppError {
    pub fn status(&self) -> StatusCode {
        match self {
            AppError::NotFound => StatusCode::NOT_FOUND,
            AppError::Unauthorized => StatusCode::UNAUTHORIZED,
            AppError::Forbidden => StatusCode::FORBIDDEN,
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::Conflict(_) => StatusCode::CONFLICT,
            AppError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            AppError::Broker(_) => StatusCode::BAD_GATEWAY,
            AppError::Database(_) | AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn is_server_fault(&self) -> bool {
        self.status().is_server_error()
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        if self.is_server_fault() {
            tracing::error!(error = %self, "request failed");
        }
        let message = if self.is_server_fault() {
            "request failed; consult the server logs".to_string()
        } else {
            self.to_string()
        };
        let limited = matches!(&self, AppError::RateLimited);
        let mut response = (self.status(), message).into_response();
        if limited {
            response.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from_static("900"),
            );
        }
        response
    }
}

impl From<sqlx::migrate::MigrateError> for AppError {
    fn from(error: sqlx::migrate::MigrateError) -> Self {
        AppError::Internal(anyhow::Error::new(error))
    }
}
