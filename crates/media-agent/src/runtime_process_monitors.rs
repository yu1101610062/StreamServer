//! Runtime 辅助进程监控器：等待收养进程和伴随录制进程退出并回写状态。
//!
//! 这里负责“非当前任务主进程”的后台观察逻辑，包括伴随录制进程完成判定、
//! 收养 runtime 的退出判定、停止请求期间的长时间未退出日志，以及退出事件投递。

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use chrono::Utc;
use media_domain::RuntimeState;
use serde_json::json;
use tokio::time::sleep;
use tracing::warn;
use uuid::Uuid;

use crate::{
    runtime::{STOP_REQUESTED_STILL_RUNNING_LOG_INTERVAL, SuccessCheck},
    runtime_artifacts::attach_file_artifact_metadata,
    runtime_events::{RuntimeNotification, RuntimeTaskEvent, runtime_session_epoch},
    runtime_manager::{CompanionProcessExitedEvent, RuntimeInternalEvent, RuntimeMonitorHandle},
    runtime_metadata::{
        CompanionProcessMetadata, completion_reason_from_handle, runtime_lease_token,
    },
    runtime_plan::CompanionProcessPlan,
    runtime_process::{ProcessIdentity, is_process_running, is_process_running_for_command_line},
    runtime_recovery::classify_adopted_exit,
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
    monitor_handle: RuntimeMonitorHandle,
    mut child: tokio::process::Child,
) {
    tokio::spawn(async move {
        let status = child.wait().await;
        {
            let succeeded = match (&status, &companion_plan.success_check) {
                (Ok(status), SuccessCheck::FileExists(path)) => status.success() && path.exists(),
                (Ok(status), SuccessCheck::FilesExist(paths)) => {
                    status.success() && paths.iter().all(|path| path.exists())
                }
                (Ok(status), SuccessCheck::ProcessExit) => status.success(),
                (Err(_), _) => false,
            };
            let error = if succeeded {
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
            monitor_handle
                .send_event(RuntimeInternalEvent::CompanionProcessExited(
                    CompanionProcessExitedEvent {
                        runtime_id,
                        generation: monitor_handle.generation(),
                        companion_pid,
                        task_id,
                        attempt_no,
                        work_dir,
                        success_check,
                        succeeded,
                        error,
                        exit_payload: json!({
                            "output_target": companion_plan.output_target,
                            "exit_code": status.ok().and_then(|value| value.code()),
                            "reason": "recording_sidecar_exit_failed",
                        }),
                    },
                ))
                .await;
            return;
        }
    });
}

pub(crate) fn spawn_adopted_companion_process_monitor(
    runtime_id: Uuid,
    companion_process: ProcessIdentity,
    companion_plan: CompanionProcessMetadata,
    work_dir: PathBuf,
    success_check: SuccessCheck,
    monitor_handle: RuntimeMonitorHandle,
) {
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(2)).await;

            {
                let Some(snapshot) = monitor_handle.snapshot().await else {
                    return;
                };
                if is_process_running_for_command_line(
                    &companion_process,
                    companion_plan.command_line.as_deref(),
                ) {
                    continue;
                }

                let succeeded = companion_plan
                    .outputs
                    .iter()
                    .any(|output| Path::new(output).exists());
                monitor_handle
                    .send_event(RuntimeInternalEvent::CompanionProcessExited(
                        CompanionProcessExitedEvent {
                            runtime_id,
                            generation: monitor_handle.generation(),
                            companion_pid: companion_process.pid,
                            task_id: snapshot.handle.task_id,
                            attempt_no: snapshot.handle.attempt_no,
                            work_dir,
                            success_check,
                            succeeded,
                            error: if succeeded {
                                None
                            } else {
                                Some(
                                    "mp4 recording sidecar exited before artifact was finalized"
                                        .to_string(),
                                )
                            },
                            exit_payload: json!({
                                "output_target": companion_plan.output_target,
                                "reason": "recording_sidecar_exit_failed",
                                "orphaned": true,
                            }),
                        },
                    ))
                    .await;
                return;
            }
        }
    });
}

pub(crate) fn spawn_adopted_runtime_monitor(
    adopted_process: Option<ProcessIdentity>,
    work_dir: PathBuf,
    success_check: SuccessCheck,
    monitor_handle: RuntimeMonitorHandle,
) {
    tokio::spawn(async move {
        let mut stop_requested_wait_started_at: Option<Instant> = None;
        let mut last_stop_requested_running_log_at: Option<Instant> = None;
        loop {
            sleep(Duration::from_secs(2)).await;

            {
                let Some(snapshot) = monitor_handle.snapshot().await else {
                    return;
                };
                let Some(process) = adopted_process else {
                    return;
                };
                let pid = process.pid;
                let stop_requested = snapshot.stop_requested;
                let current_handle = snapshot.handle;
                if is_process_running_for_command_line(
                    &process,
                    current_handle.command_line.as_deref(),
                ) {
                    if stop_requested {
                        let waited_since =
                            stop_requested_wait_started_at.get_or_insert_with(Instant::now);
                        let should_log =
                            last_stop_requested_running_log_at.is_none_or(|logged_at| {
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

                let mut exited_handle = current_handle;
                exited_handle.state = RuntimeState::Exited;
                exited_handle.last_progress_at = Some(Utc::now());
                attach_file_artifact_metadata(&mut exited_handle, &success_check);

                let (event_type, event_level, message, payload) =
                    classify_adopted_exit(&exited_handle, &success_check, stop_requested);
                monitor_handle
                    .send_event(RuntimeInternalEvent::ApplyMonitorCommit(
                        crate::runtime_manager::RuntimeMonitorCommit::new(
                            exited_handle.clone(),
                            monitor_handle.generation(),
                        )
                        .with_persist(work_dir, success_check)
                        .with_notifications(vec![
                            RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                                task_id: exited_handle.task_id,
                                attempt_no: exited_handle.attempt_no,
                                lease_token: runtime_lease_token(&exited_handle)
                                    .unwrap_or_default(),
                                session_epoch: runtime_session_epoch(&exited_handle),
                                event_type: event_type.to_string(),
                                event_level: event_level.to_string(),
                                message,
                                payload,
                            }),
                            RuntimeNotification::TaskSnapshot(exited_handle),
                        ])
                        .terminal(),
                    ))
                    .await;
                return;
            }
        }
    });
}
