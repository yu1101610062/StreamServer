//! 运行时恢复判定与重启编排：集中处理自动重启、离线 grace 计数和收养进程退出结果归类。
//!
//! 这里既包含纯判定逻辑，也承接受管进程异常退出后的恢复动作：等待 ZLM API、
//! 清理重启前的旧 ZLM stream、重建本地进程 runtime，并延续断流/录制空洞 metadata。

use std::process::ExitStatus;

use media_domain::{RecoveryPolicy, RuntimeHandle, TaskType};
use reqwest::Client;
use serde_json::{Value, json};

use crate::{
    config::AgentSettings,
    runtime::{SuccessCheck, TaskRuntimeMode},
    runtime_events::{RuntimeNotification, RuntimeTaskEvent, runtime_session_epoch},
    runtime_metadata::managed_stream_restart_cleanup_binding,
    runtime_metadata::{
        completion_reason_from_handle, continuous_stream_ingest_from_handle,
        fatal_recording_error_from_handle, recovery_policy_from_handle, requires_stream_online,
        runtime_lease_token, sticky_reconnect_stream_ingest_from_handle, stop_reason_from_handle,
        stream_online, task_runtime_mode_from_handle, task_type_from_handle,
    },
    runtime_zlm::{build_close_stream_params, call_zlm_api},
};

pub(crate) const LIVE_STREAM_OFFLINE_GRACE_POLLS: u32 = 3;
pub(crate) const RTP_SERVER_MISSING_GRACE_POLLS: u32 = 3;

pub(crate) struct ProcessRecoveryContext<'a> {
    pub(crate) settings: &'a AgentSettings,
    pub(crate) http_client: &'a Client,
}

pub(crate) async fn cleanup_managed_stream_before_restart_notifications(
    ctx: ProcessRecoveryContext<'_>,
    handle: &RuntimeHandle,
) -> Vec<RuntimeNotification> {
    let Some(binding) = managed_stream_restart_cleanup_binding(handle) else {
        return Vec::new();
    };

    let notification = match call_zlm_api(
        ctx.http_client,
        ctx.settings,
        "/index/api/close_streams",
        &build_close_stream_params(&binding, true),
    )
    .await
    {
        Ok(_) => RuntimeNotification::TaskEvent(RuntimeTaskEvent {
            task_id: handle.task_id,
            attempt_no: handle.attempt_no,
            lease_token: runtime_lease_token(handle).unwrap_or_default(),
            session_epoch: runtime_session_epoch(handle),
            event_type: "stream_cleanup".to_string(),
            event_level: "info".to_string(),
            message: "closed stale ZLM stream before managed process restart".to_string(),
            payload: json!({
                "schema": binding.schema,
                "vhost": binding.vhost,
                "app": binding.app,
                "stream": binding.stream,
                "reason": "managed_process_restart",
            }),
        }),
        Err(error) => RuntimeNotification::TaskEvent(RuntimeTaskEvent {
            task_id: handle.task_id,
            attempt_no: handle.attempt_no,
            lease_token: runtime_lease_token(handle).unwrap_or_default(),
            session_epoch: runtime_session_epoch(handle),
            event_type: "zlm_api_error".to_string(),
            event_level: "warn".to_string(),
            message: format!(
                "failed to close stale ZLM stream before managed process restart: {error}"
            ),
            payload: json!({
                "schema": binding.schema,
                "vhost": binding.vhost,
                "app": binding.app,
                "stream": binding.stream,
                "reason": "managed_process_restart",
            }),
        }),
    };
    vec![notification]
}

pub(crate) fn should_auto_restart_process(
    handle: &RuntimeHandle,
    was_stopped: bool,
    status: &Result<ExitStatus, std::io::Error>,
) -> bool {
    let sticky_reconnect = sticky_reconnect_stream_ingest_from_handle(handle);
    if was_stopped
        || task_type_from_handle(handle) != Some(TaskType::StreamIngest)
        || task_runtime_mode_from_handle(handle) != Some(TaskRuntimeMode::ManagedProcess)
        || (!continuous_stream_ingest_from_handle(handle) && !sticky_reconnect)
        || (!sticky_reconnect && !stream_online(handle))
        || fatal_recording_error_from_handle(handle).is_some()
    {
        return false;
    }

    if !matches!(
        recovery_policy_from_handle(handle),
        Some(RecoveryPolicy::Auto)
    ) {
        return false;
    }

    should_restart_continuous_stream_ingest(status)
}

pub(crate) fn should_restart_continuous_stream_ingest(
    status: &Result<ExitStatus, std::io::Error>,
) -> bool {
    match status {
        Ok(_) => true,
        Err(_) => true,
    }
}

pub(crate) fn next_live_relay_offline_polls(
    current: u32,
    stream_was_online: bool,
    stream_state: Result<bool, ()>,
) -> (u32, bool) {
    match stream_state {
        Ok(true) => (0, false),
        Ok(false) if stream_was_online => {
            let next = current.saturating_add(1);
            (next, next >= LIVE_STREAM_OFFLINE_GRACE_POLLS)
        }
        Ok(false) | Err(()) => (0, false),
    }
}

pub(crate) fn next_rtp_server_missing_polls(
    current: u32,
    server_present: Result<bool, ()>,
) -> (u32, bool) {
    match server_present {
        Ok(true) => (0, false),
        Ok(false) => {
            let next = current.saturating_add(1);
            (next, next >= RTP_SERVER_MISSING_GRACE_POLLS)
        }
        Err(()) => (0, false),
    }
}

pub(crate) fn classify_adopted_exit(
    handle: &RuntimeHandle,
    success_check: &SuccessCheck,
    stop_requested: bool,
) -> (&'static str, &'static str, String, Value) {
    let output_target = handle.outputs.first().cloned().unwrap_or_default();
    if let Some(reason) =
        completion_reason_from_handle(handle).filter(|reason| reason == "record_duration_reached")
    {
        return (
            "succeeded",
            "info",
            "adopted child process completed after recording duration reached".to_string(),
            json!({
                "output_target": output_target,
                "orphaned": true,
                "reason": reason,
            }),
        );
    }
    if let Some(error) = fatal_recording_error_from_handle(handle) {
        return (
            "failed",
            "error",
            format!("adopted child process stopped after recording startup failed: {error}"),
            json!({
                "output_target": output_target,
                "orphaned": true,
                "recording_error": error,
            }),
        );
    }
    if stop_requested {
        if stop_reason_from_handle(handle).as_deref() == Some("disk_threshold_exceeded") {
            return (
                "failed",
                "error",
                "adopted child process stopped after disk threshold was exceeded".to_string(),
                json!({
                    "output_target": output_target,
                    "orphaned": true,
                    "reason": "disk_threshold_exceeded",
                }),
            );
        }
        return (
            "canceled",
            "info",
            "adopted child process stopped".to_string(),
            json!({
                "output_target": output_target,
                "orphaned": true,
            }),
        );
    }

    match success_check {
        _ if requires_stream_online(handle) && !stream_online(handle) => (
            "failed",
            "error",
            "adopted child process exited before ZLM stream became online".to_string(),
            json!({
                "output_target": output_target,
                "orphaned": true,
            }),
        ),
        SuccessCheck::FileExists(path) if path.exists() => (
            "succeeded",
            "info",
            "adopted child process completed".to_string(),
            json!({
                "output_target": output_target,
                "orphaned": true,
            }),
        ),
        SuccessCheck::FileExists(path) => (
            "failed",
            "error",
            format!(
                "adopted child process exited without artifact: {}",
                path.display()
            ),
            json!({
                "output_target": output_target,
                "orphaned": true,
            }),
        ),
        SuccessCheck::FilesExist(paths) if paths.iter().all(|path| path.exists()) => (
            "succeeded",
            "info",
            "adopted child process completed".to_string(),
            json!({
                "output_target": output_target,
                "orphaned": true,
            }),
        ),
        SuccessCheck::FilesExist(paths) => {
            let missing = paths
                .iter()
                .filter(|path| !path.exists())
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>();
            (
                "failed",
                "error",
                format!(
                    "adopted child process exited without artifacts: {}",
                    missing.join(", ")
                ),
                json!({
                    "output_target": output_target,
                    "orphaned": true,
                    "missing_outputs": missing,
                }),
            )
        }
        SuccessCheck::ProcessExit => match task_type_from_handle(handle) {
            Some(TaskType::StreamIngest)
                if task_runtime_mode_from_handle(handle)
                    == Some(TaskRuntimeMode::ManagedProcess) =>
            {
                if continuous_stream_ingest_from_handle(handle) {
                    (
                        "failed",
                        "error",
                        "adopted continuous stream_ingest process exited unexpectedly".to_string(),
                        json!({
                            "output_target": output_target,
                            "orphaned": true,
                            "reason": "unexpected_stream_exit",
                        }),
                    )
                } else {
                    (
                        "succeeded",
                        "info",
                        "adopted stream_ingest process completed".to_string(),
                        json!({
                            "output_target": output_target,
                            "orphaned": true,
                        }),
                    )
                }
            }
            _ => (
                "failed",
                "error",
                "adopted child process disappeared without exit status".to_string(),
                json!({
                    "output_target": output_target,
                    "orphaned": true,
                }),
            ),
        },
    }
}
