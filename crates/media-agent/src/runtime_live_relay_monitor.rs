//! Live relay 常态监控：跟踪 ZLM 代理流在线/离线状态并回写 runtime。
//!
//! 这里只保留 live relay 启动后主循环调度，包括轮询 ZLM 状态、分派录制补启动、时长停止、
//! 启动超时、离线阈值和停止请求；具体状态回写与事件收口由相邻辅助模块处理。

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
    runtime_events::{RuntimeEventSink, RuntimeNotification},
    runtime_live_relay_events::{
        LiveRelayEventStream, emit_live_relay_terminal_event, live_relay_stopped_terminal_event,
    },
    runtime_live_relay_recording::notify_live_relay_record_duration_if_reached,
    runtime_manager::{RuntimeInternalEvent, RuntimeMonitorCommit, RuntimeMonitorHandle},
    runtime_metadata::{
        StreamBinding, clear_source_reconnecting, emit_recording_gap_started_event,
        live_relay_recording_from_handle, mark_source_reconnecting,
        should_emit_recording_gap_started, should_emit_source_reconnecting,
        sticky_reconnect_stream_ingest_from_handle, stream_binding_from_handle, stream_online,
    },
    runtime_recording::{
        mark_recording_failed, should_fail_on_recording_start_error,
        should_start_live_relay_recording,
    },
    runtime_recovery::next_live_relay_offline_polls,
    runtime_zlm::zlm_stream_status,
};

use crate::runtime_live_relay_cleanup::cleanup_live_relay_runtime;

pub(crate) fn spawn_live_relay_monitor(
    work_dir: PathBuf,
    startup_probe: StartupProbe,
    settings: AgentSettings,
    http_client: Client,
    events: RuntimeEventSink,
    monitor_handle: RuntimeMonitorHandle,
) {
    tokio::spawn(async move {
        {
            let started_at = tokio::time::Instant::now();
            let mut offline_polls = 0_u32;
            loop {
                let Some(snapshot) = monitor_handle.snapshot().await else {
                    return;
                };
                let stop_requested = snapshot.stop_requested;
                let handle = snapshot.handle;
                // 显式停止请求优先于所有在线探测，先关闭 ZLM 侧代理流，再把 runtime 收口为终态。
                if stop_requested {
                    let binding = stream_binding_from_handle(&handle).unwrap_or(StreamBinding {
                        schema: startup_probe.schema.clone(),
                        vhost: startup_probe.vhost.clone(),
                        app: startup_probe.app.clone(),
                        stream: startup_probe.stream.clone(),
                    });
                    cleanup_live_relay_runtime(&http_client, &settings, &handle, &binding).await;
                    let mut exited_handle = handle.clone();
                    exited_handle.state = RuntimeState::Exited;
                    exited_handle.last_progress_at = Some(Utc::now());
                    exited_handle.metadata["stream_online"] = json!(false);
                    clear_source_reconnecting(&mut exited_handle);
                    emit_live_relay_terminal_event(
                        &events,
                        &exited_handle,
                        LiveRelayEventStream::from(&binding),
                        live_relay_stopped_terminal_event(&exited_handle),
                        false,
                    );
                    monitor_handle
                        .send_event(RuntimeInternalEvent::LiveRelayOffline(
                            RuntimeMonitorCommit::new(
                                exited_handle.clone(),
                                monitor_handle.generation(),
                            )
                            .with_persist(work_dir.clone(), SuccessCheck::ProcessExit)
                            .with_notifications(vec![RuntimeNotification::TaskSnapshot(
                                exited_handle,
                            )])
                            .terminal(),
                        ))
                        .await;
                    return;
                }

                // 常态监控只以 ZLM 当前可见状态为准；错误和缺流都先折算成离线计数，
                // 由阈值函数决定是否进入重连或退出，避免单次探测抖动误杀任务。
                let stream_status =
                    zlm_stream_status(&http_client, &settings, &startup_probe).await;
                let stream_state = stream_status
                    .as_ref()
                    .map(|status| status.is_some())
                    .map_err(|_| ());
                let stream_was_online = stream_online(&handle);
                let (next_offline_polls, offline_threshold_reached) =
                    next_live_relay_offline_polls(offline_polls, stream_was_online, stream_state);
                match stream_status {
                    Ok(Some(stream_status)) => {
                        // 流重新可见时刷新 runtime/read-model，并补齐 stream_binding 供后续事件、
                        // 清理和录制控制复用；只有状态发生变化时才发送 running 事件。
                        offline_polls = next_offline_polls;
                        let binding = stream_binding_from_handle(&handle)
                            .unwrap_or_else(|| stream_status.binding.clone());
                        let mut running_handle = handle.clone();
                        let should_emit_running = running_handle.state != RuntimeState::Running
                            || !stream_online(&running_handle);
                        running_handle.state = RuntimeState::Running;
                        running_handle.last_progress_at = Some(Utc::now());
                        running_handle.metadata["stream_online"] = json!(true);
                        clear_source_reconnecting(&mut running_handle);
                        running_handle.metadata["stream_binding"] = json!({
                            "schema": binding.schema,
                            "vhost": binding.vhost,
                            "app": binding.app,
                            "stream": binding.stream,
                        });
                        let mut notifications = Vec::new();
                        let mut recording_started = false;
                        // live relay 的录制启动依赖流已上线；失败时按任务策略决定是降级运行，
                        // 还是把启动视为致命错误并让 manager 走终态提交。
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
                                    running_handle.metadata["recording"] =
                                        json!(updated_recording.clone());
                                    running_handle.metadata["recording_error"] = Value::Null;
                                    notifications.push(RuntimeNotification::TaskEvent(
                                        crate::runtime_events::RuntimeTaskEvent {
                                            task_id: running_handle.task_id,
                                            attempt_no: running_handle.attempt_no,
                                            lease_token: crate::runtime_metadata::runtime_lease_token(
                                                &running_handle,
                                            )
                                            .unwrap_or_default(),
                                            session_epoch:
                                                crate::runtime_events::runtime_session_epoch(
                                                    &running_handle,
                                                ),
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
                                        },
                                    ));
                                    recording_started = true;
                                }
                                Err(error) => {
                                    let failed_recording = mark_recording_failed(&recording);
                                    let fatal = should_fail_on_recording_start_error(&recording);
                                    running_handle.metadata["recording_error"] =
                                        json!(error.to_string());
                                    running_handle.metadata["recording"] = json!(failed_recording);
                                    if fatal {
                                        running_handle.metadata["recording_fatal_error"] =
                                            json!(error.to_string());
                                    }
                                    notifications.push(RuntimeNotification::TaskEvent(
                                        crate::runtime_events::RuntimeTaskEvent {
                                            task_id: running_handle.task_id,
                                            attempt_no: running_handle.attempt_no,
                                            lease_token:
                                                crate::runtime_metadata::runtime_lease_token(
                                                    &running_handle,
                                                )
                                                .unwrap_or_default(),
                                            session_epoch:
                                                crate::runtime_events::runtime_session_epoch(
                                                    &running_handle,
                                                ),
                                            event_type: "zlm_api_error".to_string(),
                                            event_level: "error".to_string(),
                                            message: format!(
                                                "failed to start live_relay recording: {error}"
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
                                            running_handle.clone(),
                                        ));
                                        monitor_handle
                                            .send_event(RuntimeInternalEvent::StartupProbeFailed(
                                                RuntimeMonitorCommit::new(
                                                    running_handle,
                                                    monitor_handle.generation(),
                                                )
                                                .with_persist(
                                                    work_dir.clone(),
                                                    SuccessCheck::ProcessExit,
                                                )
                                                .with_notifications(notifications)
                                                .terminal(),
                                            ))
                                            .await;
                                        return;
                                    }
                                    notifications.push(RuntimeNotification::TaskEvent(
                                        crate::runtime_events::RuntimeTaskEvent {
                                            task_id: running_handle.task_id,
                                            attempt_no: running_handle.attempt_no,
                                            lease_token: crate::runtime_metadata::runtime_lease_token(
                                                &running_handle,
                                            )
                                            .unwrap_or_default(),
                                            session_epoch:
                                                crate::runtime_events::runtime_session_epoch(
                                                    &running_handle,
                                                ),
                                            event_type: "recording_degraded".to_string(),
                                            event_level: "warn".to_string(),
                                            message:
                                                "live_relay recording startup failed; continuing without recording"
                                                    .to_string(),
                                            payload: json!({
                                                "schema": binding.schema,
                                                "vhost": binding.vhost,
                                                "app": binding.app,
                                                "stream": binding.stream,
                                                "record_root": recording.primary_root_path(),
                                                "record_roots": recording.root_paths_payload(),
                                            }),
                                        },
                                    ));
                                }
                            }
                        }
                        if should_emit_running {
                            notifications.push(RuntimeNotification::TaskEvent(
                                crate::runtime_events::RuntimeTaskEvent {
                                    task_id: running_handle.task_id,
                                    attempt_no: running_handle.attempt_no,
                                    lease_token: crate::runtime_metadata::runtime_lease_token(
                                        &running_handle,
                                    )
                                    .unwrap_or_default(),
                                    session_epoch: crate::runtime_events::runtime_session_epoch(
                                        &running_handle,
                                    ),
                                    event_type: "running".to_string(),
                                    event_level: "info".to_string(),
                                    message: "ZLM live_relay stream is online".to_string(),
                                    payload: json!({
                                        "runtime_id": running_handle.runtime_id,
                                        "schema": binding.schema,
                                        "vhost": binding.vhost,
                                        "app": binding.app,
                                        "stream": binding.stream,
                                        "recording_started": recording_started,
                                    }),
                                },
                            ));
                            notifications
                                .push(RuntimeNotification::TaskSnapshot(running_handle.clone()));
                        }
                        let duration_handle = running_handle.clone();
                        monitor_handle
                            .send_event(RuntimeInternalEvent::ApplyMonitorCommit(
                                RuntimeMonitorCommit::new(
                                    running_handle,
                                    monitor_handle.generation(),
                                )
                                .with_persist(work_dir.clone(), SuccessCheck::ProcessExit)
                                .with_notifications(notifications),
                            ))
                            .await;
                        if notify_live_relay_record_duration_if_reached(
                            &monitor_handle,
                            &duration_handle,
                        )
                        .await
                        {
                            sleep(STARTUP_PROBE_POLL_INTERVAL).await;
                            continue;
                        }
                    }
                    Ok(None)
                        if !stream_online(&handle)
                            && started_at.elapsed() >= STARTUP_PROBE_TIMEOUT =>
                    {
                        // 首次上线超时分两条路：粘性重连任务继续保活并暴露 reconnecting，
                        // 普通任务直接失败，防止永久占用 runtime slot。
                        if sticky_reconnect_stream_ingest_from_handle(&handle) {
                            let emit_event =
                                should_emit_source_reconnecting(&handle, "startup_timeout");
                            let emit_gap_started = should_emit_recording_gap_started(&handle);
                            let mut reconnecting_handle = handle.clone();
                            reconnecting_handle.metadata["startup_timeout"] = json!(true);
                            mark_source_reconnecting(&mut reconnecting_handle, "startup_timeout");
                            let mut notifications = Vec::new();
                            if emit_event {
                                notifications.push(RuntimeNotification::TaskEvent(
                                    crate::runtime_events::RuntimeTaskEvent {
                                        task_id: reconnecting_handle.task_id,
                                        attempt_no: reconnecting_handle.attempt_no,
                                        lease_token:
                                            crate::runtime_metadata::runtime_lease_token(
                                                &reconnecting_handle,
                                            )
                                            .unwrap_or_default(),
                                        session_epoch:
                                            crate::runtime_events::runtime_session_epoch(
                                                &reconnecting_handle,
                                            ),
                                        event_type: "source_reconnecting".to_string(),
                                        event_level: "warn".to_string(),
                                        message: format!(
                                            "live_relay stream {}/{}/{} is not online yet; continuing to retry",
                                            startup_probe.vhost,
                                            startup_probe.app,
                                            startup_probe.stream
                                        ),
                                        payload: json!({
                                            "runtime_id": reconnecting_handle.runtime_id,
                                            "schema": startup_probe.schema,
                                            "vhost": startup_probe.vhost,
                                            "app": startup_probe.app,
                                            "stream": startup_probe.stream,
                                            "reason": "startup_timeout",
                                        }),
                                    },
                                ));
                                notifications.push(RuntimeNotification::TaskSnapshot(
                                    reconnecting_handle.clone(),
                                ));
                            }
                            if emit_gap_started {
                                emit_recording_gap_started_event(
                                    &events,
                                    &reconnecting_handle,
                                    "startup_timeout",
                                    json!({
                                        "runtime_id": reconnecting_handle.runtime_id,
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
                                        reconnecting_handle,
                                        monitor_handle.generation(),
                                    )
                                    .with_persist(work_dir.clone(), SuccessCheck::ProcessExit)
                                    .with_notifications(notifications),
                                ))
                                .await;
                            sleep(STARTUP_PROBE_POLL_INTERVAL).await;
                            continue;
                        }
                        let mut failed_handle = handle.clone();
                        failed_handle.state = RuntimeState::Exited;
                        failed_handle.last_progress_at = Some(Utc::now());
                        failed_handle.metadata["startup_timeout"] = json!(true);
                        failed_handle.metadata["stream_online"] = json!(false);
                        let notifications = vec![
                            RuntimeNotification::TaskEvent(
                                crate::runtime_events::RuntimeTaskEvent {
                                    task_id: failed_handle.task_id,
                                    attempt_no: failed_handle.attempt_no,
                                    lease_token: crate::runtime_metadata::runtime_lease_token(
                                        &failed_handle,
                                    )
                                    .unwrap_or_default(),
                                    session_epoch: crate::runtime_events::runtime_session_epoch(
                                        &failed_handle,
                                    ),
                                    event_type: "startup_timeout".to_string(),
                                    event_level: "error".to_string(),
                                    message: format!(
                                        "live_relay stream {}/{}/{} did not become online within {} seconds",
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
                                },
                            ),
                            RuntimeNotification::TaskSnapshot(failed_handle.clone()),
                        ];
                        monitor_handle
                            .send_event(RuntimeInternalEvent::StartupProbeFailed(
                                RuntimeMonitorCommit::new(
                                    failed_handle,
                                    monitor_handle.generation(),
                                )
                                .with_persist(work_dir.clone(), SuccessCheck::ProcessExit)
                                .with_notifications(notifications)
                                .terminal(),
                            ))
                            .await;
                        return;
                    }
                    Ok(None) if stream_was_online => {
                        // 已运行过的 live relay 掉线后先给 ZLM 几个轮询周期恢复；达到阈值后，
                        // 粘性重连任务进入 source_reconnecting，非粘性任务才真正退出。
                        offline_polls = next_offline_polls;
                        if !offline_threshold_reached {
                            sleep(STARTUP_PROBE_POLL_INTERVAL).await;
                            continue;
                        }
                        if sticky_reconnect_stream_ingest_from_handle(&handle) {
                            let emit_event =
                                should_emit_source_reconnecting(&handle, "source_disconnected");
                            let emit_gap_started = should_emit_recording_gap_started(&handle);
                            let mut reconnecting_handle = handle.clone();
                            mark_source_reconnecting(
                                &mut reconnecting_handle,
                                "source_disconnected",
                            );
                            let mut notifications = Vec::new();
                            if emit_event {
                                notifications.push(RuntimeNotification::TaskEvent(
                                    crate::runtime_events::RuntimeTaskEvent {
                                        task_id: reconnecting_handle.task_id,
                                        attempt_no: reconnecting_handle.attempt_no,
                                        lease_token:
                                            crate::runtime_metadata::runtime_lease_token(
                                                &reconnecting_handle,
                                            )
                                            .unwrap_or_default(),
                                        session_epoch:
                                            crate::runtime_events::runtime_session_epoch(
                                                &reconnecting_handle,
                                            ),
                                        event_type: "source_reconnecting".to_string(),
                                        event_level: "warn".to_string(),
                                        message:
                                            "live_relay stream went offline; waiting for ZLM reconnect"
                                                .to_string(),
                                        payload: json!({
                                            "runtime_id": reconnecting_handle.runtime_id,
                                            "schema": startup_probe.schema,
                                            "vhost": startup_probe.vhost,
                                            "app": startup_probe.app,
                                            "stream": startup_probe.stream,
                                            "reason": "source_disconnected",
                                            "orphaned": reconnecting_handle.metadata.get("orphaned").and_then(Value::as_bool).unwrap_or(false),
                                        }),
                                    },
                                ));
                                notifications.push(RuntimeNotification::TaskSnapshot(
                                    reconnecting_handle.clone(),
                                ));
                            }
                            if emit_gap_started {
                                emit_recording_gap_started_event(
                                    &events,
                                    &reconnecting_handle,
                                    "source_disconnected",
                                    json!({
                                        "runtime_id": reconnecting_handle.runtime_id,
                                        "schema": startup_probe.schema,
                                        "vhost": startup_probe.vhost,
                                        "app": startup_probe.app,
                                        "stream": startup_probe.stream,
                                        "orphaned": reconnecting_handle.metadata.get("orphaned").and_then(Value::as_bool).unwrap_or(false),
                                    }),
                                );
                            }
                            monitor_handle
                                .send_event(RuntimeInternalEvent::ApplyMonitorCommit(
                                    RuntimeMonitorCommit::new(
                                        reconnecting_handle,
                                        monitor_handle.generation(),
                                    )
                                    .with_persist(work_dir.clone(), SuccessCheck::ProcessExit)
                                    .with_notifications(notifications),
                                ))
                                .await;
                            sleep(STARTUP_PROBE_POLL_INTERVAL).await;
                            continue;
                        }
                        let mut exited_handle = handle.clone();
                        exited_handle.state = RuntimeState::Exited;
                        exited_handle.last_progress_at = Some(Utc::now());
                        exited_handle.metadata["stream_online"] = json!(false);
                        emit_live_relay_terminal_event(
                            &events,
                            &exited_handle,
                            LiveRelayEventStream::from(&startup_probe),
                            crate::runtime_live_relay_events::live_relay_offline_terminal_event(
                                &settings,
                                &handle,
                                &exited_handle,
                                stop_requested,
                            ),
                            true,
                        );
                        monitor_handle
                            .send_event(RuntimeInternalEvent::LiveRelayOffline(
                                RuntimeMonitorCommit::new(
                                    exited_handle.clone(),
                                    monitor_handle.generation(),
                                )
                                .with_persist(work_dir.clone(), SuccessCheck::ProcessExit)
                                .with_notifications(vec![RuntimeNotification::TaskSnapshot(
                                    exited_handle,
                                )])
                                .terminal(),
                            ))
                            .await;
                        return;
                    }
                    Ok(None) | Err(_) => {
                        offline_polls = next_offline_polls;
                    }
                }

                sleep(STARTUP_PROBE_POLL_INTERVAL).await;
            }
        }
    });
}
