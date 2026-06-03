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
    runtime_events::{
        RuntimeEventSink, RuntimeNotification, RuntimeTaskEvent, runtime_session_epoch,
    },
    runtime_io::attempt_work_dir,
    runtime_metadata::{
        companion_process_identity_from_metadata, companion_recording_from_handle,
        process_identity_from_handle, rtp_stream_id_from_handle, runtime_lease_token,
        stream_binding_from_handle, task_runtime_mode_from_handle,
    },
    runtime_persistence::{
        persist_runtime_state, scan_persisted_runtimes, success_check_from_handle,
    },
    runtime_plan::TaskRuntimeMode,
    runtime_process::{
        ManagedRuntime, ProcessIdentity, is_pid_running, remove_managed_runtime, runtime_processes,
        schedule_force_kill_if_running, schedule_force_kill_processes_if_running, signal_process,
        signal_runtime_processes,
    },
    runtime_registry::LocalRuntimeRegistry,
};

const STALE_ATTEMPT_FORCE_KILL_DELAY: Duration = Duration::from_secs(1);

pub(crate) struct StaleAttemptCleanupContext<'a> {
    pub(crate) settings: &'a AgentSettings,
    pub(crate) registry: &'a LocalRuntimeRegistry,
    pub(crate) runtimes: &'a Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
}

pub(crate) fn cleanup_stale_attempt_runtimes(
    ctx: StaleAttemptCleanupContext<'_>,
    request: &StartTaskRequest,
) {
    let active_handles = ctx.registry.active_handles();
    let mut handled_attempts = std::collections::HashSet::new();
    for handle in active_handles {
        if !is_stale_attempt_for_request(&handle, request) {
            continue;
        }
        handled_attempts.insert((handle.task_id, handle.attempt_no));

        let runtime = {
            let runtimes = ctx.runtimes.read().expect("runtime map lock poisoned");
            runtimes.get(&handle.runtime_id).cloned()
        };
        if let Some(runtime) = runtime {
            runtime.stop_requested.store(true, Ordering::Relaxed);
            let processes = runtime_processes(&runtime);
            if processes.is_empty() {
                continue;
            }
            ctx.registry.update(handle.runtime_id, |runtime| {
                runtime.state = RuntimeState::Stopping;
                runtime.last_progress_at = Some(Utc::now());
                runtime.metadata["stop"] = json!({
                    "reason": "stale_attempt_replaced",
                    "replacement_attempt_no": request.attempt_no,
                });
            });
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
            schedule_force_kill_if_running(
                handle.runtime_id,
                processes,
                ctx.runtimes.clone(),
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
}

pub(crate) struct RuntimeStopContext<'a> {
    pub(crate) settings: &'a AgentSettings,
    pub(crate) registry: &'a LocalRuntimeRegistry,
    pub(crate) runtimes: &'a Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    pub(crate) events: &'a RuntimeEventSink,
    pub(crate) stop_intents: &'a Arc<RwLock<HashMap<(Uuid, i32), StopTaskRequest>>>,
    pub(crate) controls: RuntimeControlContext<'a>,
}

pub(crate) async fn stop_runtime_task(
    ctx: RuntimeStopContext<'_>,
    request: &StopTaskRequest,
) -> Result<(), ExecutorError> {
    let key = (request.task_id, request.attempt_no);
    {
        let mut stop_intents = ctx
            .stop_intents
            .write()
            .expect("stop intents lock poisoned");
        stop_intents.insert(key, request.clone());
    }

    let handle = ctx
        .registry
        .find_by_task_attempt(request.task_id, request.attempt_no)
        .ok_or(ExecutorError::RuntimeNotFound {
            task_id: request.task_id,
            attempt_no: request.attempt_no,
        });
    let Ok(handle) = handle else {
        return Ok(());
    };
    let handle_lease_token = runtime_lease_token(&handle).unwrap_or_default();
    if handle_lease_token != request.lease_token {
        return Err(ExecutorError::InvalidRequest(format!(
            "stale stop for {}/{}: lease_token mismatch",
            request.task_id, request.attempt_no
        )));
    }
    let runtime = {
        let runtimes = ctx.runtimes.read().expect("runtime map lock poisoned");
        runtimes.get(&handle.runtime_id).cloned()
    }
    .ok_or(ExecutorError::RuntimeNotFound {
        task_id: request.task_id,
        attempt_no: request.attempt_no,
    })?;

    runtime.stop_requested.store(true, Ordering::Relaxed);
    let runtime_id = handle.runtime_id;
    let reason = request.reason.clone();
    let grace_period_sec = request.grace_period_sec;
    let force_after_sec = request.force_after_sec;
    ctx.registry.update(runtime_id, |runtime| {
        runtime.state = RuntimeState::Stopping;
        runtime.last_progress_at = Some(Utc::now());
        runtime.metadata["stop"] = json!({
            "reason": reason,
            "grace_period_sec": grace_period_sec,
            "force_after_sec": force_after_sec,
        });
        if let Some(mut recording) = runtime
            .metadata
            .get("recording")
            .cloned()
            .and_then(|value| serde_json::from_value::<LiveRelayRecording>(value).ok())
        {
            recording.started = false;
            runtime.metadata["recording"] = json!(recording);
        }
    });

    if runtime.process.is_some() {
        let managed_live_relay = matches!(
            task_runtime_mode_from_handle(&handle),
            Some(TaskRuntimeMode::ManagedProcess)
        ) && stream_binding_from_handle(&handle).is_some();
        if managed_live_relay {
            stop_live_relay_recording_for_handle(&ctx.controls, &handle).await?;
        }
        signal_runtime_processes(&runtime, libc::SIGTERM)?;
        if managed_live_relay {
            close_live_relay(&ctx.controls, &handle, true).await?;
        }
    } else if matches!(
        task_runtime_mode_from_handle(&handle),
        Some(TaskRuntimeMode::ZlmProxy)
    ) {
        stop_live_relay_recording_for_handle(&ctx.controls, &handle).await?;
        close_live_relay(&ctx.controls, &handle, true).await?;
    } else if matches!(
        task_runtime_mode_from_handle(&handle),
        Some(TaskRuntimeMode::ZlmRtpServer)
    ) {
        let stopping_handle = ctx.registry.get(runtime_id).unwrap_or(handle.clone());
        let work_dir = attempt_work_dir(ctx.settings, request.task_id, request.attempt_no);
        let _ = persist_runtime_state(
            &work_dir,
            &stopping_handle,
            &success_check_from_handle(&stopping_handle),
        );
        close_rtp_receive(&ctx.controls, &stopping_handle).await?;
        let _ = remove_managed_runtime(ctx.runtimes, runtime_id);
        let exited_handle = ctx
            .registry
            .update(runtime_id, |runtime| {
                runtime.state = RuntimeState::Exited;
                runtime.last_progress_at = Some(Utc::now());
                runtime.metadata["stream_online"] = json!(false);
            })
            .unwrap_or_else(|| {
                let mut handle = stopping_handle.clone();
                handle.state = RuntimeState::Exited;
                handle.last_progress_at = Some(Utc::now());
                handle.metadata["stream_online"] = json!(false);
                handle
            });
        let _ = persist_runtime_state(
            &work_dir,
            &exited_handle,
            &success_check_from_handle(&exited_handle),
        );
        let (event_type, event_level, message) = if request.reason == "disk_threshold_exceeded" {
            (
                "failed",
                "error",
                "stream_ingest rtp server stopped after disk threshold was exceeded",
            )
        } else {
            ("canceled", "info", "stream_ingest rtp server stopped")
        };
        let _ = ctx
            .events
            .send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
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
                    "reason": request.reason,
                }),
            }));
        let _ = ctx
            .events
            .send(RuntimeNotification::TaskSnapshot(exited_handle.clone()));
        let _ = ctx.registry.remove(runtime_id);
        return Ok(());
    }
    if let Some(handle) = ctx
        .registry
        .find_by_task_attempt(request.task_id, request.attempt_no)
    {
        let work_dir = attempt_work_dir(ctx.settings, request.task_id, request.attempt_no);
        let _ = persist_runtime_state(&work_dir, &handle, &success_check_from_handle(&handle));
    }

    if runtime.process.is_some() && force_after_sec > 0 {
        schedule_force_kill_if_running(
            runtime_id,
            runtime_processes(&runtime),
            ctx.runtimes.clone(),
            Duration::from_secs(force_after_sec as u64),
            "stop_task_force_after",
        );
    }

    Ok(())
}

fn runtime_handle_live_processes(handle: &RuntimeHandle) -> Vec<ProcessIdentity> {
    let mut processes = Vec::new();
    if let Some(process) =
        process_identity_from_handle(handle).filter(|process| is_pid_running(process.pid))
    {
        processes.push(process);
    }
    if let Some(companion_process) = companion_recording_from_handle(handle)
        .and_then(|companion| companion_process_identity_from_metadata(&companion))
        .filter(|process| is_pid_running(process.pid))
    {
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
