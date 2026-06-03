//! 仓储层门面模块：集中装配拆分后的 repository 子模块，并保持对上层调用方的公开 API 不变。

#[cfg(test)]
#[path = "tests/repository.rs"]
mod tests;

#[path = "repository_agent_events.rs"]
mod repository_agent_events;
#[path = "repository_artifacts.rs"]
mod repository_artifacts;
#[path = "repository_attempt_state.rs"]
mod repository_attempt_state;
#[path = "repository_auth.rs"]
mod repository_auth;
#[path = "repository_callback_outbox.rs"]
mod repository_callback_outbox;
#[path = "repository_core.rs"]
mod repository_core;
#[path = "repository_dispatch.rs"]
mod repository_dispatch;
#[path = "repository_events.rs"]
mod repository_events;
#[path = "repository_lifecycle.rs"]
mod repository_lifecycle;
#[path = "repository_models.rs"]
mod repository_models;
#[path = "repository_nodes.rs"]
mod repository_nodes;
#[path = "repository_schedules.rs"]
mod repository_schedules;
#[path = "repository_streams.rs"]
mod repository_streams;
#[path = "repository_task_spec.rs"]
mod repository_task_spec;
#[path = "repository_tasks.rs"]
mod repository_tasks;
#[path = "repository_zlm_hooks.rs"]
mod repository_zlm_hooks;

pub use repository_agent_events::{
    AgentTaskEventRecord, TaskLogBatchRecord, TaskProgressRecord, TaskSnapshotRecord,
};
pub use repository_artifacts::{
    FileArtifactKind, FileArtifactListFilter, FileArtifactSummary, MediaUploadAssetDeleteTarget,
    MediaUploadAssetListFilter, MediaUploadAssetSummary, NewMediaUploadAsset, RecordFileSummary,
    RecordListFilter,
};
pub use repository_auth::{
    AuthUser, MachineAllowlistEntry, MachineAllowlistWrite, NewRefreshSession,
    SecurityAuditEventRecord,
};
pub use repository_callback_outbox::{CallbackDeliverySummary, CallbackOutboxJob};
pub use repository_core::TaskRepository;
#[allow(unused_imports)]
pub use repository_dispatch::{
    DispatchCommand, ReclaimRuntimeCommand, ReclaimingTaskReconcile, RecordingControlCommand,
    StopCommand, StoppingTaskReconcile,
};
pub use repository_events::{TaskEventFilter, TaskEventSummary, TaskLogFilter, TaskLogResponse};
#[allow(unused_imports)]
pub use repository_lifecycle::{
    TaskCloneCommonOverride, TaskCloneOverride, TaskCloneScheduleOverride,
};
pub use repository_models::{AttemptSummary, RepoError, TaskDetail, TaskListFilter, TaskSummary};
pub use repository_nodes::{NodeDebugTarget, NodeHeartbeatSummary, NodeSummary};
pub use repository_schedules::CronScheduleEntry;
pub use repository_streams::{
    HookEventListFilter, HookEventSummary, StreamListFilter, StreamSummary,
};
pub(crate) use repository_task_spec::validation_error;
pub use repository_tasks::{CreateTaskResult, TaskPreview};
#[allow(unused_imports)]
pub use repository_zlm_hooks::{
    PublishTaskTarget, ZlmPublishTaskRecord, ZlmRecordFileRecord, ZlmStreamEventRecord,
    ZlmTaskEventHookRecord,
};

use repository_attempt_state::{
    DEFAULT_MAX_CONSECUTIVE_FAILURES, OwnershipMode, retry_enabled_on_disconnect,
    start_rejected_retry_limit, sticky_reconnect_active, sticky_reconnect_from_spec_value,
};
use repository_models::task_summary_transcode_mode;

#[cfg(test)]
use media_domain::{TaskSpec, TaskType};
#[cfg(test)]
use repository_agent_events::should_persist_agent_task_event;
#[cfg(test)]
use repository_models::{TASK_TRANSCODE_ADAPTIVE, TASK_TRANSCODE_FORCED, TASK_TRANSCODE_NONE};
#[cfg(test)]
use repository_task_spec::{
    build_resolved_task_json, task_spec_overlay, validate_managed_file_publish_target,
};
#[cfg(test)]
use repository_zlm_hooks::{
    HookStreamBinding, compact_hook_payload, should_persist_hook_event,
    should_persist_record_file_hook, should_persist_zlm_stream_event,
};
#[cfg(test)]
use serde_json::json;
#[cfg(test)]
use uuid::Uuid;

#[cfg(test)]
pub(crate) use crate::repository_paths::externalize_managed_path;
pub(crate) use crate::repository_paths::{
    OutputMountPrefixes, absolute_http_url_from_file_path, externalize_http_visible_path,
    externalize_path_fields_in_payload, is_hls_playlist_record_path, relative_http_url_from_path,
    task_id_from_managed_output_path,
};
