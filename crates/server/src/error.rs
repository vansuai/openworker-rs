//! Server error types.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("not found")]
    NotFound,

    #[error("unauthorized")]
    Unauthorized,

    #[error("session not found: {0}")]
    SessionNotFound(String),

    #[error("workspace required")]
    WorkspaceRequired,

    #[error("invalid request: {0}")]
    BadRequest(String),

    #[error("internal error: {0}")]
    Internal(String),

    #[error("provider error: {0}")]
    Provider(#[from] ocw_provider::Error),
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, msg) = match &self {
            Error::NotFound => (StatusCode::NOT_FOUND, self.to_string()),
            Error::Unauthorized => (StatusCode::UNAUTHORIZED, self.to_string()),
            Error::SessionNotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
            Error::WorkspaceRequired => (StatusCode::BAD_REQUEST, self.to_string()),
            Error::BadRequest(s) => (StatusCode::BAD_REQUEST, s.clone()),
            Error::Internal(s) => (StatusCode::INTERNAL_SERVER_ERROR, s.clone()),
            Error::Provider(e) => (StatusCode::BAD_GATEWAY, e.to_string()),
        };
        let body = Json(json!({ "error": msg }));
        (status, body).into_response()
    }
}

pub type Result<T> = std::result::Result<T, Error>;
