use media_domain::RuntimeHandle;
use tokio::sync::oneshot;

use crate::{
    runtime_adoption::RuntimeAdoptionOutcome,
    runtime_controls::RuntimeRecordingOutcome,
    runtime_executor::{RuntimeProcessExitOutcome, RuntimeStartWorkerResult},
    runtime_registry::AdoptFilter,
    runtime_stop::RuntimeStopOutcome,
    runtime_types::{
        ExecutorError, StartTaskRequest, StopTaskRequest, TaskRecordingControlRequest,
    },
};

use super::internal_event::{RuntimeGeneration, RuntimeInternalEvent, RuntimeMonitorSnapshot};
#[cfg(test)]
use super::state::RuntimeManagerState;
use super::state::RuntimeOperationId;

// actor command channel 是有界队列，背压点固定在 manager 入口，避免 controller
// 在重连或 Core 突发命令时无限堆积 runtime 控制请求。
pub(crate) const RUNTIME_MANAGER_COMMAND_BUFFER: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeManagerRequestOutcome<T> {
    Completed(T),
    StaleSession,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RuntimeManagerLimits {
    // 四类命令独立限流，防止慢 stop/adopt 把 heartbeat 或其他控制命令的 actor loop 卡住。
    // actor 只记录 active 数量，真实慢操作在 worker task 中 await。
    pub(crate) start: usize,
    pub(crate) stop: usize,
    pub(crate) recording: usize,
    pub(crate) adopt: usize,
}

impl Default for RuntimeManagerLimits {
    fn default() -> Self {
        Self {
            start: 4,
            stop: 8,
            recording: 4,
            adopt: 1,
        }
    }
}

pub(crate) enum RuntimeCommand {
    BeginSession {
        session_epoch: u64,
    },
    EndSession {
        session_epoch: u64,
    },
    CheckSession {
        session_epoch: u64,
        reply: oneshot::Sender<RuntimeManagerRequestOutcome<()>>,
    },
    StartTask {
        request: StartTaskRequest,
        reply: oneshot::Sender<Result<RuntimeHandle, ExecutorError>>,
    },
    StartTaskInSession {
        session_epoch: u64,
        request: StartTaskRequest,
        reply: oneshot::Sender<RuntimeManagerRequestOutcome<Result<RuntimeHandle, ExecutorError>>>,
    },
    StopTask {
        request: StopTaskRequest,
        reply: oneshot::Sender<Result<(), ExecutorError>>,
    },
    StopTaskInSession {
        session_epoch: u64,
        request: StopTaskRequest,
        reply: oneshot::Sender<RuntimeManagerRequestOutcome<Result<(), ExecutorError>>>,
    },
    SetTaskRecording {
        request: TaskRecordingControlRequest,
        reply: oneshot::Sender<Result<RuntimeHandle, ExecutorError>>,
    },
    SetTaskRecordingInSession {
        session_epoch: u64,
        request: TaskRecordingControlRequest,
        reply: oneshot::Sender<RuntimeManagerRequestOutcome<Result<RuntimeHandle, ExecutorError>>>,
    },
    AdoptOrphans {
        filter: AdoptFilter,
        reply: oneshot::Sender<Vec<RuntimeHandle>>,
    },
    AdoptOrphansInSession {
        session_epoch: u64,
        filter: AdoptFilter,
        reply: oneshot::Sender<RuntimeManagerRequestOutcome<Vec<RuntimeHandle>>>,
    },
    StartTaskFinished {
        operation_id: RuntimeOperationId,
        session_epoch: Option<u64>,
        request: StartTaskRequest,
        reply: RuntimeStartReply,
        result: Result<RuntimeStartWorkerResult, ExecutorError>,
    },
    StopTaskFinished {
        operation_id: RuntimeOperationId,
        session_epoch: Option<u64>,
        generation: Option<RuntimeGeneration>,
        request: StopTaskRequest,
        reply: Option<RuntimeStopReply>,
        result: Result<RuntimeStopOutcome, ExecutorError>,
    },
    SetTaskRecordingFinished {
        operation_id: RuntimeOperationId,
        session_epoch: Option<u64>,
        reply: RuntimeRecordingReply,
        result: Result<RuntimeHandle, ExecutorError>,
    },
    SetTaskRecordingForManagerFinished {
        runtime_id: uuid::Uuid,
        command_id: String,
        generation: RuntimeGeneration,
        result: Result<RuntimeRecordingOutcome, ExecutorError>,
    },
    AdoptOrphansFinished {
        operation_id: RuntimeOperationId,
        session_epoch: Option<u64>,
        reply: RuntimeAdoptReply,
        handles: Vec<RuntimeHandle>,
    },
    AdoptOrphansForManagerFinished {
        operation_id: RuntimeOperationId,
        session_epoch: Option<u64>,
        adopt_session_epoch: u64,
        reply: RuntimeAdoptReply,
        existing: Vec<(RuntimeHandle, RuntimeGeneration)>,
        outcomes: Vec<RuntimeAdoptionOutcome<RuntimeStartWorkerResult>>,
    },
    ObserveRuntimeSnapshot {
        handle: RuntimeHandle,
    },
    MonitorSnapshot {
        runtime_id: uuid::Uuid,
        generation: RuntimeGeneration,
        reply: oneshot::Sender<Option<RuntimeMonitorSnapshot>>,
    },
    RuntimeInternalEvent {
        event: RuntimeInternalEvent,
    },
    ProcessExitFinished {
        runtime_id: uuid::Uuid,
        generation: RuntimeGeneration,
        result: RuntimeProcessExitOutcome,
    },
    #[cfg(test)]
    InspectState {
        reply: oneshot::Sender<RuntimeManagerState>,
    },
    SetZlmServerId {
        server_id: String,
    },
    SetZlmRtmpEnhancedEnabled {
        enabled: Option<bool>,
    },
    #[allow(dead_code)]
    Shutdown,
}

pub(crate) enum RuntimeStartReply {
    Session(oneshot::Sender<RuntimeManagerRequestOutcome<Result<RuntimeHandle, ExecutorError>>>),
    Sessionless(oneshot::Sender<Result<RuntimeHandle, ExecutorError>>),
}

pub(crate) enum RuntimeStopReply {
    Session(oneshot::Sender<RuntimeManagerRequestOutcome<Result<(), ExecutorError>>>),
    Sessionless(oneshot::Sender<Result<(), ExecutorError>>),
}

pub(crate) enum RuntimeRecordingReply {
    Session(oneshot::Sender<RuntimeManagerRequestOutcome<Result<RuntimeHandle, ExecutorError>>>),
    Sessionless(oneshot::Sender<Result<RuntimeHandle, ExecutorError>>),
}

pub(crate) enum RuntimeAdoptReply {
    Session(oneshot::Sender<RuntimeManagerRequestOutcome<Vec<RuntimeHandle>>>),
    Sessionless(oneshot::Sender<Vec<RuntimeHandle>>),
}
