//! Live relay 录制辅助：处理常态监控中的录制启动、降级和时长停止请求。
//!
//! 这里只封装 live relay 主循环里重复的录制启动状态回写、失败降级/fatal 收尾、录制完成持久化
//! 和停止请求；主循环仍负责判断何时探测、何时进入运行态。

use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, RwLock, atomic::Ordering},
};

use chrono::Utc;
use media_domain::{RuntimeHandle, RuntimeState};
use reqwest::Client;
use serde_json::{Value, json};
use tracing::info;
use uuid::Uuid;

use crate::{
    config::AgentSettings,
    runtime::{RECORD_DURATION_FORCE_KILL_DELAY, StartupProbe, SuccessCheck},
    runtime_controls::{
        maybe_spawn_manual_recording_duration_timer, request_live_relay_record_duration_stop,
        start_stream_recording,
    },
    runtime_events::{
        RuntimeEventSink, RuntimeNotification, RuntimeTaskEvent, runtime_session_epoch,
    },
    runtime_live_relay_cleanup::cleanup_live_relay_runtime,
    runtime_manager::{RecordDurationReachedEvent, RuntimeInternalEvent, RuntimeMonitorHandle},
    runtime_metadata::{
        StreamBinding, clear_source_reconnecting, emit_recording_gap_ended_event,
        live_relay_recording_from_handle, runtime_lease_token, stream_binding_from_handle,
    },
    runtime_persistence::persist_runtime_state,
    runtime_process::{ManagedRuntime, remove_managed_runtime},
    runtime_recording::{
        LiveRelayRecording, mark_recording_completion, mark_recording_failed,
        recording_elapsed_seconds, should_auto_stop_live_relay_recording,
        should_fail_on_recording_start_error,
    },
    runtime_registry::LocalRuntimeRegistry,
    runtime_zlm::stop_live_relay_recording,
};

#[derive(Debug, Clone, Copy)]
pub(crate) enum LiveRelayRecordingStartMode {
    RecordingStartup,
    OnlineMonitor,
}

pub(crate) enum LiveRelayRecordingStartOutcome {
    Updated {
        handle: RuntimeHandle,
        recording_started: bool,
    },
    Fatal,
}

pub(crate) struct LiveRelayRecordingStartContext<'a> {
    pub(crate) runtime_id: Uuid,
    pub(crate) work_dir: &'a Path,
    pub(crate) settings: &'a AgentSettings,
    pub(crate) http_client: &'a Client,
    pub(crate) registry: &'a LocalRuntimeRegistry,
    pub(crate) runtimes: &'a Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    pub(crate) events: &'a RuntimeEventSink,
}

pub(crate) async fn start_live_relay_recording_from_monitor(
    ctx: LiveRelayRecordingStartContext<'_>,
    current_handle: &RuntimeHandle,
    active_handle: &RuntimeHandle,
    binding: &StreamBinding,
    recording: &LiveRelayRecording,
    mode: LiveRelayRecordingStartMode,
) -> LiveRelayRecordingStartOutcome {
    match start_stream_recording(
        ctx.http_client,
        ctx.settings,
        binding,
        recording,
        Utc::now(),
    )
    .await
    {
        Ok(updated_recording) => {
            if matches!(mode, LiveRelayRecordingStartMode::OnlineMonitor) {
                emit_recording_gap_ended_event(
                    ctx.events,
                    current_handle,
                    "source_reconnected",
                    json!({
                        "schema": binding.schema,
                        "vhost": binding.vhost,
                        "app": binding.app,
                        "stream": binding.stream,
                    }),
                );
            }
            let updated_handle = ctx
                .registry
                .update(ctx.runtime_id, |runtime| {
                    runtime.last_progress_at = Some(Utc::now());
                    runtime.metadata["stream_online"] = json!(true);
                    if matches!(mode, LiveRelayRecordingStartMode::OnlineMonitor) {
                        clear_source_reconnecting(runtime);
                    }
                    if matches!(mode, LiveRelayRecordingStartMode::RecordingStartup) {
                        runtime.metadata["stream_binding"] = json!({
                            "schema": binding.schema,
                            "vhost": binding.vhost,
                            "app": binding.app,
                            "stream": binding.stream,
                        });
                    }
                    runtime.metadata["recording"] = json!(updated_recording.clone());
                    runtime.metadata["recording_error"] = Value::Null;
                })
                .unwrap_or_else(|| {
                    let mut handle = active_handle.clone();
                    handle.last_progress_at = Some(Utc::now());
                    handle.metadata["stream_online"] = json!(true);
                    if matches!(mode, LiveRelayRecordingStartMode::OnlineMonitor) {
                        clear_source_reconnecting(&mut handle);
                    }
                    if matches!(mode, LiveRelayRecordingStartMode::RecordingStartup) {
                        handle.metadata["stream_binding"] = json!({
                            "schema": binding.schema,
                            "vhost": binding.vhost,
                            "app": binding.app,
                            "stream": binding.stream,
                        });
                    }
                    handle.metadata["recording"] = json!(updated_recording);
                    handle.metadata["recording_error"] = Value::Null;
                    handle
                });
            let _ = ctx
                .events
                .send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                    task_id: updated_handle.task_id,
                    attempt_no: updated_handle.attempt_no,
                    lease_token: runtime_lease_token(&updated_handle).unwrap_or_default(),
                    session_epoch: runtime_session_epoch(&updated_handle),
                    event_type: "recording_started".to_string(),
                    event_level: "info".to_string(),
                    message: "live_relay recording started".to_string(),
                    payload: json!({
                        "formats": updated_recording.formats,
                        "root_path": updated_recording.primary_root_path(),
                        "root_paths": updated_recording.root_paths_payload(),
                        "duration_sec": updated_recording.duration_sec,
                        "segment_sec": updated_recording.segment_sec,
                        "as_player": updated_recording.as_player,
                    }),
                }));
            let _ =
                persist_runtime_state(ctx.work_dir, &updated_handle, &SuccessCheck::ProcessExit);
            maybe_spawn_manual_recording_duration_timer(
                ctx.runtime_id,
                ctx.work_dir.to_path_buf(),
                SuccessCheck::ProcessExit,
                binding.clone(),
                ctx.settings.clone(),
                ctx.http_client.clone(),
                ctx.registry.clone(),
                ctx.runtimes.clone(),
                ctx.events.clone(),
                updated_recording.clone(),
            );
            LiveRelayRecordingStartOutcome::Updated {
                handle: updated_handle,
                recording_started: true,
            }
        }
        Err(error) if matches!(mode, LiveRelayRecordingStartMode::RecordingStartup) => {
            let updated_handle = ctx
                .registry
                .update(ctx.runtime_id, |runtime| {
                    runtime.last_progress_at = Some(Utc::now());
                    runtime.metadata["recording_error"] = json!(error.to_string());
                })
                .unwrap_or_else(|| {
                    let mut handle = active_handle.clone();
                    handle.last_progress_at = Some(Utc::now());
                    handle.metadata["recording_error"] = json!(error.to_string());
                    handle
                });
            let _ =
                persist_runtime_state(ctx.work_dir, &updated_handle, &SuccessCheck::ProcessExit);
            LiveRelayRecordingStartOutcome::Updated {
                handle: updated_handle,
                recording_started: false,
            }
        }
        Err(error) => {
            let failed_recording = mark_recording_failed(recording);
            let fatal = should_fail_on_recording_start_error(recording);
            emit_recording_gap_ended_event(
                ctx.events,
                current_handle,
                "source_reconnected",
                json!({
                    "schema": binding.schema,
                    "vhost": binding.vhost,
                    "app": binding.app,
                    "stream": binding.stream,
                }),
            );
            let degraded_handle = ctx
                .registry
                .update(ctx.runtime_id, |runtime| {
                    runtime.last_progress_at = Some(Utc::now());
                    runtime.metadata["stream_online"] = json!(true);
                    clear_source_reconnecting(runtime);
                    runtime.metadata["recording_error"] = json!(error.to_string());
                    runtime.metadata["recording"] = json!(failed_recording.clone());
                    if fatal {
                        runtime.metadata["recording_fatal_error"] = json!(error.to_string());
                    }
                })
                .unwrap_or_else(|| {
                    let mut handle = active_handle.clone();
                    handle.last_progress_at = Some(Utc::now());
                    handle.metadata["stream_online"] = json!(true);
                    clear_source_reconnecting(&mut handle);
                    handle.metadata["recording_error"] = json!(error.to_string());
                    handle.metadata["recording"] = json!(failed_recording);
                    if fatal {
                        handle.metadata["recording_fatal_error"] = json!(error.to_string());
                    }
                    handle
                });
            let _ = ctx
                .events
                .send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                    task_id: degraded_handle.task_id,
                    attempt_no: degraded_handle.attempt_no,
                    lease_token: runtime_lease_token(&degraded_handle).unwrap_or_default(),
                    session_epoch: runtime_session_epoch(&degraded_handle),
                    event_type: "zlm_api_error".to_string(),
                    event_level: "error".to_string(),
                    message: format!("failed to start live_relay recording: {error}"),
                    payload: json!({
                        "schema": binding.schema,
                        "vhost": binding.vhost,
                        "app": binding.app,
                        "stream": binding.stream,
                        "record_root": recording.primary_root_path(),
                        "record_roots": recording.root_paths_payload(),
                        "duration_sec": recording.duration_sec,
                    }),
                }));
            let _ =
                persist_runtime_state(ctx.work_dir, &degraded_handle, &SuccessCheck::ProcessExit);
            if fatal {
                let _ = ctx
                    .events
                    .send(RuntimeNotification::TaskSnapshot(degraded_handle.clone()));
                let _ =
                    stop_live_relay_recording(ctx.http_client, ctx.settings, binding, recording)
                        .await;
                cleanup_live_relay_runtime(
                    ctx.http_client,
                    ctx.settings,
                    &degraded_handle,
                    binding,
                )
                .await;
                let failed_handle = ctx
                    .registry
                    .update(ctx.runtime_id, |runtime| {
                        runtime.state = RuntimeState::Exited;
                        runtime.last_progress_at = Some(Utc::now());
                    })
                    .unwrap_or(degraded_handle.clone());
                let _ =
                    persist_runtime_state(ctx.work_dir, &failed_handle, &SuccessCheck::ProcessExit);
                let _ = ctx
                    .events
                    .send(RuntimeNotification::TaskSnapshot(failed_handle.clone()));
                let _ = ctx
                    .events
                    .send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                        task_id: failed_handle.task_id,
                        attempt_no: failed_handle.attempt_no,
                        lease_token: runtime_lease_token(&failed_handle).unwrap_or_default(),
                        session_epoch: runtime_session_epoch(&failed_handle),
                        event_type: "failed".to_string(),
                        event_level: "error".to_string(),
                        message: "live_relay recording startup failed".to_string(),
                        payload: json!({
                            "schema": binding.schema,
                            "vhost": binding.vhost,
                            "app": binding.app,
                            "stream": binding.stream,
                            "record_root": recording.primary_root_path(),
                            "record_roots": recording.root_paths_payload(),
                            "reason": "recording_start_failed",
                        }),
                    }));
                let _ = remove_managed_runtime(ctx.runtimes, ctx.runtime_id);
                let _ = ctx.registry.remove(ctx.runtime_id);
                return LiveRelayRecordingStartOutcome::Fatal;
            }
            let _ = ctx
                .events
                .send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                    task_id: degraded_handle.task_id,
                    attempt_no: degraded_handle.attempt_no,
                    lease_token: runtime_lease_token(&degraded_handle).unwrap_or_default(),
                    session_epoch: runtime_session_epoch(&degraded_handle),
                    event_type: "recording_degraded".to_string(),
                    event_level: "warn".to_string(),
                    message: "live_relay recording startup failed; continuing without recording"
                        .to_string(),
                    payload: json!({
                        "schema": binding.schema,
                        "vhost": binding.vhost,
                        "app": binding.app,
                        "stream": binding.stream,
                        "record_root": recording.primary_root_path(),
                        "record_roots": recording.root_paths_payload(),
                    }),
                }));
            let _ = ctx
                .events
                .send(RuntimeNotification::TaskSnapshot(degraded_handle.clone()));
            LiveRelayRecordingStartOutcome::Updated {
                handle: degraded_handle,
                recording_started: false,
            }
        }
    }
}

pub(crate) async fn notify_live_relay_record_duration_if_reached(
    monitor_handle: &RuntimeMonitorHandle,
    handle: &RuntimeHandle,
) -> bool {
    let now = Utc::now();
    let Some(recording) = live_relay_recording_from_handle(handle)
        .filter(|recording| should_auto_stop_live_relay_recording(recording, now))
    else {
        return false;
    };

    info!(
        task_id = %handle.task_id,
        attempt_no = handle.attempt_no,
        runtime_id = %handle.runtime_id,
        generation = monitor_handle.generation().value(),
        recording_started_at = ?recording.recording_started_at,
        duration_sec = recording.duration_sec.unwrap_or_default(),
        now = %now.to_rfc3339(),
        elapsed_sec = recording_elapsed_seconds(&recording, now).unwrap_or_default(),
        command_line = handle.command_line.as_deref().unwrap_or(""),
        "wall-clock recording duration reached; notifying runtime manager"
    );
    monitor_handle
        .send_event(RuntimeInternalEvent::RecordDurationReached(
            RecordDurationReachedEvent {
                runtime_id: handle.runtime_id,
                generation: monitor_handle.generation(),
            },
        ))
        .await;
    true
}

pub(crate) async fn stop_live_relay_for_record_duration_if_reached(
    runtime_id: Uuid,
    work_dir: &Path,
    startup_probe: &StartupProbe,
    settings: &AgentSettings,
    http_client: &Client,
    registry: &LocalRuntimeRegistry,
    runtimes: &Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    handle: &RuntimeHandle,
) -> bool {
    let Some(recording) = live_relay_recording_from_handle(handle)
        .filter(|recording| should_auto_stop_live_relay_recording(recording, Utc::now()))
    else {
        return false;
    };

    let completed_recording = mark_recording_completion(&recording, "record_duration_reached");
    let completion_handle = registry
        .update(runtime_id, |runtime| {
            runtime.state = RuntimeState::Stopping;
            runtime.last_progress_at = Some(Utc::now());
            runtime.metadata["recording"] = json!(completed_recording.clone());
            runtime.metadata["completion_reason"] = json!("record_duration_reached");
            runtime.metadata["stop"] = json!({
                "reason": "record_duration_reached",
                "grace_period_sec": 0,
                "force_after_sec": RECORD_DURATION_FORCE_KILL_DELAY.as_secs_f64(),
            });
        })
        .unwrap_or_else(|| {
            let mut handle = handle.clone();
            handle.state = RuntimeState::Stopping;
            handle.last_progress_at = Some(Utc::now());
            handle.metadata["recording"] = json!(completed_recording.clone());
            handle.metadata["completion_reason"] = json!("record_duration_reached");
            handle.metadata["stop"] = json!({
                "reason": "record_duration_reached",
                "grace_period_sec": 0,
                "force_after_sec": RECORD_DURATION_FORCE_KILL_DELAY.as_secs_f64(),
            });
            handle
        });
    let _ = persist_runtime_state(work_dir, &completion_handle, &SuccessCheck::ProcessExit);
    let runtime = {
        let runtimes = runtimes.read().expect("runtime map lock poisoned");
        runtimes.get(&runtime_id).cloned()
    };
    if let Some(runtime) = runtime {
        runtime.stop_requested.store(true, Ordering::Relaxed);
    }
    let binding = stream_binding_from_handle(&completion_handle).unwrap_or(StreamBinding {
        schema: startup_probe.schema.clone(),
        vhost: startup_probe.vhost.clone(),
        app: startup_probe.app.clone(),
        stream: startup_probe.stream.clone(),
    });
    let _ = request_live_relay_record_duration_stop(
        &completion_handle,
        &binding,
        settings,
        http_client,
        runtimes,
    )
    .await;

    true
}
