//! RTP 接收监控：轮询 ZLM RTP server 状态并维护 rtp_receive runtime 生命周期。
//!
//! 这里只处理 RTP server 丢失、粘性重连、运行态事件和退出态收尾，不混入 live relay 或
//! 普通 FFmpeg 进程监控逻辑。

use std::path::PathBuf;

use chrono::Utc;
use media_domain::RuntimeState;
use reqwest::Client;
use serde_json::{Value, json};
use tokio::time::sleep;

use crate::{
    config::AgentSettings,
    runtime::{STARTUP_PROBE_POLL_INTERVAL, SuccessCheck},
    runtime_events::{
        RuntimeEventSink, RuntimeNotification, RuntimeTaskEvent, runtime_session_epoch,
    },
    runtime_manager::{RuntimeInternalEvent, RuntimeMonitorCommit, RuntimeMonitorHandle},
    runtime_metadata::{
        RtpServerMetadata, clear_source_reconnecting, emit_recording_gap_started_event,
        mark_source_reconnecting, rtp_server_from_handle, runtime_lease_token,
        should_emit_recording_gap_started, should_emit_source_reconnecting,
        sticky_reconnect_stream_ingest_from_handle, stream_online,
    },
    runtime_plan::build_open_rtp_server_params_from_metadata,
    runtime_recovery::next_rtp_server_missing_polls,
    runtime_zlm::{
        call_zlm_api, extract_zlm_local_port, zlm_rtp_server_port, zlm_stream_binding_by_stream_id,
    },
};

pub(crate) fn spawn_rtp_receive_monitor(
    work_dir: PathBuf,
    stream_id: String,
    settings: AgentSettings,
    http_client: Client,
    events: RuntimeEventSink,
    monitor_handle: RuntimeMonitorHandle,
) {
    tokio::spawn(async move {
        {
            let mut missing_polls = 0_u32;
            loop {
                let Some(snapshot) = monitor_handle.snapshot().await else {
                    return;
                };
                let stop_requested = snapshot.stop_requested;
                let handle = snapshot.handle;

                // RTP receive 的权威事实是 ZLM 是否还保留对应 stream_id 的 RTP server。
                // 缺失计数用于过滤 ZLM API 瞬时失败或 server 刚切换时的短暂空窗。
                let server_port = zlm_rtp_server_port(&http_client, &settings, &stream_id).await;
                let (next_missing_polls, missing_threshold_reached) = next_rtp_server_missing_polls(
                    missing_polls,
                    server_port
                        .as_ref()
                        .map(|value| value.is_some())
                        .map_err(|_| ()),
                );
                match server_port {
                    Ok(Some(local_port)) => {
                        // server 仍存在时，只在首次进入 running 或曾经标记离线后才刷新状态，
                        // 防止每个轮询周期都刷 task event。
                        missing_polls = next_missing_polls;
                        let should_emit_running =
                            handle.state != RuntimeState::Running || !stream_online(&handle);
                        if should_emit_running {
                            if let Ok(Some(binding)) =
                                zlm_stream_binding_by_stream_id(&http_client, &settings, &stream_id)
                                    .await
                            {
                                let mut running_handle = handle.clone();
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
                                if let Some(mut rtp_server) =
                                    running_handle.metadata.get("rtp_server").cloned().and_then(
                                        |value| {
                                            serde_json::from_value::<RtpServerMetadata>(value).ok()
                                        },
                                    )
                                {
                                    rtp_server.local_port = local_port;
                                    running_handle.metadata["rtp_server"] = json!(rtp_server);
                                }
                                let notifications = vec![
                                    RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                                        task_id: running_handle.task_id,
                                        attempt_no: running_handle.attempt_no,
                                        lease_token: runtime_lease_token(&running_handle)
                                            .unwrap_or_default(),
                                        session_epoch: runtime_session_epoch(&running_handle),
                                        event_type: "running".to_string(),
                                        event_level: "info".to_string(),
                                        message: "rtp_receive stream is online".to_string(),
                                        payload: json!({
                                            "runtime_id": running_handle.runtime_id,
                                            "rtp_stream_id": stream_id.clone(),
                                            "local_port": local_port,
                                            "schema": binding.schema,
                                            "vhost": binding.vhost,
                                            "app": binding.app,
                                            "stream": binding.stream,
                                        }),
                                    }),
                                    RuntimeNotification::TaskSnapshot(running_handle.clone()),
                                ];
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
                            }
                        }
                    }
                    Ok(None) => {
                        // ZLM 没有这个 RTP server 不一定立即失败：可能是 media server 重启、
                        // orphan adopt 过程或 openRtpServer 短暂丢失，先等阈值再裁决。
                        missing_polls = next_missing_polls;
                        if !missing_threshold_reached {
                            sleep(STARTUP_PROBE_POLL_INTERVAL).await;
                            continue;
                        }
                        if sticky_reconnect_stream_ingest_from_handle(&handle) {
                            // 粘性重连任务会尝试重新 openRtpServer，并继续保留 runtime。
                            // 失败信息只写进事件 payload，下一轮仍有机会再次重开。
                            let emit_event =
                                should_emit_source_reconnecting(&handle, "rtp_server_missing");
                            let emit_gap_started = should_emit_recording_gap_started(&handle);
                            let Some(mut rtp_server) = rtp_server_from_handle(&handle) else {
                                sleep(STARTUP_PROBE_POLL_INTERVAL).await;
                                continue;
                            };
                            let reopen = call_zlm_api(
                                &http_client,
                                &settings,
                                "/index/api/openRtpServer",
                                &build_open_rtp_server_params_from_metadata(&rtp_server),
                            )
                            .await;
                            let reopen_error = match reopen {
                                Ok(response) => {
                                    rtp_server.local_port = extract_zlm_local_port(&response)
                                        .unwrap_or(rtp_server.requested_port);
                                    None
                                }
                                Err(error) => Some(error.to_string()),
                            };
                            let mut reconnecting_handle = handle.clone();
                            mark_source_reconnecting(
                                &mut reconnecting_handle,
                                "rtp_server_missing",
                            );
                            reconnecting_handle.metadata["rtp_server"] = json!(rtp_server.clone());
                            let mut notifications = Vec::new();
                            if emit_event {
                                notifications.push(RuntimeNotification::TaskEvent(
                                    RuntimeTaskEvent {
                                        task_id: reconnecting_handle.task_id,
                                        attempt_no: reconnecting_handle.attempt_no,
                                        lease_token: runtime_lease_token(&reconnecting_handle)
                                            .unwrap_or_default(),
                                        session_epoch: runtime_session_epoch(&reconnecting_handle),
                                        event_type: "source_reconnecting".to_string(),
                                        event_level: "warn".to_string(),
                                        message:
                                            "rtp_receive server disappeared; reopening and waiting for media"
                                                .to_string(),
                                        payload: json!({
                                            "runtime_id": reconnecting_handle.runtime_id,
                                            "rtp_stream_id": stream_id.clone(),
                                            "local_port": rtp_server.local_port,
                                            "requested_port": rtp_server.requested_port,
                                            "re_use_port": rtp_server.reuse_port,
                                            "ssrc": rtp_server.ssrc,
                                            "reason": "rtp_server_missing",
                                            "reopen_error": reopen_error,
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
                                    "rtp_server_missing",
                                    json!({
                                        "runtime_id": reconnecting_handle.runtime_id,
                                        "rtp_stream_id": stream_id.clone(),
                                        "local_port": rtp_server.local_port,
                                        "requested_port": rtp_server.requested_port,
                                        "re_use_port": rtp_server.reuse_port,
                                        "ssrc": rtp_server.ssrc,
                                        "reopen_error": reopen_error,
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
                            missing_polls = 0;
                            sleep(STARTUP_PROBE_POLL_INTERVAL).await;
                            continue;
                        }
                        // 非粘性接入没有可恢复语义；server 达到缺失阈值后收口成终态，
                        // 如果这是主动停止导致的缺失，则跳过告警事件。
                        let mut exited_handle = handle.clone();
                        exited_handle.state = RuntimeState::Exited;
                        exited_handle.last_progress_at = Some(Utc::now());
                        exited_handle.metadata["stream_online"] = json!(false);
                        let mut notifications = Vec::new();
                        if !stop_requested {
                            notifications.push(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                                task_id: exited_handle.task_id,
                                attempt_no: exited_handle.attempt_no,
                                lease_token: runtime_lease_token(&exited_handle)
                                    .unwrap_or_default(),
                                session_epoch: runtime_session_epoch(&exited_handle),
                                event_type: "rtp_server_closed".to_string(),
                                event_level: "warn".to_string(),
                                message: "rtp_receive server disappeared from ZLM".to_string(),
                                payload: json!({
                                    "rtp_stream_id": stream_id.clone(),
                                    "orphaned": exited_handle.metadata.get("orphaned").and_then(Value::as_bool).unwrap_or(false),
                                }),
                            }));
                            notifications
                                .push(RuntimeNotification::TaskSnapshot(exited_handle.clone()));
                        }
                        monitor_handle
                            .send_event(RuntimeInternalEvent::RtpServerMissing(
                                RuntimeMonitorCommit::new(
                                    exited_handle,
                                    monitor_handle.generation(),
                                )
                                .with_persist(work_dir.clone(), SuccessCheck::ProcessExit)
                                .with_notifications(notifications)
                                .terminal(),
                            ))
                            .await;
                        return;
                    }
                    Err(_) => {
                        missing_polls = next_missing_polls;
                    }
                }

                sleep(STARTUP_PROBE_POLL_INTERVAL).await;
            }
        }
    });
}
