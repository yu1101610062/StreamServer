//! Runtime 辅助进程监控器：等待收养进程和伴随录制进程退出并回写状态。
//!
//! 这里负责“非当前任务主进程”的后台观察逻辑，包括伴随录制进程完成判定、
//! 收养 runtime 的退出判定、停止请求期间的长时间未退出日志，以及退出事件投递。

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, RwLock, atomic::Ordering},
    time::{Duration, Instant},
};

use chrono::Utc;
use media_domain::{RuntimeHandle, RuntimeState};
use serde_json::json;
use tokio::time::sleep;
use tracing::warn;
use uuid::Uuid;

use crate::{
    runtime::{STOP_REQUESTED_STILL_RUNNING_LOG_INTERVAL, SuccessCheck},
    runtime_artifacts::attach_file_artifact_metadata,
    runtime_events::{
        RuntimeEventSink, RuntimeNotification, RuntimeTaskEvent, runtime_session_epoch,
    },
    runtime_metadata::{
        CompanionProcessMetadata, CompanionProcessState, completion_reason_from_handle,
        runtime_lease_token, update_companion_recording_metadata,
    },
    runtime_persistence::persist_runtime_state,
    runtime_plan::CompanionProcessPlan,
    runtime_process::{
        ManagedRuntime, ProcessIdentity, is_process_running, is_process_running_for_command_line,
        remove_managed_runtime,
    },
    runtime_recovery::classify_adopted_exit,
    runtime_registry::LocalRuntimeRegistry,
};

pub(crate) async fn wait_for_companion_pids_exit(
    processes: &[ProcessIdentity],
    timeout_after_signal: Duration,
) {
    let started_at = Instant::now();
    loop {
        if processes.iter().all(|process| !is_process_running(process)) {
            return;
        }
        if started_at.elapsed() >= timeout_after_signal {
            return;
        }
        sleep(Duration::from_millis(100)).await;
    }
}

pub(crate) fn spawn_companion_process_monitor(
    runtime_id: Uuid,
    task_id: Uuid,
    attempt_no: i32,
    companion_pid: i32,
    companion_plan: CompanionProcessPlan,
    work_dir: PathBuf,
    success_check: SuccessCheck,
    registry: LocalRuntimeRegistry,
    runtimes: Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    events: RuntimeEventSink,
    mut child: tokio::process::Child,
) {
    tokio::spawn(async move {
        let status = child.wait().await;
        let (stop_requested, suppress_events) = {
            let mut runtimes_guard = runtimes.write().expect("runtime map lock poisoned");
            let Some(runtime) = runtimes_guard.get_mut(&runtime_id) else {
                return;
            };
            runtime
                .companion_processes
                .retain(|process| process.pid != companion_pid);
            (
                runtime.stop_requested.load(Ordering::Relaxed),
                runtime.suppress_companion_events.load(Ordering::Relaxed),
            )
        };

        let succeeded = match (&status, &companion_plan.success_check) {
            (Ok(status), SuccessCheck::FileExists(path)) => status.success() && path.exists(),
            (Ok(status), SuccessCheck::FilesExist(paths)) => {
                status.success() && paths.iter().all(|path| path.exists())
            }
            (Ok(status), SuccessCheck::ProcessExit) => status.success(),
            (Err(_), _) => false,
        };

        let updated_handle = registry.update(runtime_id, |runtime| {
            update_companion_recording_metadata(runtime, |companion| {
                companion.pid = None;
                companion.state = if succeeded {
                    CompanionProcessState::Succeeded
                } else {
                    CompanionProcessState::Failed
                };
                companion.error = if succeeded {
                    None
                } else {
                    Some(match &status {
                        Ok(status) => format!(
                            "mp4 recording sidecar exited unsuccessfully: {:?}",
                            status.code()
                        ),
                        Err(error) => format!("failed to wait mp4 recording sidecar: {error}"),
                    })
                };
            });
        });

        if let Some(handle) = updated_handle.as_ref() {
            let _ = persist_runtime_state(&work_dir, handle, &success_check);
        }

        if succeeded || stop_requested || suppress_events {
            return;
        }

        let Some(updated_handle) = updated_handle else {
            return;
        };
        let _ = events.send(RuntimeNotification::TaskSnapshot(updated_handle.clone()));
        let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
            task_id,
            attempt_no,
            lease_token: runtime_lease_token(&updated_handle).unwrap_or_default(),
            session_epoch: runtime_session_epoch(&updated_handle),
            event_type: "recording_degraded".to_string(),
            event_level: "warn".to_string(),
            message: "mp4 recording sidecar stopped; continuing without recording".to_string(),
            payload: json!({
                "output_target": companion_plan.output_target,
                "exit_code": status.ok().and_then(|value| value.code()),
                "reason": "recording_sidecar_exit_failed",
            }),
        }));
    });
}

pub(crate) fn spawn_adopted_companion_process_monitor(
    runtime_id: Uuid,
    companion_process: ProcessIdentity,
    companion_plan: CompanionProcessMetadata,
    work_dir: PathBuf,
    success_check: SuccessCheck,
    registry: LocalRuntimeRegistry,
    runtimes: Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    events: RuntimeEventSink,
) {
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(2)).await;

            let (stop_requested, suppress_events) = {
                let mut runtimes_guard = runtimes.write().expect("runtime map lock poisoned");
                let Some(runtime) = runtimes_guard.get_mut(&runtime_id) else {
                    return;
                };
                if is_process_running_for_command_line(
                    &companion_process,
                    companion_plan.command_line.as_deref(),
                ) {
                    continue;
                }
                runtime
                    .companion_processes
                    .retain(|process| process.pid != companion_process.pid);
                (
                    runtime.stop_requested.load(Ordering::Relaxed),
                    runtime.suppress_companion_events.load(Ordering::Relaxed),
                )
            };

            let succeeded = companion_plan
                .outputs
                .iter()
                .any(|output| Path::new(output).exists());
            let updated_handle = registry.update(runtime_id, |runtime| {
                update_companion_recording_metadata(runtime, |companion| {
                    companion.pid = None;
                    companion.state = if succeeded {
                        CompanionProcessState::Succeeded
                    } else {
                        CompanionProcessState::Failed
                    };
                    companion.error = if succeeded {
                        None
                    } else {
                        Some(
                            "mp4 recording sidecar exited before artifact was finalized"
                                .to_string(),
                        )
                    };
                });
            });

            if let Some(handle) = updated_handle.as_ref() {
                let _ = persist_runtime_state(&work_dir, handle, &success_check);
            }

            if succeeded || stop_requested || suppress_events {
                return;
            }

            let Some(updated_handle) = updated_handle else {
                return;
            };
            let _ = events.send(RuntimeNotification::TaskSnapshot(updated_handle.clone()));
            let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                task_id: updated_handle.task_id,
                attempt_no: updated_handle.attempt_no,
                lease_token: runtime_lease_token(&updated_handle).unwrap_or_default(),
                session_epoch: runtime_session_epoch(&updated_handle),
                event_type: "recording_degraded".to_string(),
                event_level: "warn".to_string(),
                message: "mp4 recording sidecar stopped; continuing without recording".to_string(),
                payload: json!({
                    "output_target": companion_plan.output_target,
                    "reason": "recording_sidecar_exit_failed",
                    "orphaned": true,
                }),
            }));
            return;
        }
    });
}

pub(crate) fn spawn_adopted_runtime_monitor(
    handle: RuntimeHandle,
    work_dir: PathBuf,
    success_check: SuccessCheck,
    registry: LocalRuntimeRegistry,
    runtimes: Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    events: RuntimeEventSink,
) {
    let runtime_id = handle.runtime_id;
    tokio::spawn(async move {
        let mut stop_requested_wait_started_at: Option<Instant> = None;
        let mut last_stop_requested_running_log_at: Option<Instant> = None;
        loop {
            sleep(Duration::from_secs(2)).await;

            let runtime = {
                runtimes
                    .read()
                    .expect("runtime map lock poisoned")
                    .get(&runtime_id)
                    .cloned()
            };
            let Some(runtime) = runtime else {
                return;
            };
            let Some(process) = runtime.process else {
                return;
            };
            let pid = process.pid;
            let stop_requested = runtime.stop_requested.load(Ordering::Relaxed);
            let current_handle = registry.get(runtime_id).unwrap_or_else(|| handle.clone());
            if is_process_running_for_command_line(&process, current_handle.command_line.as_deref())
            {
                if stop_requested {
                    let waited_since =
                        stop_requested_wait_started_at.get_or_insert_with(Instant::now);
                    let should_log = last_stop_requested_running_log_at.is_none_or(|logged_at| {
                        logged_at.elapsed() >= STOP_REQUESTED_STILL_RUNNING_LOG_INTERVAL
                    });
                    if should_log {
                        warn!(
                            task_id = %current_handle.task_id,
                            attempt_no = current_handle.attempt_no,
                            runtime_id = %current_handle.runtime_id,
                            pid,
                            state = ?current_handle.state,
                            completion_reason =
                                completion_reason_from_handle(&current_handle).unwrap_or_default(),
                            command_line = current_handle.command_line.as_deref().unwrap_or(""),
                            last_progress_at = ?current_handle.last_progress_at,
                            waited_for_exit_sec = waited_since.elapsed().as_secs_f64(),
                            "runtime stop requested but process is still running"
                        );
                        last_stop_requested_running_log_at = Some(Instant::now());
                    }
                } else {
                    stop_requested_wait_started_at = None;
                    last_stop_requested_running_log_at = None;
                }
                continue;
            }

            let _ = remove_managed_runtime(&runtimes, runtime_id);

            let mut exited_handle = registry
                .update(runtime_id, |runtime| {
                    runtime.state = RuntimeState::Exited;
                    runtime.last_progress_at = Some(Utc::now());
                })
                .unwrap_or_else(|| {
                    let mut handle = handle.clone();
                    handle.state = RuntimeState::Exited;
                    handle.last_progress_at = Some(Utc::now());
                    handle
                });
            attach_file_artifact_metadata(&mut exited_handle, &success_check);

            let (event_type, event_level, message, payload) =
                classify_adopted_exit(&exited_handle, &success_check, stop_requested);
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
            return;
        }
    });
}
