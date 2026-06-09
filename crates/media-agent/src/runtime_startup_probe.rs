//! 启动探测监控：等待 ZLM 流上线并处理录制启动、降级和启动超时。
//!
//! 这里只处理启动阶段的轮询与状态回写，live relay 常态在线/离线监控、RTP 接收监控和
//! ZLM 清理由相邻 runtime_* 模块负责。

use std::path::PathBuf;

use chrono::Utc;
use media_domain::RuntimeState;
use reqwest::Client;
use serde_json::{Value, json};
use tokio::time::sleep;

use crate::{
    config::AgentSettings,
    runtime::{STARTUP_PROBE_POLL_INTERVAL, STARTUP_PROBE_TIMEOUT, StartupProbe, SuccessCheck},
    runtime_controls::start_stream_recording,
    runtime_events::{
        RuntimeEventSink, RuntimeNotification, RuntimeTaskEvent, runtime_session_epoch,
    },
    runtime_live_relay_recording::notify_live_relay_record_duration_if_reached,
    runtime_manager::{RuntimeInternalEvent, RuntimeMonitorCommit, RuntimeMonitorHandle},
    runtime_metadata::{
        clear_source_reconnecting, emit_recording_gap_started_event,
        live_relay_recording_from_handle, live_relay_startup_ready, mark_source_reconnecting,
        process_identity_from_handle, resolved_spec_from_handle, runtime_lease_token,
        should_emit_recording_gap_started, should_emit_source_reconnecting,
        sticky_reconnect_stream_ingest_from_handle, stream_binding_from_handle, stream_online,
    },
    runtime_process::{ProcessIdentity, is_pid_running, signal_process},
    runtime_recording::{
        mark_recording_failed, should_fail_on_recording_start_error,
        should_start_live_relay_recording,
    },
    runtime_zlm::zlm_stream_status,
};

pub(crate) fn spawn_startup_probe_monitor(
    work_dir: PathBuf,
    success_check: SuccessCheck,
    startup_probe: StartupProbe,
    settings: AgentSettings,
    http_client: Client,
    events: RuntimeEventSink,
    monitor_handle: RuntimeMonitorHandle,
) {
    tokio::spawn(async move {
        {
            let started_at = tokio::time::Instant::now();
            let mut startup_completed = false;
            loop {
                let Some(snapshot) = monitor_handle.snapshot().await else {
                    return;
                };
                let handle = snapshot.handle;
                let Some(pid) = handle.pid else {
                    return;
                };
                // 启动探测绑定底层进程生命周期；进程已退出时让进程监控路径负责终态，
                // 这里不再额外制造启动失败事件。
                if !is_pid_running(pid) {
                    return;
                }

                let stream_status =
                    zlm_stream_status(&http_client, &settings, &startup_probe).await;
                if let Ok(Some(stream_status)) = stream_status {
                    // ZLM 已看到流后，先处理“上线但还不能宣告 running”的中间态：
                    // 录像补启动、元数据回写和时长控制都可能要求继续轮询。
                    let wall_clock_duration = resolved_spec_from_handle(&handle)
                        .is_some_and(|spec| spec.stream_ingest_uses_wall_clock_record_duration());
                    let binding = stream_binding_from_handle(&handle)
                        .unwrap_or_else(|| stream_status.binding.clone());
                    let mut recording_started = false;
                    let mut active_handle = handle.clone();
                    let mut notifications = Vec::new();
                    if let Some(recording) = live_relay_recording_from_handle(&handle)
                        .filter(should_start_live_relay_recording)
                    {
                        // 录制启动失败不一定终止接入任务：配置允许降级时继续提供播放链路，
                        // 但会把错误写入 runtime metadata 并上报 recording_degraded。
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
                                active_handle.last_progress_at = Some(Utc::now());
                                active_handle.metadata["stream_online"] = json!(true);
                                clear_source_reconnecting(&mut active_handle);
                                active_handle.metadata["stream_binding"] = json!({
                                    "schema": binding.schema,
                                    "vhost": binding.vhost,
                                    "app": binding.app,
                                    "stream": binding.stream,
                                });
                                active_handle.metadata["recording"] =
                                    json!(updated_recording.clone());
                                active_handle.metadata["recording_error"] = Value::Null;
                                notifications.push(RuntimeNotification::TaskEvent(
                                    RuntimeTaskEvent {
                                        task_id: active_handle.task_id,
                                        attempt_no: active_handle.attempt_no,
                                        lease_token: runtime_lease_token(&active_handle)
                                            .unwrap_or_default(),
                                        session_epoch: runtime_session_epoch(&active_handle),
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
                                    },
                                ));
                                recording_started = true;
                            }
                            Err(error) => {
                                let failed_recording = mark_recording_failed(&recording);
                                let fatal = should_fail_on_recording_start_error(&recording);
                                active_handle.last_progress_at = Some(Utc::now());
                                active_handle.metadata["stream_online"] = json!(true);
                                clear_source_reconnecting(&mut active_handle);
                                active_handle.metadata["stream_binding"] = json!({
                                    "schema": binding.schema,
                                    "vhost": binding.vhost,
                                    "app": binding.app,
                                    "stream": binding.stream,
                                });
                                active_handle.metadata["recording_error"] =
                                    json!(error.to_string());
                                active_handle.metadata["recording"] = json!(failed_recording);
                                if fatal {
                                    active_handle.metadata["recording_fatal_error"] =
                                        json!(error.to_string());
                                }
                                notifications.push(RuntimeNotification::TaskEvent(
                                    RuntimeTaskEvent {
                                        task_id: active_handle.task_id,
                                        attempt_no: active_handle.attempt_no,
                                        lease_token: runtime_lease_token(&active_handle)
                                            .unwrap_or_default(),
                                        session_epoch: runtime_session_epoch(&active_handle),
                                        event_type: "zlm_api_error".to_string(),
                                        event_level: "error".to_string(),
                                        message: format!(
                                            "failed to start stream recording: {error}"
                                        ),
                                        payload: json!({
                                            "schema": binding.schema,
                                            "vhost": binding.vhost,
                                            "app": binding.app,
                                            "stream": binding.stream,
                                            "record_root": recording.primary_root_path(),
                                            "record_roots": recording.root_paths_payload(),
                                            "duration_sec": recording.duration_sec,
                                        }),
                                    },
                                ));
                                if fatal {
                                    notifications.push(RuntimeNotification::TaskSnapshot(
                                        active_handle.clone(),
                                    ));
                                    let process = process_identity_from_handle(&active_handle)
                                        .unwrap_or_else(|| ProcessIdentity::pid_only(pid));
                                    let _ = signal_process(&process, libc::SIGTERM);
                                    monitor_handle
                                        .send_event(RuntimeInternalEvent::StartupProbeFailed(
                                            RuntimeMonitorCommit::new(
                                                active_handle,
                                                monitor_handle.generation(),
                                            )
                                            .with_persist(work_dir.clone(), success_check.clone())
                                            .with_notifications(notifications),
                                        ))
                                        .await;
                                    return;
                                }
                                notifications.push(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                                    task_id: active_handle.task_id,
                                    attempt_no: active_handle.attempt_no,
                                    lease_token: runtime_lease_token(&active_handle)
                                        .unwrap_or_default(),
                                    session_epoch: runtime_session_epoch(&active_handle),
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
                                notifications
                                    .push(RuntimeNotification::TaskSnapshot(active_handle.clone()));
                            }
                        }
                    }

                    let startup_ready = live_relay_startup_ready(&active_handle);
                    if !startup_ready {
                        // 某些 stream_ingest 需要等录像状态也稳定后才算启动成功；
                        // 在 ready 前只提交快照，不把任务提升为 Running。
                        let duration_handle = active_handle.clone();
                        monitor_handle
                            .send_event(RuntimeInternalEvent::ApplyMonitorCommit(
                                RuntimeMonitorCommit::new(
                                    active_handle,
                                    monitor_handle.generation(),
                                )
                                .with_persist(work_dir.clone(), success_check.clone())
                                .with_notifications(notifications),
                            ))
                            .await;
                        if notify_live_relay_record_duration_if_reached(
                            &monitor_handle,
                            &duration_handle,
                        )
                        .await
                        {
                            return;
                        }
                        sleep(STARTUP_PROBE_POLL_INTERVAL).await;
                        continue;
                    }

                    let should_emit_running = !startup_completed
                        || active_handle.state != RuntimeState::Running
                        || !stream_online(&active_handle)
                        || recording_started;
                    if should_emit_running {
                        active_handle.state = RuntimeState::Running;
                        active_handle.last_progress_at = Some(Utc::now());
                        active_handle.metadata["stream_online"] = json!(true);
                        clear_source_reconnecting(&mut active_handle);
                        active_handle.metadata["stream_binding"] = json!({
                            "schema": startup_probe.schema,
                            "vhost": startup_probe.vhost,
                            "app": startup_probe.app,
                            "stream": startup_probe.stream,
                        });
                        notifications.push(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                            task_id: active_handle.task_id,
                            attempt_no: active_handle.attempt_no,
                            lease_token: runtime_lease_token(&active_handle).unwrap_or_default(),
                            session_epoch: runtime_session_epoch(&active_handle),
                            event_type: "running".to_string(),
                            event_level: "info".to_string(),
                            message: "ZLM stream is online".to_string(),
                            payload: json!({
                                "runtime_id": active_handle.runtime_id,
                                "pid": active_handle.pid,
                                "schema": startup_probe.schema,
                                "vhost": startup_probe.vhost,
                                "app": startup_probe.app,
                                "stream": startup_probe.stream,
                                "recording_started": recording_started,
                            }),
                        }));
                        notifications
                            .push(RuntimeNotification::TaskSnapshot(active_handle.clone()));
                    }
                    let duration_handle = active_handle.clone();
                    if !startup_completed || should_emit_running {
                        let commit =
                            RuntimeMonitorCommit::new(active_handle, monitor_handle.generation())
                                .with_persist(work_dir.clone(), success_check.clone())
                                .with_notifications(notifications);
                        // 首次 ready 走 StartupProbeSucceeded，之后的心跳式更新只做普通 commit。
                        if startup_completed {
                            monitor_handle
                                .send_event(RuntimeInternalEvent::ApplyMonitorCommit(commit))
                                .await;
                        } else {
                            monitor_handle
                                .send_event(RuntimeInternalEvent::StartupProbeSucceeded(commit))
                                .await;
                        }
                    }
                    startup_completed = true;
                    if notify_live_relay_record_duration_if_reached(
                        &monitor_handle,
                        &duration_handle,
                    )
                    .await
                    {
                        return;
                    }
                    if !wall_clock_duration {
                        return;
                    }
                    sleep(STARTUP_PROBE_POLL_INTERVAL).await;
                    continue;
                } else if !startup_completed && started_at.elapsed() >= STARTUP_PROBE_TIMEOUT {
                    // 启动期超时只在尚未成功过时生效；成功后的长期在线维护交给
                    // live relay/RTP 常态监控，避免两个监控器同时裁决同一 runtime。
                    let mut timeout_handle = handle.clone();
                    timeout_handle.metadata["startup_timeout"] = json!(true);
                    if sticky_reconnect_stream_ingest_from_handle(&timeout_handle) {
                        let emit_event =
                            should_emit_source_reconnecting(&timeout_handle, "startup_timeout");
                        let emit_gap_started = should_emit_recording_gap_started(&timeout_handle);
                        mark_source_reconnecting(&mut timeout_handle, "startup_timeout");
                        let mut notifications = Vec::new();
                        if emit_event {
                            notifications.push(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                                task_id: timeout_handle.task_id,
                                attempt_no: timeout_handle.attempt_no,
                                lease_token: runtime_lease_token(&timeout_handle)
                                    .unwrap_or_default(),
                                session_epoch: runtime_session_epoch(&timeout_handle),
                                event_type: "source_reconnecting".to_string(),
                                event_level: "warn".to_string(),
                                message: format!(
                                    "ZLM stream {}/{}/{} is not online yet; continuing to retry",
                                    startup_probe.vhost, startup_probe.app, startup_probe.stream
                                ),
                                payload: json!({
                                    "runtime_id": timeout_handle.runtime_id,
                                    "schema": startup_probe.schema,
                                    "vhost": startup_probe.vhost,
                                    "app": startup_probe.app,
                                    "stream": startup_probe.stream,
                                    "reason": "startup_timeout",
                                }),
                            }));
                            notifications
                                .push(RuntimeNotification::TaskSnapshot(timeout_handle.clone()));
                        }
                        if emit_gap_started {
                            emit_recording_gap_started_event(
                                &events,
                                &timeout_handle,
                                "startup_timeout",
                                json!({
                                    "runtime_id": timeout_handle.runtime_id,
                                    "schema": startup_probe.schema,
                                    "vhost": startup_probe.vhost,
                                    "app": startup_probe.app,
                                    "stream": startup_probe.stream,
                                }),
                            );
                        }
                        monitor_handle
                            .send_event(RuntimeInternalEvent::ApplyMonitorCommit(
                                RuntimeMonitorCommit::new(
                                    timeout_handle,
                                    monitor_handle.generation(),
                                )
                                .with_persist(work_dir.clone(), success_check.clone())
                                .with_notifications(notifications),
                            ))
                            .await;
                        sleep(STARTUP_PROBE_POLL_INTERVAL).await;
                        continue;
                    }
                    timeout_handle.metadata["stream_online"] = json!(false);
                    let notifications = vec![RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                        task_id: timeout_handle.task_id,
                        attempt_no: timeout_handle.attempt_no,
                        lease_token: runtime_lease_token(&timeout_handle).unwrap_or_default(),
                        session_epoch: runtime_session_epoch(&timeout_handle),
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
                    })];
                    let process = process_identity_from_handle(&timeout_handle)
                        .unwrap_or_else(|| ProcessIdentity::pid_only(pid));
                    let _ = signal_process(&process, libc::SIGTERM);
                    monitor_handle
                        .send_event(RuntimeInternalEvent::StartupProbeFailed(
                            RuntimeMonitorCommit::new(timeout_handle, monitor_handle.generation())
                                .with_persist(work_dir.clone(), success_check.clone())
                                .with_notifications(notifications),
                        ))
                        .await;
                    return;
                }

                sleep(STARTUP_PROBE_POLL_INTERVAL).await;
            }
        }
    });
}
