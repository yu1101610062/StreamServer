use std::time::Duration;

pub use crate::runtime_events::{
    RuntimeEventSink, RuntimeNotification, RuntimeTaskEvent, RuntimeTaskLogBatch,
    RuntimeTaskProgress, TerminalRuntimeReplay,
};
pub(crate) use crate::runtime_events::{bounded_log_batches, runtime_session_epoch};
pub use crate::runtime_manager::{
    RuntimeManager, RuntimeManagerHandle, RuntimeManagerRequestOutcome,
};
pub(crate) use crate::runtime_metadata::{
    CompanionProcessKind, StreamBinding, runtime_lease_token,
};
pub use crate::runtime_persistence::{
    cleanup_persisted_runtime_state, collect_terminal_runtime_replays, is_terminal_runtime_event,
};
pub(crate) use crate::runtime_plan::{TaskRuntimeMode, task_runtime_mode};
pub(crate) use crate::runtime_recording::{LiveRelayRecording, ZlmRecordKind};
pub(crate) use crate::runtime_recovery::classify_adopted_exit;
pub use crate::runtime_registry::{
    AdoptFilter, AdoptRuntimeFilter, RuntimeReadHandle, RuntimeReadModel,
};
pub use crate::runtime_types::{
    ExecutorError, RecordingControlAction, StartTaskRequest, StopTaskRequest,
    TaskRecordingControlRequest, rejected_runtime_handle,
};
pub(crate) use crate::runtime_types::{
    RuntimeCapabilityHints, StartupProbe, SuccessCheck, ZlmMediaStatus,
};

pub(crate) const STARTUP_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const STARTUP_PROBE_POLL_INTERVAL: Duration = Duration::from_secs(1);
pub(crate) const STOP_REQUESTED_STILL_RUNNING_LOG_INTERVAL: Duration = Duration::from_secs(10);
pub(crate) const RECORD_DURATION_FORCE_KILL_DELAY: Duration = Duration::from_millis(250);
