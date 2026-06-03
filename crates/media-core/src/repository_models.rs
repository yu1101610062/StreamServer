//! 仓储模型：集中定义 repository 对外返回的 DTO、查询过滤器、错误类型和任务摘要派生逻辑。

use std::str::FromStr;

use chrono::{DateTime, Utc};
use media_domain::{
    AttemptStatus, InputKind, PublishTargetKind, SourceMode, TaskSpec, TaskStateError, TaskStatus,
    TaskType, WorkerKind,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Row, postgres::PgRow};
use thiserror::Error;
use uuid::Uuid;

use super::{CallbackDeliverySummary, FileArtifactSummary, RecordFileSummary, TaskEventSummary};

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TaskListFilter {
    #[serde(default)]
    pub status: Option<TaskStatus>,
    #[serde(default, rename = "type")]
    pub task_type: Option<TaskType>,
    #[serde(default)]
    pub assigned_node_id: Option<Uuid>,
    #[serde(default)]
    pub keyword: Option<String>,
    #[serde(default)]
    pub created_from: Option<DateTime<Utc>>,
    #[serde(default)]
    pub created_to: Option<DateTime<Utc>>,
    #[serde(default)]
    pub page: Option<u32>,
    #[serde(default)]
    pub page_size: Option<u32>,
    #[serde(default)]
    pub sort_by: Option<String>,
    #[serde(default)]
    pub sort_order: Option<String>,
}

pub(super) const TASK_TRANSCODE_NONE: &str = "none";
pub(super) const TASK_TRANSCODE_ADAPTIVE: &str = "adaptive";
pub(super) const TASK_TRANSCODE_FORCED: &str = "forced";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: Uuid,
    pub name: String,
    #[serde(rename = "type")]
    pub task_type: TaskType,
    pub status: TaskStatus,
    pub priority: u8,
    pub created_by: String,
    pub assigned_node_id: Option<Uuid>,
    pub current_attempt_no: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcode_mode: Option<String>,
}

impl TaskSummary {
    pub(super) fn current_attempt_no_value(&self) -> Option<i32> {
        (self.current_attempt_no > 0).then_some(self.current_attempt_no)
    }

    pub(super) fn from_row(row: &PgRow) -> Result<Self, RepoError> {
        let task_type = TaskType::from_str(row.try_get::<&str, _>("task_type")?)
            .map_err(RepoError::ParseEnum)?;
        let status = TaskStatus::from_str(row.try_get::<&str, _>("status")?)
            .map_err(RepoError::ParseEnum)?;
        let priority = row.try_get::<i32, _>("priority")?;
        let resolved_spec = optional_json_column(row, "resolved_spec")?;
        let transcode_mode = resolved_spec
            .as_ref()
            .and_then(task_summary_transcode_mode_from_value);

        Ok(Self {
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            task_type,
            status,
            priority: u8::try_from(priority).unwrap_or(50),
            created_by: row.try_get("created_by")?,
            assigned_node_id: row.try_get("assigned_node_id")?,
            current_attempt_no: row.try_get("current_attempt_no")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
            started_at: row.try_get("started_at")?,
            finished_at: row.try_get("finished_at")?,
            transcode_mode,
        })
    }
}

fn optional_json_column(row: &PgRow, column: &str) -> Result<Option<Value>, RepoError> {
    match row.try_get::<Option<Value>, _>(column) {
        Ok(value) => Ok(value),
        Err(sqlx::Error::ColumnNotFound(_)) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn task_summary_transcode_mode_from_value(value: &Value) -> Option<String> {
    serde_json::from_value::<TaskSpec>(value.clone())
        .ok()
        .and_then(|spec| task_summary_transcode_mode(&spec).map(str::to_string))
}

pub(super) fn task_summary_transcode_mode(spec: &TaskSpec) -> Option<&'static str> {
    match task_runtime_mode(spec) {
        TaskRuntimeMode::ZlmProxy | TaskRuntimeMode::ZlmRtpServer => Some(TASK_TRANSCODE_NONE),
        TaskRuntimeMode::ManagedProcess => {
            if should_force_bridge_stabilization_transcode(spec) {
                Some(TASK_TRANSCODE_FORCED)
            } else {
                let default_mode = match spec.task_type {
                    TaskType::StreamBridge => "passthrough",
                    TaskType::StreamIngest | TaskType::FileTranscode => "copy_or_transcode",
                };
                effective_transcode_mode(spec.process.mode.as_deref(), default_mode)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskRuntimeMode {
    ZlmProxy,
    ZlmRtpServer,
    ManagedProcess,
}

fn task_runtime_mode(spec: &TaskSpec) -> TaskRuntimeMode {
    match spec.task_type {
        TaskType::FileTranscode | TaskType::StreamBridge => TaskRuntimeMode::ManagedProcess,
        TaskType::StreamIngest => match (spec.input.kind, spec.input.source_mode) {
            (Some(InputKind::GbRtp), _) => TaskRuntimeMode::ZlmRtpServer,
            (Some(InputKind::Rtsp | InputKind::Rtmp | InputKind::HttpFlv), _) => {
                TaskRuntimeMode::ZlmProxy
            }
            (Some(InputKind::Hls | InputKind::HttpTs), Some(SourceMode::Live)) => {
                TaskRuntimeMode::ZlmProxy
            }
            _ => TaskRuntimeMode::ManagedProcess,
        },
    }
}

fn effective_transcode_mode(mode: Option<&str>, default_mode: &str) -> Option<&'static str> {
    match mode.unwrap_or(default_mode) {
        "passthrough" => Some(TASK_TRANSCODE_NONE),
        "copy_or_transcode" => Some(TASK_TRANSCODE_ADAPTIVE),
        "force_transcode" => Some(TASK_TRANSCODE_FORCED),
        _ => None,
    }
}

fn should_force_bridge_stabilization_transcode(spec: &TaskSpec) -> bool {
    if spec.task_type != TaskType::StreamBridge
        || spec.process.mode.as_deref().unwrap_or("passthrough") != "passthrough"
    {
        return false;
    }

    bridge_output_format(spec).is_some_and(|format| format.eq_ignore_ascii_case("mpegts"))
        && matches!(
            spec.input.kind,
            Some(
                InputKind::Rtsp
                    | InputKind::Rtmp
                    | InputKind::Hls
                    | InputKind::HttpFlv
                    | InputKind::HttpTs
            )
        )
}

fn bridge_output_format(spec: &TaskSpec) -> Option<String> {
    match spec.publish.kind? {
        PublishTargetKind::File => Some(
            spec.publish
                .format
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("mp4")
                .to_ascii_lowercase(),
        ),
        PublishTargetKind::UdpMpegtsMulticast => Some(
            spec.publish
                .format
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("mpegts")
                .to_ascii_lowercase(),
        ),
        PublishTargetKind::RtpMulticast => Some(
            spec.publish
                .format
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("rtp_mpegts")
                .to_ascii_lowercase(),
        ),
        PublishTargetKind::RtmpPush => Some("flv".to_string()),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskDetail {
    pub task: TaskSummary,
    pub requested_spec: Value,
    pub resolved_spec: Option<Value>,
    pub current_attempt: Option<AttemptSummary>,
    pub recent_events: Vec<TaskEventSummary>,
    pub callback_delivery: Option<CallbackDeliverySummary>,
    pub records: Vec<RecordFileSummary>,
    pub file_artifacts: Vec<FileArtifactSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AttemptSummary {
    pub id: Uuid,
    pub attempt_no: i32,
    pub worker_kind: WorkerKind,
    pub status: AttemptStatus,
    pub node_id: Option<Uuid>,
    pub pid: Option<i32>,
    pub exit_code: Option<i32>,
    pub failure_code: Option<String>,
    pub failure_reason: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
}

impl AttemptSummary {
    pub(super) fn from_row(row: &PgRow) -> Result<Self, RepoError> {
        let worker_kind = WorkerKind::from_str(row.try_get::<&str, _>("worker_kind")?)
            .map_err(RepoError::ParseEnum)?;
        let status = AttemptStatus::from_str(row.try_get::<&str, _>("status")?)
            .map_err(RepoError::ParseEnum)?;

        Ok(Self {
            id: row.try_get("id")?,
            attempt_no: row.try_get("attempt_no")?,
            worker_kind,
            status,
            node_id: row.try_get("node_id")?,
            pid: row.try_get("pid")?,
            exit_code: row.try_get("exit_code")?,
            failure_code: row.try_get("failure_code")?,
            failure_reason: row.try_get("failure_reason")?,
            started_at: row.try_get("started_at")?,
            ended_at: row.try_get("ended_at")?,
        })
    }
}

#[derive(Debug, Error)]
pub enum RepoError {
    #[error("task {0} was not found")]
    TaskNotFound(Uuid),
    #[error("auth user {0} was not found")]
    AuthUserNotFound(String),
    #[error("node {0} was not found")]
    NodeNotFound(Uuid),
    #[error("media upload asset {0} was not found")]
    MediaUploadAssetNotFound(Uuid),
    #[error("task {0} is missing resolved_spec")]
    TaskMissingResolvedSpec(Uuid),
    #[error("task is not dispatchable from status {0}")]
    TaskNotDispatchable(TaskStatus),
    #[error("task cannot be deleted from status {0}")]
    TaskDeleteForbidden(TaskStatus),
    #[error("recording control is not supported: {0}")]
    RecordingControlUnsupported(String),
    #[error("idempotency key already exists with different request body")]
    IdempotencyConflict,
    #[error("operation with the same idempotency key is still in progress")]
    OperationInProgress,
    #[error("task {task_id} attempt {attempt_no} violates repository invariants: {detail}")]
    TaskAttemptInvariant {
        task_id: Uuid,
        attempt_no: i32,
        detail: String,
    },
    #[error(transparent)]
    TaskState(#[from] TaskStateError),
    #[error(transparent)]
    Validation(#[from] media_domain::TaskValidationError),
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
    #[error(transparent)]
    ParseEnum(#[from] media_domain::task::ParseEnumError),
}
