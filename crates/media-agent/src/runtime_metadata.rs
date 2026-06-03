//! 运行时元数据协议：集中维护 RuntimeHandle.metadata 的结构解析、状态标记和事件载荷拼装。

use std::str::FromStr;

use chrono::Utc;
use media_domain::{RecoveryPolicy, RuntimeHandle, TaskSpec, TaskType};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    config::AgentSettings,
    runtime::{
        ExecutorError, StartTaskRequest, StartupProbe, TaskRecordingControlRequest,
        TaskRuntimeMode, task_runtime_mode,
    },
    runtime_events::{
        RuntimeEventSink, RuntimeNotification, RuntimeTaskEvent, runtime_session_epoch,
    },
    runtime_outputs::ManagedFileOutputKind,
    runtime_process::{ProcessIdentity, linux_pid_start_time},
    runtime_recording::{LiveRelayRecording, should_start_live_relay_recording},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StreamBinding {
    pub(crate) schema: Option<String>,
    pub(crate) vhost: String,
    pub(crate) app: String,
    pub(crate) stream: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompanionProcessKind {
    StreamIngestMp4Record,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompanionProcessState {
    #[default]
    Starting,
    Running,
    Succeeded,
    Failed,
    Exited,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompanionProcessMetadata {
    pub(crate) kind: CompanionProcessKind,
    pub(crate) pid: Option<i32>,
    #[serde(default)]
    pub(crate) pgid: Option<i32>,
    #[serde(default)]
    pub(crate) pid_start_time: Option<u64>,
    pub(crate) output_target: String,
    pub(crate) outputs: Vec<String>,
    #[serde(default)]
    pub(crate) command_line: Option<String>,
    #[serde(default)]
    pub(crate) state: CompanionProcessState,
    #[serde(default)]
    pub(crate) error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RtpServerMetadata {
    pub(crate) stream_id: String,
    pub(crate) local_port: u16,
    pub(crate) requested_port: u16,
    pub(crate) tcp_mode: u8,
    pub(crate) reuse_port: Option<bool>,
    pub(crate) ssrc: Option<u32>,
}

pub(crate) fn requires_stream_online(handle: &RuntimeHandle) -> bool {
    handle
        .metadata
        .get("startup_probe")
        .map(|value| !value.is_null())
        .unwrap_or(false)
}

pub(crate) fn stream_online(handle: &RuntimeHandle) -> bool {
    handle
        .metadata
        .get("stream_online")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

pub(crate) fn live_relay_uses_recording_startup(
    startup_probe: &StartupProbe,
    handle: &RuntimeHandle,
) -> bool {
    startup_probe.schema.is_none() && live_relay_recording_from_handle(handle).is_some()
}

pub(crate) fn completion_reason_from_handle(handle: &RuntimeHandle) -> Option<String> {
    handle
        .metadata
        .get("completion_reason")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            live_relay_recording_from_handle(handle)
                .and_then(|recording| recording.completion_reason)
        })
}

pub(crate) fn stop_reason_from_handle(handle: &RuntimeHandle) -> Option<String> {
    handle
        .metadata
        .get("stop")
        .and_then(|value| value.get("reason"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

pub(crate) fn fatal_recording_error_from_handle(handle: &RuntimeHandle) -> Option<String> {
    handle
        .metadata
        .get("recording_fatal_error")
        .and_then(Value::as_str)
        .map(str::to_string)
}

pub(crate) fn stream_binding_from_handle(handle: &RuntimeHandle) -> Option<StreamBinding> {
    handle
        .metadata
        .get("stream_binding")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
}

pub(crate) fn managed_stream_restart_cleanup_binding(
    handle: &RuntimeHandle,
) -> Option<StreamBinding> {
    if task_type_from_handle(handle) != Some(TaskType::StreamIngest)
        || task_runtime_mode_from_handle(handle) != Some(TaskRuntimeMode::ManagedProcess)
    {
        return None;
    }

    stream_binding_from_handle(handle).or_else(|| {
        startup_probe_from_handle(handle).map(|probe| StreamBinding {
            schema: probe.schema,
            vhost: probe.vhost,
            app: probe.app,
            stream: probe.stream,
        })
    })
}

pub(crate) fn rtp_stream_id_from_handle(handle: &RuntimeHandle) -> Option<String> {
    handle
        .metadata
        .get("rtp_stream_id")
        .and_then(Value::as_str)
        .map(str::to_string)
}

pub(crate) fn rtp_server_from_handle(handle: &RuntimeHandle) -> Option<RtpServerMetadata> {
    handle
        .metadata
        .get("rtp_server")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
}

pub(crate) fn process_identity_from_handle(handle: &RuntimeHandle) -> Option<ProcessIdentity> {
    handle
        .metadata
        .get("process")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .or_else(|| handle.pid.map(ProcessIdentity::pid_only))
}

pub(crate) fn live_relay_recording_from_handle(
    handle: &RuntimeHandle,
) -> Option<LiveRelayRecording> {
    handle
        .metadata
        .get("recording")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
}

pub(crate) fn zlm_proxy_key_from_handle(handle: &RuntimeHandle) -> Option<String> {
    handle
        .metadata
        .get("zlm_proxy_key")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

pub(crate) fn live_relay_startup_ready(handle: &RuntimeHandle) -> bool {
    live_relay_recording_from_handle(handle)
        .is_none_or(|recording| !should_start_live_relay_recording(&recording))
}

pub(crate) fn live_relay_auto_close_enabled(settings: &AgentSettings, spec: &TaskSpec) -> bool {
    settings.zlm_auto_close_on_no_reader_enabled && spec.expose.stop_on_no_reader.unwrap_or(false)
}

pub(crate) fn live_relay_auto_close_enabled_from_handle(
    settings: &AgentSettings,
    handle: &RuntimeHandle,
) -> bool {
    resolved_spec_from_handle(handle)
        .map(|spec| live_relay_auto_close_enabled(settings, &spec))
        .unwrap_or(false)
}

pub(crate) fn recovery_policy_from_handle(handle: &RuntimeHandle) -> Option<RecoveryPolicy> {
    resolved_spec_from_handle(handle).and_then(|spec| spec.recovery.policy)
}

pub(crate) fn continuous_stream_ingest_from_handle(handle: &RuntimeHandle) -> bool {
    resolved_spec_from_handle(handle).is_some_and(|spec| spec.stream_ingest_is_continuous())
}

pub(crate) fn sticky_reconnect_stream_ingest_from_handle(handle: &RuntimeHandle) -> bool {
    resolved_spec_from_handle(handle).is_some_and(|spec| spec.stream_ingest_uses_sticky_reconnect())
}

pub(crate) fn stream_ingest_recording_enabled_from_handle(handle: &RuntimeHandle) -> bool {
    let spec_enabled = resolved_spec_from_handle(handle).is_some_and(|spec| {
        spec.task_type == TaskType::StreamIngest && spec.record.enabled.unwrap_or(false)
    });
    spec_enabled
        || live_relay_recording_from_handle(handle)
            .is_some_and(|recording| recording.desired_enabled || recording.started)
}

pub(crate) fn recording_gap_active(handle: &RuntimeHandle) -> bool {
    handle
        .metadata
        .get("recording_gap_active")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

pub(crate) fn should_emit_recording_gap_started(handle: &RuntimeHandle) -> bool {
    stream_ingest_recording_enabled_from_handle(handle) && !recording_gap_active(handle)
}

pub(crate) fn should_emit_source_reconnecting(handle: &RuntimeHandle, reason: &str) -> bool {
    !handle
        .metadata
        .get("source_reconnecting")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || handle
            .metadata
            .get("source_reconnect_reason")
            .and_then(Value::as_str)
            != Some(reason)
}

pub(crate) fn mark_source_reconnecting(runtime: &mut RuntimeHandle, reason: &str) {
    runtime.last_progress_at = Some(Utc::now());
    runtime.metadata["stream_online"] = json!(false);
    runtime.metadata["source_reconnecting"] = json!(true);
    runtime.metadata["source_reconnect_reason"] = json!(reason);
    if stream_ingest_recording_enabled_from_handle(runtime) && !recording_gap_active(runtime) {
        runtime.metadata["recording_gap_active"] = json!(true);
        runtime.metadata["recording_gap_reason"] = json!(reason);
        runtime.metadata["recording_gap_started_at"] = json!(Utc::now().to_rfc3339());
        runtime.metadata["recording_gap_ended_at"] = Value::Null;
    }
}

pub(crate) fn clear_source_reconnecting(runtime: &mut RuntimeHandle) {
    runtime.metadata["source_reconnecting"] = json!(false);
    runtime.metadata["source_reconnect_reason"] = Value::Null;
    runtime.metadata["startup_timeout"] = Value::Null;
    if recording_gap_active(runtime) {
        runtime.metadata["recording_gap_active"] = json!(false);
        runtime.metadata["recording_gap_ended_at"] = json!(Utc::now().to_rfc3339());
    }
}

pub(crate) fn emit_source_reconnecting_event(
    events: &RuntimeEventSink,
    handle: &RuntimeHandle,
    message: impl Into<String>,
    payload: Value,
) {
    let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
        task_id: handle.task_id,
        attempt_no: handle.attempt_no,
        lease_token: runtime_lease_token(handle).unwrap_or_default(),
        session_epoch: runtime_session_epoch(handle),
        event_type: "source_reconnecting".to_string(),
        event_level: "warn".to_string(),
        message: message.into(),
        payload,
    }));
}

pub(crate) fn emit_recording_gap_started_event(
    events: &RuntimeEventSink,
    handle: &RuntimeHandle,
    reason: &str,
    payload: Value,
) {
    let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
        task_id: handle.task_id,
        attempt_no: handle.attempt_no,
        lease_token: runtime_lease_token(handle).unwrap_or_default(),
        session_epoch: runtime_session_epoch(handle),
        event_type: "recording_gap_started".to_string(),
        event_level: "warn".to_string(),
        message: "stream recording gap started while source reconnects".to_string(),
        payload: merge_event_payload(
            payload,
            json!({
                "reason": reason,
                "recording_gap_started_at": handle.metadata.get("recording_gap_started_at").cloned().unwrap_or(Value::Null),
            }),
        ),
    }));
}

pub(crate) fn emit_recording_gap_ended_event(
    events: &RuntimeEventSink,
    handle: &RuntimeHandle,
    reason: &str,
    payload: Value,
) {
    if !recording_gap_active(handle) {
        return;
    }

    let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
        task_id: handle.task_id,
        attempt_no: handle.attempt_no,
        lease_token: runtime_lease_token(handle).unwrap_or_default(),
        session_epoch: runtime_session_epoch(handle),
        event_type: "recording_gap_ended".to_string(),
        event_level: "info".to_string(),
        message: "stream recording gap ended after source reconnected".to_string(),
        payload: merge_event_payload(
            payload,
            json!({
                "reason": reason,
                "recording_gap_started_at": handle.metadata.get("recording_gap_started_at").cloned().unwrap_or(Value::Null),
            }),
        ),
    }));
}

pub(crate) fn emit_recording_control_event(
    events: &RuntimeEventSink,
    handle: &RuntimeHandle,
    event_type: &str,
    event_level: &str,
    message: impl Into<String>,
    recording: &LiveRelayRecording,
    request: &TaskRecordingControlRequest,
    payload: Value,
) {
    let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
        task_id: handle.task_id,
        attempt_no: handle.attempt_no,
        lease_token: runtime_lease_token(handle).unwrap_or_default(),
        session_epoch: runtime_session_epoch(handle),
        event_type: event_type.to_string(),
        event_level: event_level.to_string(),
        message: message.into(),
        payload: merge_event_payload(
            payload,
            json!({
                "command_id": request.command_id,
                "manual_control": recording.manual_control,
                "desired_enabled": recording.desired_enabled,
                "formats": recording.formats,
                "root_path": recording.primary_root_path(),
                "root_paths": recording.root_paths_payload(),
                "duration_sec": recording.duration_sec,
                "segment_sec": recording.segment_sec,
                "as_player": recording.as_player,
                "stop_task_on_duration": recording.stop_task_on_duration,
                "reason": request.reason,
            }),
        ),
    }));
}

pub(crate) fn merge_event_payload(mut base: Value, extra: Value) -> Value {
    if let (Some(base), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            base.insert(key.clone(), value.clone());
        }
    }
    base
}

pub(crate) fn restart_request_from_handle(
    handle: &RuntimeHandle,
) -> Result<StartTaskRequest, ExecutorError> {
    Ok(StartTaskRequest {
        task_id: handle.task_id,
        attempt_no: handle.attempt_no,
        task_type: task_type_from_handle(handle).ok_or_else(|| {
            ExecutorError::InvalidRequest("persisted runtime is missing task_type".to_string())
        })?,
        resolved_spec: handle
            .metadata
            .get("resolved_spec")
            .cloned()
            .ok_or_else(|| {
                ExecutorError::InvalidRequest(
                    "persisted runtime is missing resolved_spec".to_string(),
                )
            })?,
        execution_mode: handle
            .metadata
            .get("execution_mode")
            .and_then(Value::as_str)
            .unwrap_or("managed")
            .to_string(),
        lease_token: handle
            .metadata
            .get("lease_token")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ExecutorError::InvalidRequest(
                    "persisted runtime is missing lease_token".to_string(),
                )
            })?
            .to_string(),
        trace_context: handle
            .metadata
            .get("trace_context")
            .and_then(Value::as_str)
            .map(str::to_string),
        session_epoch: runtime_session_epoch(handle),
    })
}

pub(crate) fn runtime_lease_token(handle: &RuntimeHandle) -> Option<String> {
    handle
        .metadata
        .get("lease_token")
        .and_then(Value::as_str)
        .map(str::to_string)
}

pub(crate) fn attach_zlm_server_id(metadata: &mut Value, zlm_server_id: Option<&str>) {
    let Some(server_id) = zlm_server_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return;
    };
    if let Some(map) = metadata.as_object_mut() {
        map.insert(
            "zlm_server_id".to_string(),
            Value::String(server_id.to_string()),
        );
    }
}

pub(crate) fn task_type_from_handle(handle: &RuntimeHandle) -> Option<TaskType> {
    handle
        .metadata
        .get("task_type")
        .and_then(Value::as_str)
        .and_then(|value| TaskType::from_str(value).ok())
}

pub(crate) fn managed_file_output_kind_from_handle(
    handle: &RuntimeHandle,
) -> Option<ManagedFileOutputKind> {
    handle
        .metadata
        .get("managed_file_output_kind")
        .cloned()
        .and_then(|value| serde_json::from_value::<ManagedFileOutputKind>(value).ok())
}

pub(crate) fn companion_recording_from_handle(
    handle: &RuntimeHandle,
) -> Option<CompanionProcessMetadata> {
    handle
        .metadata
        .get("companion_recording")
        .cloned()
        .and_then(|value| serde_json::from_value::<CompanionProcessMetadata>(value).ok())
}

pub(crate) fn companion_process_identity_from_metadata(
    companion: &CompanionProcessMetadata,
) -> Option<ProcessIdentity> {
    let pid = companion.pid?;
    Some(ProcessIdentity {
        pid,
        pgid: companion.pgid,
        pid_start_time: companion
            .pid_start_time
            .or_else(|| linux_pid_start_time(pid)),
    })
}

pub(crate) fn update_companion_recording_metadata(
    runtime: &mut RuntimeHandle,
    update: impl FnOnce(&mut CompanionProcessMetadata),
) {
    let Some(value) = runtime.metadata.get("companion_recording").cloned() else {
        return;
    };
    let Ok(mut companion) = serde_json::from_value::<CompanionProcessMetadata>(value) else {
        return;
    };
    update(&mut companion);
    runtime.metadata["companion_recording"] = json!(companion);
}

pub(crate) fn resolved_spec_from_handle(handle: &RuntimeHandle) -> Option<TaskSpec> {
    handle
        .metadata
        .get("resolved_spec")
        .cloned()
        .and_then(|value| serde_json::from_value::<TaskSpec>(value).ok())
}

pub(crate) fn task_runtime_mode_from_handle(handle: &RuntimeHandle) -> Option<TaskRuntimeMode> {
    resolved_spec_from_handle(handle).map(|spec| task_runtime_mode(&spec))
}

pub(crate) fn startup_probe_from_handle(handle: &RuntimeHandle) -> Option<StartupProbe> {
    handle
        .metadata
        .get("startup_probe")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
}
