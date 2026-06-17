use axum::http::StatusCode;
use axum::response::IntoResponse;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AppError {
    #[error("Authentication failed: {0}")]
    AuthError(String),

    #[error("Authorization failed: {0}")]
    Forbidden(String),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Validation error: {0}")]
    Validation(String),

    #[error("Storage error: {0}")]
    Storage(String),

    #[error("RPC error: {0}")]
    RpcError(String),

    #[error("Execution error: {0}")]
    Execution(String),

    #[error("Notification error: {0}")]
    Notification(String),

    #[error("Config error: {0}")]
    Config(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

impl AppError {
    pub fn status_code(&self) -> StatusCode {
        match self {
            AppError::AuthError(_) => StatusCode::UNAUTHORIZED,
            AppError::Forbidden(_) => StatusCode::FORBIDDEN,
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::Validation(_) => StatusCode::BAD_REQUEST,
            AppError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::RpcError(_) => StatusCode::BAD_GATEWAY,
            AppError::Execution(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::Notification(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::Config(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let status = self.status_code();
        let body = axum::Json(serde_json::json!({
            "error": self.to_string(),
            "code": status.as_u16(),
        }));
        (status, body).into_response()
    }
}

pub type AppResult<T> = Result<T, AppError>;
