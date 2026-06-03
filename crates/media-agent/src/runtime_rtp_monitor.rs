//! RTP 接收监控：轮询 ZLM RTP server 状态并维护 rtp_receive runtime 生命周期。
//!
//! 这里只处理 RTP server 丢失、粘性重连、运行态事件和退出态收尾，不混入 live relay 或
//! 普通 FFmpeg 进程监控逻辑。

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
use uuid::Uuid;

use crate::{
    config::AgentSettings,
    runtime::{STARTUP_PROBE_POLL_INTERVAL, SuccessCheck},
    runtime_events::{
        RuntimeEventSink, RuntimeNotification, RuntimeTaskEvent, runtime_session_epoch,
    },
    runtime_metadata::{
        RtpServerMetadata, clear_source_reconnecting, emit_recording_gap_ended_event,
        emit_recording_gap_started_event, emit_source_reconnecting_event, mark_source_reconnecting,
        rtp_server_from_handle, runtime_lease_token, should_emit_recording_gap_started,
        should_emit_source_reconnecting, sticky_reconnect_stream_ingest_from_handle, stream_online,
    },
    runtime_persistence::persist_runtime_state,
    runtime_plan::build_open_rtp_server_params_from_metadata,
    runtime_process::{ManagedRuntime, remove_managed_runtime},
    runtime_recovery::next_rtp_server_missing_polls,
    runtime_registry::LocalRuntimeRegistry,
    runtime_zlm::{
        call_zlm_api, extract_zlm_local_port, zlm_rtp_server_port, zlm_stream_binding_by_stream_id,
    },
};

pub(crate) fn spawn_rtp_receive_monitor(
    runtime_id: Uuid,
    work_dir: PathBuf,
    stream_id: String,
    settings: AgentSettings,
    http_client: Client,
    registry: LocalRuntimeRegistry,
    runtimes: Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    events: RuntimeEventSink,
) {
    tokio::spawn(async move {
        let mut missing_polls = 0_u32;
        loop {
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
            let stop_requested = runtime.stop_requested.load(Ordering::Relaxed);
            let handle = registry.get(runtime_id);
            let Some(handle) = handle else {
                let _ = remove_managed_runtime(&runtimes, runtime_id);
                return;
            };

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
                    missing_polls = next_missing_polls;
                    let should_emit_running =
                        handle.state != RuntimeState::Running || !stream_online(&handle);
                    if should_emit_running {
                        if let Ok(Some(binding)) =
                            zlm_stream_binding_by_stream_id(&http_client, &settings, &stream_id)
                                .await
                        {
                            emit_recording_gap_ended_event(
                                &events,
                                &handle,
                                "source_reconnected",
                                json!({
                                    "rtp_stream_id": stream_id.clone(),
                                    "local_port": local_port,
                                    "schema": binding.schema,
                                    "vhost": binding.vhost,
                                    "app": binding.app,
                                    "stream": binding.stream,
                                }),
                            );
                            let running_handle = registry
                                .update(runtime_id, |runtime| {
                                    runtime.state = RuntimeState::Running;
                                    runtime.last_progress_at = Some(Utc::now());
                                    runtime.metadata["stream_online"] = json!(true);
                                    clear_source_reconnecting(runtime);
                                    runtime.metadata["stream_binding"] = json!({
                                            "schema": binding.schema,
                                            "vhost": binding.vhost,
                                            "app": binding.app,
                                        "stream": binding.stream,
                                    });
                                    if let Some(mut rtp_server) = runtime
                                        .metadata
                                        .get("rtp_server")
                                        .cloned()
                                        .and_then(|value| {
                                            serde_json::from_value::<RtpServerMetadata>(value).ok()
                                        })
                                    {
                                        rtp_server.local_port = local_port;
                                        runtime.metadata["rtp_server"] = json!(rtp_server);
                                    }
                                })
                                .unwrap_or_else(|| handle.clone());
                            let _ = persist_runtime_state(
                                &work_dir,
                                &running_handle,
                                &SuccessCheck::ProcessExit,
                            );
                            let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
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
                            }));
                            let _ = events.send(RuntimeNotification::TaskSnapshot(running_handle));
                        }
                    }
                }
                Ok(None) => {
                    missing_polls = next_missing_polls;
                    if !missing_threshold_reached {
                        sleep(STARTUP_PROBE_POLL_INTERVAL).await;
                        continue;
                    }
                    if sticky_reconnect_stream_ingest_from_handle(&handle) {
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
                        let reconnecting_handle = registry
                            .update(runtime_id, |runtime| {
                                mark_source_reconnecting(runtime, "rtp_server_missing");
                                runtime.metadata["rtp_server"] = json!(rtp_server.clone());
                            })
                            .unwrap_or_else(|| {
                                let mut handle = handle.clone();
                                mark_source_reconnecting(&mut handle, "rtp_server_missing");
                                handle.metadata["rtp_server"] = json!(rtp_server.clone());
                                handle
                            });
                        let _ = persist_runtime_state(
                            &work_dir,
                            &reconnecting_handle,
                            &SuccessCheck::ProcessExit,
                        );
                        if emit_event {
                            emit_source_reconnecting_event(
                                &events,
                                &reconnecting_handle,
                                "rtp_receive server disappeared; reopening and waiting for media",
                                json!({
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
                            );
                            let _ = events.send(RuntimeNotification::TaskSnapshot(
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
                        missing_polls = 0;
                        sleep(STARTUP_PROBE_POLL_INTERVAL).await;
                        continue;
                    }
                    let _ = remove_managed_runtime(&runtimes, runtime_id);
                    let exited_handle = registry
                        .update(runtime_id, |runtime| {
                            runtime.state = RuntimeState::Exited;
                            runtime.last_progress_at = Some(Utc::now());
                            runtime.metadata["stream_online"] = json!(false);
                        })
                        .unwrap_or_else(|| {
                            let mut handle = handle.clone();
                            handle.state = RuntimeState::Exited;
                            handle.last_progress_at = Some(Utc::now());
                            handle.metadata["stream_online"] = json!(false);
                            handle
                        });
                    let _ = persist_runtime_state(
                        &work_dir,
                        &exited_handle,
                        &SuccessCheck::ProcessExit,
                    );
                    if !stop_requested {
                        let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                            task_id: exited_handle.task_id,
                            attempt_no: exited_handle.attempt_no,
                            lease_token: runtime_lease_token(&exited_handle).unwrap_or_default(),
                            session_epoch: runtime_session_epoch(&exited_handle),
                            event_type: "rtp_server_closed".to_string(),
                            event_level: "warn".to_string(),
                            message: "rtp_receive server disappeared from ZLM".to_string(),
                            payload: json!({
                                "rtp_stream_id": stream_id.clone(),
                                "orphaned": exited_handle.metadata.get("orphaned").and_then(Value::as_bool).unwrap_or(false),
                            }),
                        }));
                        let _ =
                            events.send(RuntimeNotification::TaskSnapshot(exited_handle.clone()));
                    }
                    let _ = registry.remove(runtime_id);
                    return;
                }
                Err(_) => {
                    missing_polls = next_missing_polls;
                }
            }

            sleep(STARTUP_PROBE_POLL_INTERVAL).await;
        }
    });
}
