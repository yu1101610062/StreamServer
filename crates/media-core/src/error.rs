use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use media_domain::TaskValidationError;
use serde::Serialize;
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

use crate::{control_plane::ControlPlaneError, repository::RepoError};

#[derive(Debug, Error)]
pub enum AppError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    NotFound(String),
    #[error(transparent)]
    Validation(#[from] TaskValidationError),
    #[error(transparent)]
    ControlPlane(#[from] ControlPlaneError),
    #[error(transparent)]
    Repository(#[from] RepoError),
    #[error("{0}")]
    Internal(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let request_id = Uuid::now_v7().to_string();
        let (status, code, message, details) = match self {
            Self::BadRequest(message) => (
                StatusCode::BAD_REQUEST,
                "VALIDATION_BAD_REQUEST",
                message,
                Value::Null,
            ),
            Self::Forbidden(message) => (
                StatusCode::FORBIDDEN,
                "ACCESS_FORBIDDEN",
                message,
                Value::Null,
            ),
            Self::NotFound(message) => (
                StatusCode::NOT_FOUND,
                "RESOURCE_NOT_FOUND",
                message,
                Value::Null,
            ),
            Self::Validation(error) => (
                StatusCode::BAD_REQUEST,
                "VALIDATION_TASK_SPEC_INVALID",
                "task validation failed".to_string(),
                json!({ "issues": error.issues }),
            ),
            Self::ControlPlane(error) => match error {
                ControlPlaneError::NoConnectedNode => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "CONTROL_PLANE_NO_CONNECTED_NODE",
                    "no connected media-agent is available".to_string(),
                    Value::Null,
                ),
                ControlPlaneError::NodeDisconnected(node_id) => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "CONTROL_PLANE_NODE_DISCONNECTED",
                    format!("media-agent {node_id} is not connected"),
                    Value::Null,
                ),
                ControlPlaneError::Repository(error) => {
                    return Self::Repository(error).into_response();
                }
                ControlPlaneError::Serde(error) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "CONTROL_PLANE_SERIALIZATION_ERROR",
                    error.to_string(),
                    Value::Null,
                ),
            },
            Self::Repository(error) => match error {
                RepoError::TaskNotFound(task_id) => (
                    StatusCode::NOT_FOUND,
                    "TASK_NOT_FOUND",
                    format!("task {task_id} was not found"),
                    Value::Null,
                ),
                RepoError::TemplateNotFound(template_id) => (
                    StatusCode::NOT_FOUND,
                    "TEMPLATE_NOT_FOUND",
                    format!("template {template_id} was not found"),
                    Value::Null,
                ),
                RepoError::TaskMissingResolvedSpec(task_id) => (
                    StatusCode::CONFLICT,
                    "TASK_MISSING_RESOLVED_SPEC",
                    format!("task {task_id} is missing resolved_spec"),
                    Value::Null,
                ),
                RepoError::TaskNotDispatchable(status) => (
                    StatusCode::CONFLICT,
                    "TASK_NOT_DISPATCHABLE",
                    format!("task is not dispatchable from status {status}"),
                    Value::Null,
                ),
                RepoError::IdempotencyConflict => (
                    StatusCode::CONFLICT,
                    "CONFLICT_IDEMPOTENCY_KEY",
                    "idempotency key already exists with a different request body".to_string(),
                    Value::Null,
                ),
                RepoError::OperationInProgress => (
                    StatusCode::CONFLICT,
                    "CONFLICT_OPERATION_IN_PROGRESS",
                    "a request with the same idempotency key is still being processed".to_string(),
                    Value::Null,
                ),
                RepoError::TaskState(error) => (
                    StatusCode::CONFLICT,
                    "TASK_INVALID_STATE",
                    error.to_string(),
                    Value::Null,
                ),
                RepoError::Validation(error) => (
                    StatusCode::BAD_REQUEST,
                    "VALIDATION_TASK_SPEC_INVALID",
                    "task validation failed".to_string(),
                    json!({ "issues": error.issues }),
                ),
                other => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "CORE_INTERNAL_ERROR",
                    other.to_string(),
                    Value::Null,
                ),
            },
            Self::Internal(message) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "CORE_INTERNAL_ERROR",
                message,
                Value::Null,
            ),
        };

        let body = ErrorBody {
            code,
            message,
            request_id,
            details: (!details.is_null()).then_some(details),
        };

        (status, Json(body)).into_response()
    }
}

#[derive(Debug, Serialize)]
struct ErrorBody<'a> {
    code: &'a str,
    message: String,
    request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<Value>,
}
