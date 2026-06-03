//! 启动探测监控：等待 ZLM 流上线并处理录制启动、降级和启动超时。
//!
//! 这里只处理启动阶段的轮询与状态回写，live relay 常态在线/离线监控、RTP 接收监控和
//! ZLM 清理由相邻 runtime_* 模块负责。

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, RwLock, atomic::Ordering},
};

use chrono::Utc;
use media_domain::RuntimeState;
use reqwest::Client;
use serde_json::{Value, json};
use tokio::time::sleep;
use tracing::{error, info};
use uuid::Uuid;

use crate::{
    config::AgentSettings,
    runtime::{
        AUTO_STOP_FORCE_KILL_DELAY, RECORD_DURATION_FORCE_KILL_DELAY, STARTUP_PROBE_POLL_INTERVAL,
        STARTUP_PROBE_TIMEOUT, StartupProbe, SuccessCheck,
    },
    runtime_controls::{
        RecordDurationStopAction, maybe_spawn_manual_recording_duration_timer,
        request_live_relay_record_duration_stop, start_stream_recording,
    },
    runtime_events::{
        RuntimeEventSink, RuntimeNotification, RuntimeTaskEvent, runtime_session_epoch,
    },
    runtime_metadata::{
        clear_source_reconnecting, emit_recording_gap_ended_event,
        emit_recording_gap_started_event, emit_source_reconnecting_event,
        live_relay_recording_from_handle, live_relay_startup_ready, mark_source_reconnecting,
        process_identity_from_handle, resolved_spec_from_handle, runtime_lease_token,
        should_emit_recording_gap_started, should_emit_source_reconnecting,
        sticky_reconnect_stream_ingest_from_handle, stream_binding_from_handle, stream_online,
    },
    runtime_persistence::persist_runtime_state,
    runtime_process::{
        ManagedRuntime, ProcessIdentity, is_pid_running, runtime_processes,
        schedule_force_kill_if_running, signal_process, signal_runtime_processes,
    },
    runtime_recording::{
        mark_recording_completion, mark_recording_failed, recording_elapsed_seconds,
        should_auto_stop_live_relay_recording, should_fail_on_recording_start_error,
        should_start_live_relay_recording,
    },
    runtime_registry::LocalRuntimeRegistry,
    runtime_zlm::zlm_stream_status,
};

pub(crate) fn spawn_startup_probe_monitor(
    runtime_id: Uuid,
    work_dir: PathBuf,
    success_check: SuccessCheck,
    startup_probe: StartupProbe,
    settings: AgentSettings,
    http_client: Client,
    registry: LocalRuntimeRegistry,
    runtimes: Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    events: RuntimeEventSink,
) {
    tokio::spawn(async move {
        let started_at = tokio::time::Instant::now();
        let mut startup_completed = false;
        loop {
            let handle = registry.get(runtime_id);
            let Some(handle) = handle else {
                return;
            };
            let Some(pid) = handle.pid else {
                return;
            };
            if !is_pid_running(pid) {
                return;
            }

            let stream_status = zlm_stream_status(&http_client, &settings, &startup_probe).await;
            if let Ok(Some(stream_status)) = stream_status {
                let wall_clock_duration = resolved_spec_from_handle(&handle)
                    .is_some_and(|spec| spec.stream_ingest_uses_wall_clock_record_duration());
                let binding = stream_binding_from_handle(&handle)
                    .unwrap_or_else(|| stream_status.binding.clone());
                let mut recording_started = false;
                let mut active_handle = handle.clone();
                if let Some(recording) = live_relay_recording_from_handle(&handle)
                    .filter(should_start_live_relay_recording)
                {
                    match start_stream_recording(
                        &http_client,
                        &settings,
                        &binding,
                        &recording,
                        Utc::now(),
                    )
                    .await
                    {
                        Ok(updated_recording) => {
                            emit_recording_gap_ended_event(
                                &events,
                                &handle,
                                "source_reconnected",
                                json!({
                                    "schema": binding.schema,
                                    "vhost": binding.vhost,
                                    "app": binding.app,
                                    "stream": binding.stream,
                                }),
                            );
                            let updated_handle = registry
                                .update(runtime_id, |runtime| {
                                    runtime.last_progress_at = Some(Utc::now());
                                    runtime.metadata["stream_online"] = json!(true);
                                    clear_source_reconnecting(runtime);
                                    runtime.metadata["stream_binding"] = json!({
                                            "schema": binding.schema,
                                            "vhost": binding.vhost,
                                            "app": binding.app,
                                        "stream": binding.stream,
                                    });
                                    runtime.metadata["recording"] =
                                        json!(updated_recording.clone());
                                    runtime.metadata["recording_error"] = Value::Null;
                                })
                                .unwrap_or_else(|| {
                                    let mut handle = active_handle.clone();
                                    handle.last_progress_at = Some(Utc::now());
                                    handle.metadata["stream_online"] = json!(true);
                                    clear_source_reconnecting(&mut handle);
                                    handle.metadata["stream_binding"] = json!({
                                            "schema": binding.schema,
                                            "vhost": binding.vhost,
                                            "app": binding.app,
                                        "stream": binding.stream,
                                    });
                                    handle.metadata["recording"] = json!(updated_recording);
                                    handle.metadata["recording_error"] = Value::Null;
                                    handle
                                });
                            let _ =
                                persist_runtime_state(&work_dir, &updated_handle, &success_check);
                            maybe_spawn_manual_recording_duration_timer(
                                runtime_id,
                                work_dir.clone(),
                                success_check.clone(),
                                binding.clone(),
                                settings.clone(),
                                http_client.clone(),
                                registry.clone(),
                                runtimes.clone(),
                                events.clone(),
                                updated_recording.clone(),
                            );
                            let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                                task_id: updated_handle.task_id,
                                attempt_no: updated_handle.attempt_no,
                                lease_token: runtime_lease_token(&updated_handle)
                                    .unwrap_or_default(),
                                session_epoch: runtime_session_epoch(&updated_handle),
                                event_type: "recording_started".to_string(),
                                event_level: "info".to_string(),
                                message: "stream recording started".to_string(),
                                payload: json!({
                                    "formats": updated_recording.formats,
                                    "root_path": updated_recording.primary_root_path(),
                                    "root_paths": updated_recording.root_paths_payload(),
                                    "duration_sec": updated_recording.duration_sec,
                                    "segment_sec": updated_recording.segment_sec,
                                    "as_player": updated_recording.as_player,
                                }),
                            }));
                            recording_started = true;
                            active_handle = updated_handle;
                        }
                        Err(error) => {
                            let failed_recording = mark_recording_failed(&recording);
                            let fatal = should_fail_on_recording_start_error(&recording);
                            emit_recording_gap_ended_event(
                                &events,
                                &handle,
                                "source_reconnected",
                                json!({
                                    "schema": binding.schema,
                                    "vhost": binding.vhost,
                                    "app": binding.app,
                                    "stream": binding.stream,
                                }),
                            );
                            let updated_handle = registry
                                .update(runtime_id, |runtime| {
                                    runtime.last_progress_at = Some(Utc::now());
                                    runtime.metadata["stream_online"] = json!(true);
                                    clear_source_reconnecting(runtime);
                                    runtime.metadata["stream_binding"] = json!({
                                            "schema": binding.schema,
                                            "vhost": binding.vhost,
                                            "app": binding.app,
                                        "stream": binding.stream,
                                    });
                                    runtime.metadata["recording_error"] = json!(error.to_string());
                                    runtime.metadata["recording"] = json!(failed_recording.clone());
                                    if fatal {
                                        runtime.metadata["recording_fatal_error"] =
                                            json!(error.to_string());
                                    }
                                })
                                .unwrap_or_else(|| {
                                    let mut handle = active_handle.clone();
                                    handle.last_progress_at = Some(Utc::now());
                                    handle.metadata["stream_online"] = json!(true);
                                    clear_source_reconnecting(&mut handle);
                                    handle.metadata["stream_binding"] = json!({
                                            "schema": binding.schema,
                                            "vhost": binding.vhost,
                                            "app": binding.app,
                                        "stream": binding.stream,
                                    });
                                    handle.metadata["recording_error"] = json!(error.to_string());
                                    handle.metadata["recording"] = json!(failed_recording);
                                    if fatal {
                                        handle.metadata["recording_fatal_error"] =
                                            json!(error.to_string());
                                    }
                                    handle
                                });
                            let _ =
                                persist_runtime_state(&work_dir, &updated_handle, &success_check);
                            let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                                task_id: updated_handle.task_id,
                                attempt_no: updated_handle.attempt_no,
                                lease_token: runtime_lease_token(&updated_handle)
                                    .unwrap_or_default(),
                                session_epoch: runtime_session_epoch(&updated_handle),
                                event_type: "zlm_api_error".to_string(),
                                event_level: "error".to_string(),
                                message: format!("failed to start stream recording: {error}"),
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
                            if fatal {
                                let process = process_identity_from_handle(&updated_handle)
                                    .unwrap_or_else(|| ProcessIdentity::pid_only(pid));
                                let _ =
                                    events.send(RuntimeNotification::TaskSnapshot(updated_handle));
                                if signal_process(&process, libc::SIGTERM).is_ok() {
                                    schedule_force_kill_if_running(
                                        runtime_id,
                                        vec![process],
                                        runtimes.clone(),
                                        AUTO_STOP_FORCE_KILL_DELAY,
                                        "recording_start_fatal",
                                    );
                                }
                                return;
                            }
                            let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                                task_id: updated_handle.task_id,
                                attempt_no: updated_handle.attempt_no,
                                lease_token: runtime_lease_token(&updated_handle)
                                    .unwrap_or_default(),
                                session_epoch: runtime_session_epoch(&updated_handle),
                                event_type: "recording_degraded".to_string(),
                                event_level: "warn".to_string(),
                                message:
                                    "stream recording startup failed; continuing without recording"
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
                            let _ = events
                                .send(RuntimeNotification::TaskSnapshot(updated_handle.clone()));
                            active_handle = updated_handle;
                        }
                    }
                }
                let handle = registry.get(runtime_id).unwrap_or(active_handle);
                if let Some(recording) = live_relay_recording_from_handle(&handle) {
                    let now = Utc::now();
                    if should_auto_stop_live_relay_recording(&recording, now) {
                        info!(
                            task_id = %handle.task_id,
                            attempt_no = handle.attempt_no,
                            runtime_id = %handle.runtime_id,
                            pid,
                            stream_schema = binding.schema.as_deref().unwrap_or(""),
                            stream_vhost = %binding.vhost,
                            stream_app = %binding.app,
                            stream_name = %binding.stream,
                            recording_started_at = ?recording.recording_started_at,
                            duration_sec = recording.duration_sec.unwrap_or_default(),
                            now = %now.to_rfc3339(),
                            elapsed_sec = recording_elapsed_seconds(&recording, now)
                                .unwrap_or_default(),
                            command_line = handle.command_line.as_deref().unwrap_or(""),
                            "wall-clock recording duration reached in startup probe monitor"
                        );
                        let completed_recording =
                            mark_recording_completion(&recording, "record_duration_reached");
                        let completion_handle = registry
                            .update(runtime_id, |runtime| {
                                runtime.state = RuntimeState::Stopping;
                                runtime.last_progress_at = Some(Utc::now());
                                runtime.metadata["recording"] = json!(completed_recording.clone());
                                runtime.metadata["completion_reason"] =
                                    json!("record_duration_reached");
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
                                handle.metadata["completion_reason"] =
                                    json!("record_duration_reached");
                                handle.metadata["stop"] = json!({
                                    "reason": "record_duration_reached",
                                    "grace_period_sec": 0,
                                    "force_after_sec": RECORD_DURATION_FORCE_KILL_DELAY.as_secs_f64(),
                                });
                                handle
                            });
                        let _ =
                            persist_runtime_state(&work_dir, &completion_handle, &success_check);
                        info!(
                            task_id = %completion_handle.task_id,
                            attempt_no = completion_handle.attempt_no,
                            runtime_id = %completion_handle.runtime_id,
                            pid,
                            auto_stop_requested = completed_recording.auto_stop_requested,
                            completion_reason = completed_recording
                                .completion_reason
                                .as_deref()
                                .unwrap_or(""),
                            last_progress_at = ?completion_handle.last_progress_at,
                            "updated runtime metadata after wall-clock recording duration reached"
                        );
                        let runtime = {
                            let runtimes = runtimes.read().expect("runtime map lock poisoned");
                            runtimes.get(&runtime_id).cloned()
                        };
                        if let Some(runtime) = runtime {
                            runtime.stop_requested.store(true, Ordering::Relaxed);
                        }
                        match request_live_relay_record_duration_stop(
                            &completion_handle,
                            &binding,
                            &settings,
                            &http_client,
                            &runtimes,
                        )
                        .await
                        {
                            Ok(RecordDurationStopAction::SignalProcess { pid }) => info!(
                                task_id = %completion_handle.task_id,
                                attempt_no = completion_handle.attempt_no,
                                runtime_id = %completion_handle.runtime_id,
                                pid,
                                signal = "SIGTERM",
                                force_after_sec = RECORD_DURATION_FORCE_KILL_DELAY.as_secs_f64(),
                                "requested process shutdown after wall-clock recording duration reached"
                            ),
                            Ok(RecordDurationStopAction::CloseStream) => info!(
                                task_id = %completion_handle.task_id,
                                attempt_no = completion_handle.attempt_no,
                                runtime_id = %completion_handle.runtime_id,
                                stream_schema = binding.schema.as_deref().unwrap_or(""),
                                stream_vhost = %binding.vhost,
                                stream_app = %binding.app,
                                stream_name = %binding.stream,
                                "closed live_relay stream after wall-clock recording duration reached"
                            ),
                            Err(error) => error!(
                                task_id = %completion_handle.task_id,
                                attempt_no = completion_handle.attempt_no,
                                runtime_id = %completion_handle.runtime_id,
                                error = %error,
                                "failed to stop live_relay after wall-clock recording duration reached"
                            ),
                        }
                        return;
                    }
                }

                let startup_ready = live_relay_startup_ready(&handle);
                if !startup_ready {
                    sleep(STARTUP_PROBE_POLL_INTERVAL).await;
                    continue;
                }

                let should_emit_running = !startup_completed
                    || handle.state != RuntimeState::Running
                    || !stream_online(&handle)
                    || recording_started;
                let running_handle = if should_emit_running {
                    emit_recording_gap_ended_event(
                        &events,
                        &handle,
                        "source_reconnected",
                        json!({
                            "schema": startup_probe.schema,
                            "vhost": startup_probe.vhost,
                            "app": startup_probe.app,
                            "stream": startup_probe.stream,
                        }),
                    );
                    let running_handle = registry
                        .update(runtime_id, |runtime| {
                            runtime.state = RuntimeState::Running;
                            runtime.last_progress_at = Some(Utc::now());
                            runtime.metadata["stream_online"] = json!(true);
                            clear_source_reconnecting(runtime);
                            runtime.metadata["stream_binding"] = json!({
                                        "schema": startup_probe.schema,
                                        "vhost": startup_probe.vhost,
                                        "app": startup_probe.app,
                                "stream": startup_probe.stream,
                            });
                        })
                        .unwrap_or_else(|| handle.clone());
                    let _ = persist_runtime_state(&work_dir, &running_handle, &success_check);
                    let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                        task_id: running_handle.task_id,
                        attempt_no: running_handle.attempt_no,
                        lease_token: runtime_lease_token(&running_handle).unwrap_or_default(),
                        session_epoch: runtime_session_epoch(&running_handle),
                        event_type: "running".to_string(),
                        event_level: "info".to_string(),
                        message: "ZLM stream is online".to_string(),
                        payload: json!({
                            "runtime_id": running_handle.runtime_id,
                            "pid": running_handle.pid,
                            "schema": startup_probe.schema,
                            "vhost": startup_probe.vhost,
                            "app": startup_probe.app,
                            "stream": startup_probe.stream,
                            "recording_started": recording_started,
                        }),
                    }));
                    let _ = events.send(RuntimeNotification::TaskSnapshot(running_handle.clone()));
                    running_handle
                } else {
                    handle.clone()
                };

                startup_completed = true;
                if !wall_clock_duration {
                    return;
                }
                let _ = persist_runtime_state(&work_dir, &running_handle, &success_check);
            } else if !startup_completed && started_at.elapsed() >= STARTUP_PROBE_TIMEOUT {
                if sticky_reconnect_stream_ingest_from_handle(&handle) {
                    let emit_event = should_emit_source_reconnecting(&handle, "startup_timeout");
                    let emit_gap_started = should_emit_recording_gap_started(&handle);
                    let updated = registry.update(runtime_id, |runtime| {
                        runtime.metadata["startup_timeout"] = json!(true);
                        mark_source_reconnecting(runtime, "startup_timeout");
                    });
                    if let Some(handle) = updated {
                        let _ = persist_runtime_state(&work_dir, &handle, &success_check);
                        if emit_event {
                            emit_source_reconnecting_event(
                                &events,
                                &handle,
                                format!(
                                    "ZLM stream {}/{}/{} is not online yet; continuing to retry",
                                    startup_probe.vhost, startup_probe.app, startup_probe.stream
                                ),
                                json!({
                                    "runtime_id": handle.runtime_id,
                                    "schema": startup_probe.schema,
                                    "vhost": startup_probe.vhost,
                                    "app": startup_probe.app,
                                    "stream": startup_probe.stream,
                                    "reason": "startup_timeout",
                                }),
                            );
                            let _ = events.send(RuntimeNotification::TaskSnapshot(handle.clone()));
                        }
                        if emit_gap_started {
                            emit_recording_gap_started_event(
                                &events,
                                &handle,
                                "startup_timeout",
                                json!({
                                    "runtime_id": handle.runtime_id,
                                    "schema": startup_probe.schema,
                                    "vhost": startup_probe.vhost,
                                    "app": startup_probe.app,
                                    "stream": startup_probe.stream,
                                }),
                            );
                        }
                    }
                    sleep(STARTUP_PROBE_POLL_INTERVAL).await;
                    continue;
                }
                let updated = registry.update(runtime_id, |runtime| {
                    runtime.metadata["startup_timeout"] = json!(true);
                    runtime.metadata["stream_online"] = json!(false);
                });
                if let Some(handle) = updated {
                    let _ = persist_runtime_state(&work_dir, &handle, &success_check);
                    let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                        task_id: handle.task_id,
                        attempt_no: handle.attempt_no,
                        lease_token: runtime_lease_token(&handle).unwrap_or_default(),
                        session_epoch: runtime_session_epoch(&handle),
                        event_type: "startup_timeout".to_string(),
                        event_level: "error".to_string(),
                        message: format!(
                            "ZLM stream {}/{}/{} did not become online within {} seconds",
                            startup_probe.vhost,
                            startup_probe.app,
                            startup_probe.stream,
                            STARTUP_PROBE_TIMEOUT.as_secs()
                        ),
                        payload: json!({
                            "schema": startup_probe.schema,
                            "vhost": startup_probe.vhost,
                            "app": startup_probe.app,
                            "stream": startup_probe.stream,
                        }),
                    }));
                }
                let runtime = {
                    let runtimes = runtimes.read().expect("runtime map lock poisoned");
                    runtimes.get(&runtime_id).cloned()
                };
                if let Some(runtime) = runtime {
                    if signal_runtime_processes(&runtime, libc::SIGTERM).is_ok() {
                        schedule_force_kill_if_running(
                            runtime_id,
                            runtime_processes(&runtime),
                            runtimes.clone(),
                            AUTO_STOP_FORCE_KILL_DELAY,
                            "startup_probe_timeout",
                        );
                    }
                }
                return;
            }

            sleep(STARTUP_PROBE_POLL_INTERVAL).await;
        }
    });
}
