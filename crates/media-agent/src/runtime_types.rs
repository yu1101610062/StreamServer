//! Runtime 基础类型：集中定义 executor 请求、控制动作、成功判定和错误类型。
//!
//! 这里不持有进程、网络或持久化逻辑，只提供各运行模块共享的轻量 DTO、枚举和拒绝态
//! handle 构造。

use std::path::PathBuf;

use chrono::Utc;
use media_domain::{RecordingControlSpec, RuntimeHandle, RuntimeState, TaskType};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

use crate::runtime_metadata::StreamBinding;

#[derive(Debug, Clone)]
pub struct StartTaskRequest {
    pub task_id: Uuid,
    pub attempt_no: i32,
    pub task_type: TaskType,
    pub resolved_spec: Value,
    pub execution_mode: String,
    pub lease_token: String,
    pub trace_context: Option<String>,
    pub session_epoch: u64,
}

#[derive(Debug, Clone)]
pub struct StopTaskRequest {
    pub task_id: Uuid,
    pub attempt_no: i32,
    pub lease_token: String,
    pub reason: String,
    pub grace_period_sec: u32,
    pub force_after_sec: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingControlAction {
    Start,
    Stop,
}

#[derive(Debug, Clone)]
pub struct TaskRecordingControlRequest {
    pub task_id: Uuid,
    pub attempt_no: i32,
    pub lease_token: String,
    pub action: RecordingControlAction,
    pub record: Option<RecordingControlSpec>,
    pub reason: String,
    pub command_id: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RuntimeCapabilityHints {
    pub(crate) zlm_rtmp_enhanced_enabled: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum SuccessCheck {
    FileExists(PathBuf),
    FilesExist(Vec<PathBuf>),
    ProcessExit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StartupProbe {
    pub(crate) schema: Option<String>,
    pub(crate) vhost: String,
    pub(crate) app: String,
    pub(crate) stream: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ZlmMediaStatus {
    pub(crate) binding: StreamBinding,
}

#[derive(Debug, Error)]
pub enum ExecutorError {
    #[error("runtime {task_id}/{attempt_no} was not found")]
    RuntimeNotFound { task_id: Uuid, attempt_no: i32 },
    #[error("{0}")]
    InvalidRequest(String),
    #[error("ZLM API call failed: {0}")]
    ApiCall(String),
    #[error("failed to spawn process: {0}")]
    ProcessSpawn(String),
    #[error("failed to signal process: {0}")]
    ProcessSignal(String),
}

pub fn rejected_runtime_handle(request: &StartTaskRequest) -> RuntimeHandle {
    RuntimeHandle {
        runtime_id: Uuid::now_v7(),
        task_id: request.task_id,
        attempt_no: request.attempt_no,
        worker_kind: request.task_type.default_worker_kind(),
        pid: None,
        started_at: Utc::now(),
        last_progress_at: None,
        state: RuntimeState::Pending,
        command_line: None,
        outputs: Vec::new(),
        metadata: json!({
            "task_type": request.task_type,
            "execution_mode": request.execution_mode,
            "lease_token": request.lease_token,
            "session_epoch": request.session_epoch,
            "trace_context": request.trace_context,
        }),
    }
}
