//! Runtime 主进程退出监控：处理受管 FFmpeg/进程结束后的清理、事件归类和本地恢复。
//!
//! 这个模块只关注“主进程已经启动并最终退出”之后的收尾路径，包括伴随录制进程
//! 终止、输出 artifact 校验、持续流任务自动重启，以及最终 runtime 事件和快照投递。

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use chrono::Utc;
use media_domain::{RuntimeHandle, RuntimeState, TaskType};
use serde_json::json;
use uuid::Uuid;

use crate::{
    runtime::{SuccessCheck, TaskRuntimeMode},
    runtime_artifacts::attach_file_artifact_metadata,
    runtime_events::{
        RuntimeEventSink, RuntimeNotification, RuntimeTaskEvent, runtime_session_epoch,
    },
    runtime_executor::ManagedProcessExecutor,
    runtime_manager::{ProcessExitedEvent, RuntimeInternalEvent, RuntimeMonitorHandle},
    runtime_metadata::{
        completion_reason_from_handle, continuous_stream_ingest_from_handle,
        emit_recording_gap_started_event, emit_source_reconnecting_event,
        fatal_recording_error_from_handle, mark_source_reconnecting, requires_stream_online,
        runtime_lease_token, should_emit_recording_gap_started,
        sticky_reconnect_stream_ingest_from_handle, stop_reason_from_handle, stream_online,
        task_runtime_mode_from_handle, task_type_from_handle,
    },
    runtime_persistence::persist_runtime_state,
    runtime_process::{ManagedRuntime, is_process_running, remove_managed_runtime, signal_process},
    runtime_process_monitors::wait_for_companion_pids_exit,
    runtime_recovery::should_auto_restart_process,
    runtime_registry::LocalRuntimeRegistry,
};

pub(crate) struct ProcessExitMonitorContext {
    pub(crate) runtime_id: Uuid,
    pub(crate) wait_handle: RuntimeHandle,
    pub(crate) work_dir: PathBuf,
    pub(crate) output_target: String,
    pub(crate) success_check: SuccessCheck,
    pub(crate) stop_requested: Arc<AtomicBool>,
    pub(crate) registry: LocalRuntimeRegistry,
    pub(crate) runtimes: Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    pub(crate) events: RuntimeEventSink,
    pub(crate) restart_executor: ManagedProcessExecutor,
    pub(crate) monitor_handle: Option<RuntimeMonitorHandle>,
}

pub(crate) fn spawn_process_exit_monitor(
    context: ProcessExitMonitorContext,
    mut child: tokio::process::Child,
) {
    tokio::spawn(async move {
        let ProcessExitMonitorContext {
            runtime_id,
            wait_handle,
            work_dir,
            output_target,
            success_check,
            stop_requested,
            registry,
            runtimes,
            events,
            restart_executor,
            monitor_handle,
        } = context;

        let status = child.wait().await;
        if let Some(monitor_handle) = monitor_handle {
            let Some(snapshot) = monitor_handle.snapshot().await else {
                return;
            };
            let was_stopped = snapshot.stop_requested || stop_requested.load(Ordering::Relaxed);
            if !snapshot.companion_processes.is_empty() {
                for companion_process in &snapshot.companion_processes {
                    if is_process_running(companion_process) {
                        let _ = signal_process(companion_process, libc::SIGTERM);
                    }
                }
                wait_for_companion_pids_exit(&snapshot.companion_processes, Duration::from_secs(3))
                    .await;
                for companion_process in &snapshot.companion_processes {
                    if is_process_running(companion_process) {
                        let _ = signal_process(companion_process, libc::SIGKILL);
                    }
                }
            }
            monitor_handle
                .send_event(RuntimeInternalEvent::ProcessExited(ProcessExitedEvent {
                    runtime_id,
                    generation: monitor_handle.generation(),
                    work_dir,
                    output_target,
                    success_check,
                    status: status.map_err(|error| error.to_string()),
                    was_stopped,
                }))
                .await;
            return;
        }

        let (was_stopped, companion_processes) = {
            let mut runtimes_guard = runtimes.write().expect("runtime map lock poisoned");
            if let Some(runtime) = runtimes_guard.get_mut(&runtime_id) {
                runtime
                    .suppress_companion_events
                    .store(true, Ordering::Relaxed);
                let was_stopped = runtime.stop_requested.load(Ordering::Relaxed);
                let companion_processes = runtime.companion_processes.clone();
                (was_stopped, companion_processes)
            } else {
                (stop_requested.load(Ordering::Relaxed), Vec::new())
            }
        };
        if !companion_processes.is_empty() {
            for companion_process in &companion_processes {
                if is_process_running(companion_process) {
                    let _ = signal_process(companion_process, libc::SIGTERM);
                }
            }
            wait_for_companion_pids_exit(&companion_processes, Duration::from_secs(3)).await;
            for companion_process in &companion_processes {
                if is_process_running(companion_process) {
                    let _ = signal_process(companion_process, libc::SIGKILL);
                }
            }
        }
        let _ = remove_managed_runtime(&runtimes, runtime_id);

        let mut exited_handle = registry
            .update(runtime_id, |runtime| {
                runtime.state = RuntimeState::Exited;
                runtime.last_progress_at = Some(Utc::now());
            })
            .unwrap_or_else(|| RuntimeHandle {
                runtime_id,
                task_id: wait_handle.task_id,
                attempt_no: wait_handle.attempt_no,
                worker_kind: wait_handle.worker_kind,
                pid: wait_handle.pid,
                started_at: wait_handle.started_at,
                last_progress_at: Some(Utc::now()),
                state: RuntimeState::Exited,
                command_line: wait_handle.command_line.clone(),
                outputs: wait_handle.outputs.clone(),
                metadata: wait_handle.metadata.clone(),
            });

        attach_file_artifact_metadata(&mut exited_handle, &success_check);

        if should_auto_restart_process(&exited_handle, was_stopped, &status) {
            let sticky_reconnect = sticky_reconnect_stream_ingest_from_handle(&exited_handle);
            let restart_reason = if stream_online(&exited_handle) {
                "source_disconnected"
            } else {
                "source_unavailable"
            };
            let emit_gap_started = should_emit_recording_gap_started(&exited_handle);
            if sticky_reconnect {
                mark_source_reconnecting(&mut exited_handle, restart_reason);
            }
            let _ = persist_runtime_state(&work_dir, &exited_handle, &success_check);
            if sticky_reconnect {
                emit_source_reconnecting_event(
                    &events,
                    &exited_handle,
                    "managed stream_ingest process exited; restarting locally",
                    json!({
                        "runtime_id": exited_handle.runtime_id,
                        "exit_code": status.as_ref().ok().and_then(|value| value.code()),
                        "output_target": output_target,
                        "task_type": task_type_from_handle(&exited_handle),
                        "reason": restart_reason,
                    }),
                );
                if emit_gap_started {
                    emit_recording_gap_started_event(
                        &events,
                        &exited_handle,
                        restart_reason,
                        json!({
                            "runtime_id": exited_handle.runtime_id,
                            "exit_code": status.as_ref().ok().and_then(|value| value.code()),
                            "output_target": output_target,
                            "task_type": task_type_from_handle(&exited_handle),
                        }),
                    );
                }
            } else {
                let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                    task_id: exited_handle.task_id,
                    attempt_no: exited_handle.attempt_no,
                    lease_token: runtime_lease_token(&exited_handle).unwrap_or_default(),
                    session_epoch: runtime_session_epoch(&exited_handle),
                    event_type: "recovering".to_string(),
                    event_level: "warn".to_string(),
                    message:
                        "managed process exited after stream was online; attempting local recovery"
                            .to_string(),
                    payload: json!({
                        "exit_code": status.as_ref().ok().and_then(|value| value.code()),
                        "output_target": output_target,
                        "task_type": task_type_from_handle(&exited_handle),
                    }),
                }));
            }
            restart_executor
                .cleanup_managed_stream_before_restart(&exited_handle)
                .await;
            let _ = registry.remove(runtime_id);

            if restart_executor
                .restart_process_task_after_failure(&exited_handle, !sticky_reconnect)
                .await
                .is_ok()
            {
                return;
            }
        }

        let completion_reason = completion_reason_from_handle(&exited_handle);
        let stop_reason = stop_reason_from_handle(&exited_handle);
        let fatal_recording_error = fatal_recording_error_from_handle(&exited_handle);
        let (event_type, event_level, message, payload) = match status {
            Ok(status)
                if was_stopped
                    && completion_reason.as_deref() == Some("record_duration_reached") =>
            {
                (
                    "succeeded",
                    "info",
                    "child process completed after recording duration reached".to_string(),
                    json!({
                        "exit_code": status.code(),
                        "output_target": output_target,
                        "reason": "record_duration_reached",
                    }),
                )
            }
            Ok(status)
                if was_stopped && stop_reason.as_deref() == Some("disk_threshold_exceeded") =>
            {
                (
                    "failed",
                    "error",
                    "child process stopped after disk threshold was exceeded".to_string(),
                    json!({
                        "exit_code": status.code(),
                        "output_target": output_target,
                        "reason": "disk_threshold_exceeded",
                    }),
                )
            }
            Ok(status) if was_stopped => (
                "canceled",
                "info",
                "child process stopped".to_string(),
                json!({
                    "exit_code": status.code(),
                    "output_target": output_target,
                    "reason": stop_reason,
                }),
            ),
            Ok(status) if fatal_recording_error.is_some() => (
                "failed",
                "error",
                format!(
                    "child process stopped after recording startup failed: {}",
                    fatal_recording_error
                        .as_deref()
                        .unwrap_or("unknown recording error")
                ),
                json!({
                    "exit_code": status.code(),
                    "output_target": output_target,
                    "recording_error": fatal_recording_error,
                }),
            ),
            Ok(status)
                if status.success()
                    && requires_stream_online(&exited_handle)
                    && !stream_online(&exited_handle) =>
            {
                (
                    "failed",
                    "error",
                    "child process exited before ZLM stream became online".to_string(),
                    json!({
                        "exit_code": status.code(),
                        "output_target": output_target,
                    }),
                )
            }
            Ok(status)
                if status.success()
                    && task_type_from_handle(&exited_handle) == Some(TaskType::StreamIngest)
                    && task_runtime_mode_from_handle(&exited_handle)
                        == Some(TaskRuntimeMode::ManagedProcess)
                    && continuous_stream_ingest_from_handle(&exited_handle) =>
            {
                (
                    "failed",
                    "error",
                    "continuous stream_ingest process exited unexpectedly".to_string(),
                    json!({
                        "exit_code": status.code(),
                        "output_target": output_target,
                        "reason": "unexpected_stream_exit",
                    }),
                )
            }
            Ok(status) if status.success() => match &success_check {
                SuccessCheck::FileExists(path) if path.exists() => (
                    "succeeded",
                    "info",
                    "child process completed".to_string(),
                    json!({
                        "exit_code": status.code(),
                        "output_target": output_target,
                    }),
                ),
                SuccessCheck::FileExists(path) => (
                    "failed",
                    "error",
                    format!(
                        "child process finished without artifact: {}",
                        path.display()
                    ),
                    json!({
                        "exit_code": status.code(),
                        "output_target": output_target,
                    }),
                ),
                SuccessCheck::FilesExist(paths) if paths.iter().all(|path| path.exists()) => (
                    "succeeded",
                    "info",
                    "child process completed".to_string(),
                    json!({
                        "exit_code": status.code(),
                        "output_target": output_target,
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
                            "child process finished without artifacts: {}",
                            missing.join(", ")
                        ),
                        json!({
                            "exit_code": status.code(),
                            "output_target": output_target,
                            "missing_outputs": missing,
                        }),
                    )
                }
                SuccessCheck::ProcessExit => (
                    "succeeded",
                    "info",
                    "child process completed".to_string(),
                    json!({
                        "exit_code": status.code(),
                        "output_target": output_target,
                    }),
                ),
            },
            Ok(status) => (
                "failed",
                "error",
                format!("child process exited unsuccessfully: {:?}", status.code()),
                json!({
                    "exit_code": status.code(),
                    "output_target": output_target,
                }),
            ),
            Err(error) if fatal_recording_error.is_some() => (
                "failed",
                "error",
                format!(
                    "child process stopped after recording startup failed: {}",
                    fatal_recording_error
                        .as_deref()
                        .unwrap_or("unknown recording error")
                ),
                json!({
                    "output_target": output_target,
                    "recording_error": fatal_recording_error,
                    "wait_error": error.to_string(),
                }),
            ),
            Err(error)
                if was_stopped && stop_reason.as_deref() == Some("disk_threshold_exceeded") =>
            {
                (
                    "failed",
                    "error",
                    format!("failed to wait child process after disk threshold stop: {error}"),
                    json!({
                        "output_target": output_target,
                        "reason": "disk_threshold_exceeded",
                        "wait_error": error.to_string(),
                    }),
                )
            }
            Err(error) => (
                "failed",
                "error",
                format!("failed to wait child process: {error}"),
                json!({
                    "output_target": output_target,
                }),
            ),
        };

        let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
            task_id: exited_handle.task_id,
            attempt_no: exited_handle.attempt_no,
            lease_token: runtime_lease_token(&exited_handle).unwrap_or_default(),
            session_epoch: runtime_session_epoch(&exited_handle),
            event_type: event_type.to_string(),
            event_level: event_level.to_string(),
            message,
            payload,
        }));
        let _ = persist_runtime_state(&work_dir, &exited_handle, &success_check);
        let _ = events.send(RuntimeNotification::TaskSnapshot(exited_handle.clone()));

        let _ = registry.remove(runtime_id);
    });
}
