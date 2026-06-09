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
        let parts = self.into_response_parts();
        let body = ErrorBody {
            code: parts.code,
            message: parts.message,
            request_id,
            details: (!parts.details.is_null()).then_some(parts.details),
        };

        (parts.status, Json(body)).into_response()
    }
}

impl AppError {
    fn into_response_parts(self) -> ErrorResponseParts {
        // HTTP 层只负责把内部错误映射成稳定 API 错误码；具体错误来源
        // 继续委托给 control_plane/repository 的专用映射函数。
        match self {
            Self::BadRequest(message) => (
                StatusCode::BAD_REQUEST,
                "VALIDATION_BAD_REQUEST",
                message,
                Value::Null,
            )
                .into(),
            Self::Forbidden(message) => (
                StatusCode::FORBIDDEN,
                "ACCESS_FORBIDDEN",
                message,
                Value::Null,
            )
                .into(),
            Self::NotFound(message) => (
                StatusCode::NOT_FOUND,
                "RESOURCE_NOT_FOUND",
                message,
                Value::Null,
            )
                .into(),
            Self::Validation(error) => (
                StatusCode::BAD_REQUEST,
                "VALIDATION_TASK_SPEC_INVALID",
                "task validation failed".to_string(),
                json!({ "issues": error.issues }),
            )
                .into(),
            Self::ControlPlane(error) => control_plane_error_parts(error),
            Self::Repository(error) => repo_error_parts(error),
            Self::Internal(message) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "CORE_INTERNAL_ERROR",
                message,
                Value::Null,
            )
                .into(),
        }
    }
}

fn control_plane_error_parts(error: ControlPlaneError) -> ErrorResponseParts {
    // 控制面错误面向“是否还有可用 agent”这一层语义；Repository 包装错误
    // 继续下沉到仓储映射，避免同一种数据库错误出现两个 API code。
    match error {
        ControlPlaneError::NoConnectedNode => (
            StatusCode::SERVICE_UNAVAILABLE,
            "CONTROL_PLANE_NO_CONNECTED_NODE",
            "no connected media-agent is available".to_string(),
            Value::Null,
        )
            .into(),
        ControlPlaneError::NodeDisconnected(node_id) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "CONTROL_PLANE_NODE_DISCONNECTED",
            format!("media-agent {node_id} is not connected"),
            Value::Null,
        )
            .into(),
        ControlPlaneError::Repository(error) => repo_error_parts(error),
        ControlPlaneError::Serde(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "CONTROL_PLANE_SERIALIZATION_ERROR",
            error.to_string(),
            Value::Null,
        )
            .into(),
    }
}

fn repo_error_parts(error: RepoError) -> ErrorResponseParts {
    // 仓储错误按客户端可行动性分组：找不到返回 404，状态冲突返回 409，
    // 规格校验返回 400，其余内部错误不泄露实现细节。
    match error {
        RepoError::TaskNotFound(task_id) => (
            StatusCode::NOT_FOUND,
            "TASK_NOT_FOUND",
            format!("task {task_id} was not found"),
            Value::Null,
        )
            .into(),
        RepoError::MediaUploadAssetNotFound(asset_id) => (
            StatusCode::NOT_FOUND,
            "MEDIA_UPLOAD_ASSET_NOT_FOUND",
            format!("media upload asset {asset_id} was not found"),
            Value::Null,
        )
            .into(),
        RepoError::TaskMissingResolvedSpec(task_id) => (
            StatusCode::CONFLICT,
            "TASK_MISSING_RESOLVED_SPEC",
            format!("task {task_id} is missing resolved_spec"),
            Value::Null,
        )
            .into(),
        RepoError::TaskNotDispatchable(status) => (
            StatusCode::CONFLICT,
            "TASK_NOT_DISPATCHABLE",
            format!("task is not dispatchable from status {status}"),
            Value::Null,
        )
            .into(),
        RepoError::TaskDeleteForbidden(status) => (
            StatusCode::CONFLICT,
            "TASK_DELETE_FORBIDDEN",
            format!("task cannot be deleted from status {status}"),
            Value::Null,
        )
            .into(),
        RepoError::RecordingControlUnsupported(message) => (
            StatusCode::CONFLICT,
            "RECORDING_CONTROL_UNSUPPORTED",
            message,
            Value::Null,
        )
            .into(),
        RepoError::IdempotencyConflict => (
            StatusCode::CONFLICT,
            "CONFLICT_IDEMPOTENCY_KEY",
            "idempotency key already exists with a different request body".to_string(),
            Value::Null,
        )
            .into(),
        RepoError::OperationInProgress => (
            StatusCode::CONFLICT,
            "CONFLICT_OPERATION_IN_PROGRESS",
            "a request with the same idempotency key is still being processed".to_string(),
            Value::Null,
        )
            .into(),
        RepoError::TaskState(error) => (
            StatusCode::CONFLICT,
            "TASK_INVALID_STATE",
            error.to_string(),
            Value::Null,
        )
            .into(),
        RepoError::Validation(error) => (
            StatusCode::BAD_REQUEST,
            "VALIDATION_TASK_SPEC_INVALID",
            "task validation failed".to_string(),
            json!({ "issues": error.issues }),
        )
            .into(),
        other => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "CORE_INTERNAL_ERROR",
            other.to_string(),
            Value::Null,
        )
            .into(),
    }
}

struct ErrorResponseParts {
    status: StatusCode,
    code: &'static str,
    message: String,
    details: Value,
}

impl From<(StatusCode, &'static str, String, Value)> for ErrorResponseParts {
    fn from((status, code, message, details): (StatusCode, &'static str, String, Value)) -> Self {
        Self {
            status,
            code,
            message,
            details,
        }
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
