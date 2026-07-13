//! Runtime 停止与旧 attempt 清理：处理显式停止请求和同任务旧运行实例淘汰。
//!
//! 这里集中维护停止意图记录、停止状态持久化、ZLM/RTP 关闭、进程信号发送以及
//! stale attempt 的延迟强杀调度，避免这些终止路径继续堆在 executor 主实现里。

use std::{
    collections::HashMap,
    sync::{Arc, RwLock, atomic::Ordering},
    time::Duration,
};

use chrono::Utc;
use media_domain::{RuntimeHandle, RuntimeState};
use serde_json::json;
use tracing::warn;
use uuid::Uuid;

use crate::{
    config::AgentSettings,
    runtime::{ExecutorError, LiveRelayRecording, StartTaskRequest, StopTaskRequest},
    runtime_controls::{
        RuntimeControlContext, close_live_relay, close_rtp_receive,
        stop_live_relay_recording_for_handle,
    },
    runtime_events::{RuntimeNotification, RuntimeTaskEvent, runtime_session_epoch},
    runtime_io::attempt_work_dir,
    runtime_live_relay_events::{
        LiveRelayEventStream, live_relay_stopped_terminal_event, live_relay_terminal_notification,
    },
    runtime_manager::{
        RuntimeBackendStore, RuntimeGeneration, RuntimeMonitorCommit, RuntimeMonitorHandle,
    },
    runtime_metadata::{
        clear_source_reconnecting, companion_process_identity_from_metadata,
        companion_recording_from_handle, process_identity_from_handle, rtp_stream_id_from_handle,
        runtime_lease_token, stream_binding_from_handle, task_runtime_mode_from_handle,
    },
    runtime_persistence::{scan_persisted_runtimes, success_check_from_handle},
    runtime_plan::TaskRuntimeMode,
    runtime_process::{
        ManagedRuntime, ProcessIdentity, is_process_running_for_command_line, runtime_processes,
        schedule_force_kill_processes_if_running, schedule_force_kill_with_monitor_if_running,
        signal_process, signal_runtime_processes,
    },
    runtime_recording::mark_recording_completion,
};

const STALE_ATTEMPT_FORCE_KILL_DELAY: Duration = Duration::from_secs(1);

pub(crate) struct StaleAttemptCleanupContext<'a> {
    pub(crate) settings: &'a AgentSettings,
    pub(crate) active_handles: Vec<RuntimeHandle>,
    pub(crate) backend_store: RuntimeBackendStore,
}

pub(crate) fn cleanup_stale_attempt_runtimes(
    ctx: StaleAttemptCleanupContext<'_>,
    request: &StartTaskRequest,
) -> Vec<RuntimeHandle> {
    let mut stopping_updates = Vec::new();
    let mut handled_attempts = std::collections::HashSet::new();
    for handle in ctx.active_handles {
        if !is_stale_attempt_for_request(&handle, request) {
            continue;
        }
        handled_attempts.insert((handle.task_id, handle.attempt_no));

        let runtime = ctx.backend_store.get(handle.runtime_id);
        if let Some(runtime) = runtime {
            runtime.stop_requested.store(true, Ordering::Relaxed);
            let processes = runtime_processes(&runtime);
            if processes.is_empty() {
                continue;
            }
            let mut stopping_handle = handle.clone();
            stopping_handle.state = RuntimeState::Stopping;
            stopping_handle.last_progress_at = Some(Utc::now());
            stopping_handle.metadata["stop"] = json!({
                "reason": "stale_attempt_replaced",
                "replacement_attempt_no": request.attempt_no,
            });
            stopping_updates.push(stopping_handle);
            for process in &processes {
                if let Err(error) = signal_process(process, libc::SIGTERM) {
                    warn!(
                        pid = process.pid,
                        pgid = ?process.pgid,
                        error = %error,
                        reason = "stale_attempt_replaced",
                        "failed to signal stale runtime process"
                    );
                }
            }
            schedule_force_kill_processes_if_running(
                processes,
                STALE_ATTEMPT_FORCE_KILL_DELAY,
                "stale_attempt_replaced",
            );
            continue;
        }

        let processes = runtime_handle_live_processes(&handle);
        signal_stale_processes(&processes, "stale_registry_attempt_replaced");
    }

    for persisted in scan_persisted_runtimes(&ctx.settings.work_root) {
        if handled_attempts.contains(&(persisted.handle.task_id, persisted.handle.attempt_no))
            || !is_stale_attempt_for_request(&persisted.handle, request)
        {
            continue;
        }
        let processes = runtime_handle_live_processes(&persisted.handle);
        signal_stale_processes(&processes, "stale_persisted_attempt_replaced");
    }
    stopping_updates
}

pub(crate) enum RuntimeStopPreparation {
    Worker {
        commit: RuntimeMonitorCommit,
        worker: RuntimeStopWorkerRequest,
    },
    AlreadyGone(RuntimeMonitorCommit),
}

#[derive(Clone)]
pub(crate) struct RuntimeStopWorkerRequest {
    pub(crate) request: StopTaskRequest,
    pub(crate) handle: RuntimeHandle,
    pub(crate) stopping_handle: RuntimeHandle,
    pub(crate) generation: RuntimeGeneration,
    pub(crate) runtime: ManagedRuntime,
    pub(crate) monitor_handle: RuntimeMonitorHandle,
}

#[derive(Debug)]
pub(crate) enum RuntimeStopOutcome {
    ManagedProcessStopAccepted,
    Terminal(RuntimeMonitorCommit),
    AlreadyGone(RuntimeMonitorCommit),
}

pub(crate) fn prepare_runtime_stop_for_manager(
    settings: &AgentSettings,
    stop_intents: &Arc<RwLock<HashMap<(Uuid, i32), StopTaskRequest>>>,
    runtime: Option<ManagedRuntime>,
    request: &StopTaskRequest,
    handle: &RuntimeHandle,
    generation: RuntimeGeneration,
    monitor_handle: RuntimeMonitorHandle,
) -> Result<RuntimeStopPreparation, ExecutorError> {
    let key = (request.task_id, request.attempt_no);
    {
        let mut stop_intents = stop_intents.write().expect("stop intents lock poisoned");
        stop_intents.insert(key, request.clone());
    }

    let Some(runtime) = runtime else {
        return Ok(RuntimeStopPreparation::AlreadyGone(
            runtime_already_gone_commit(settings, handle, generation, request),
        ));
    };

    let stopping_handle = stopping_handle_for_request(handle, request);
    let mut commit = RuntimeMonitorCommit::new(stopping_handle.clone(), generation).with_persist(
        attempt_work_dir(settings, request.task_id, request.attempt_no),
        success_check_from_handle(&stopping_handle),
    );
    commit.mark_stop_requested = Some(true);
    Ok(RuntimeStopPreparation::Worker {
        commit,
        worker: RuntimeStopWorkerRequest {
            request: request.clone(),
            handle: handle.clone(),
            stopping_handle,
            generation,
            runtime,
            monitor_handle,
        },
    })
}

pub(crate) async fn run_runtime_stop_worker(
    controls: RuntimeControlContext<'_>,
    worker: RuntimeStopWorkerRequest,
) -> Result<RuntimeStopOutcome, ExecutorError> {
    if worker.runtime.process.is_some() {
        let managed_live_relay = matches!(
            task_runtime_mode_from_handle(&worker.handle),
            Some(TaskRuntimeMode::ManagedProcess)
        ) && stream_binding_from_handle(&worker.handle).is_some();
        if managed_live_relay {
            stop_live_relay_recording_for_handle(&controls, &worker.handle).await?;
        }
        signal_runtime_processes(&worker.runtime, libc::SIGTERM)?;
        if managed_live_relay {
            close_live_relay(&controls, &worker.handle, true).await?;
        }
        if worker.request.force_after_sec > 0 {
            schedule_force_kill_with_monitor_if_running(
                worker.monitor_handle,
                runtime_processes(&worker.runtime),
                Duration::from_secs(worker.request.force_after_sec as u64),
                "stop_task_force_after",
            );
        }
        return Ok(RuntimeStopOutcome::ManagedProcessStopAccepted);
    }

    match task_runtime_mode_from_handle(&worker.handle) {
        Some(TaskRuntimeMode::ZlmProxy) => {
            stop_live_relay_recording_for_handle(&controls, &worker.handle).await?;
            close_live_relay(&controls, &worker.handle, true).await?;
            Ok(RuntimeStopOutcome::Terminal(
                live_relay_stop_terminal_commit(controls.settings, &worker),
            ))
        }
        Some(TaskRuntimeMode::ZlmRtpServer) => {
            close_rtp_receive(&controls, &worker.stopping_handle).await?;
            Ok(RuntimeStopOutcome::Terminal(rtp_stop_terminal_commit(
                controls.settings,
                &worker,
            )))
        }
        _ => Ok(RuntimeStopOutcome::AlreadyGone(
            runtime_already_gone_commit(
                controls.settings,
                &worker.stopping_handle,
                worker.generation,
                &worker.request,
            ),
        )),
    }
}

fn stopping_handle_for_request(handle: &RuntimeHandle, request: &StopTaskRequest) -> RuntimeHandle {
    let mut stopping_handle = handle.clone();
    stopping_handle.state = RuntimeState::Stopping;
    stopping_handle.last_progress_at = Some(Utc::now());
    stopping_handle.metadata["stop"] = json!({
        "reason": request.reason,
        "grace_period_sec": request.grace_period_sec,
        "force_after_sec": request.force_after_sec,
    });
    if let Some(mut recording) = stopping_handle
        .metadata
        .get("recording")
        .cloned()
        .and_then(|value| serde_json::from_value::<LiveRelayRecording>(value).ok())
    {
        let recording = if request.reason == "record_duration_reached" {
            stopping_handle.metadata["completion_reason"] = json!("record_duration_reached");
            mark_recording_completion(&recording, "record_duration_reached")
        } else {
            recording.started = false;
            recording
        };
        stopping_handle.metadata["recording"] = json!(recording);
    }
    stopping_handle
}

fn live_relay_stop_terminal_commit(
    settings: &AgentSettings,
    worker: &RuntimeStopWorkerRequest,
) -> RuntimeMonitorCommit {
    let mut exited_handle = worker.stopping_handle.clone();
    exited_handle.state = RuntimeState::Exited;
    exited_handle.last_progress_at = Some(Utc::now());
    exited_handle.metadata["stream_online"] = json!(false);
    clear_source_reconnecting(&mut exited_handle);
    let mut notifications = Vec::new();
    if let Some(binding) = stream_binding_from_handle(&worker.handle) {
        notifications.push(live_relay_terminal_notification(
            &exited_handle,
            LiveRelayEventStream::from(&binding),
            live_relay_stopped_terminal_event(&exited_handle),
            false,
        ));
    }
    notifications.push(RuntimeNotification::TaskSnapshot(exited_handle.clone()));
    RuntimeMonitorCommit::new(exited_handle.clone(), worker.generation)
        .with_persist(
            attempt_work_dir(settings, worker.request.task_id, worker.request.attempt_no),
            success_check_from_handle(&exited_handle),
        )
        .with_notifications(notifications)
        .terminal()
}

fn rtp_stop_terminal_commit(
    settings: &AgentSettings,
    worker: &RuntimeStopWorkerRequest,
) -> RuntimeMonitorCommit {
    let mut exited_handle = worker.stopping_handle.clone();
    exited_handle.state = RuntimeState::Exited;
    exited_handle.last_progress_at = Some(Utc::now());
    exited_handle.metadata["stream_online"] = json!(false);
    let (event_type, event_level, message) = if worker.request.reason == "disk_threshold_exceeded" {
        (
            "failed",
            "error",
            "stream_ingest rtp server stopped after disk threshold was exceeded",
        )
    } else {
        ("canceled", "info", "stream_ingest rtp server stopped")
    };
    RuntimeMonitorCommit::new(exited_handle.clone(), worker.generation)
        .with_persist(
            attempt_work_dir(settings, worker.request.task_id, worker.request.attempt_no),
            success_check_from_handle(&exited_handle),
        )
        .with_notifications(vec![
            RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                task_id: exited_handle.task_id,
                attempt_no: exited_handle.attempt_no,
                lease_token: runtime_lease_token(&exited_handle).unwrap_or_default(),
                session_epoch: runtime_session_epoch(&exited_handle),
                event_type: event_type.to_string(),
                event_level: event_level.to_string(),
                message: message.to_string(),
                payload: json!({
                    "runtime_id": exited_handle.runtime_id,
                    "rtp_stream_id": rtp_stream_id_from_handle(&exited_handle),
                    "reason": worker.request.reason,
                }),
            }),
            RuntimeNotification::TaskSnapshot(exited_handle),
        ])
        .terminal()
}

fn runtime_already_gone_commit(
    settings: &AgentSettings,
    handle: &RuntimeHandle,
    generation: RuntimeGeneration,
    request: &StopTaskRequest,
) -> RuntimeMonitorCommit {
    let mut exited_handle = stopping_handle_for_request(handle, request);
    exited_handle.state = RuntimeState::Exited;
    exited_handle.last_progress_at = Some(Utc::now());
    RuntimeMonitorCommit::new(exited_handle.clone(), generation)
        .with_persist(
            attempt_work_dir(settings, request.task_id, request.attempt_no),
            success_check_from_handle(&exited_handle),
        )
        .with_notifications(vec![RuntimeNotification::TaskSnapshot(exited_handle)])
        .terminal()
}

fn runtime_handle_live_processes(handle: &RuntimeHandle) -> Vec<ProcessIdentity> {
    let mut processes = Vec::new();
    if let Some(process) = process_identity_from_handle(handle).filter(|process| {
        is_process_running_for_command_line(process, handle.command_line.as_deref())
    }) {
        processes.push(process);
    }
    if let Some(companion_process) = companion_recording_from_handle(handle).and_then(|companion| {
        companion_process_identity_from_metadata(&companion).filter(|process| {
            is_process_running_for_command_line(process, companion.command_line.as_deref())
        })
    }) {
        processes.push(companion_process);
    }
    processes
}

fn is_stale_attempt_for_request(handle: &RuntimeHandle, request: &StartTaskRequest) -> bool {
    handle.task_id == request.task_id
        && handle.attempt_no < request.attempt_no
        && handle.state != RuntimeState::Exited
        && runtime_lease_token(handle).unwrap_or_default() != request.lease_token
}

fn signal_stale_processes(processes: &[ProcessIdentity], reason: &'static str) {
    if processes.is_empty() {
        return;
    }
    for process in processes {
        if let Err(error) = signal_process(process, libc::SIGTERM) {
            warn!(
                pid = process.pid,
                pgid = ?process.pgid,
                error = %error,
                reason,
                "failed to signal stale runtime process"
            );
        }
    }
    schedule_force_kill_processes_if_running(
        processes.to_vec(),
        STALE_ATTEMPT_FORCE_KILL_DELAY,
        reason,
    );
}
