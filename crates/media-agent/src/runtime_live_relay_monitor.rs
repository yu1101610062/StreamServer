//! Live relay 常态监控：跟踪 ZLM 代理流在线/离线状态并回写 runtime。
//!
//! 这里只保留 live relay 启动后主循环调度，包括轮询 ZLM 状态、分派录制补启动、时长停止、
//! 启动超时、离线阈值和停止请求；具体状态回写与事件收口由相邻辅助模块处理。

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, RwLock, atomic::Ordering},
};

use chrono::Utc;
use media_domain::RuntimeState;
use reqwest::Client;
use serde_json::json;
use tokio::time::sleep;
use uuid::Uuid;

use crate::{
    config::AgentSettings,
    runtime::{STARTUP_PROBE_POLL_INTERVAL, STARTUP_PROBE_TIMEOUT, StartupProbe, SuccessCheck},
    runtime_events::{RuntimeEventSink, RuntimeNotification},
    runtime_live_relay_events::{
        LiveRelayEventStream, emit_live_relay_terminal_event, live_relay_stopped_terminal_event,
    },
    runtime_live_relay_offline::{
        LiveRelayOfflineContext, LiveRelayOfflineOutcome, handle_live_relay_offline_after_threshold,
    },
    runtime_live_relay_recording::{
        LiveRelayRecordingStartContext, LiveRelayRecordingStartMode,
        LiveRelayRecordingStartOutcome, start_live_relay_recording_from_monitor,
        stop_live_relay_for_record_duration_if_reached,
    },
    runtime_live_relay_running::{
        LiveRelayRunningContext, LiveRelayRunningReadiness, ensure_live_relay_running_if_ready,
    },
    runtime_live_relay_startup_timeout::{
        LiveRelayStartupTimeoutContext, LiveRelayStartupTimeoutMode,
        LiveRelayStartupTimeoutOutcome, handle_live_relay_startup_timeout,
    },
    runtime_metadata::{
        StreamBinding, clear_source_reconnecting, live_relay_recording_from_handle,
        live_relay_uses_recording_startup, stream_binding_from_handle, stream_online,
    },
    runtime_persistence::persist_runtime_state,
    runtime_process::{ManagedRuntime, remove_managed_runtime},
    runtime_recording::should_start_live_relay_recording,
    runtime_recovery::next_live_relay_offline_polls,
    runtime_registry::LocalRuntimeRegistry,
    runtime_zlm::zlm_stream_status,
};

use crate::runtime_live_relay_cleanup::cleanup_live_relay_runtime;

pub(crate) fn spawn_live_relay_monitor(
    runtime_id: Uuid,
    work_dir: PathBuf,
    startup_probe: StartupProbe,
    settings: AgentSettings,
    http_client: Client,
    registry: LocalRuntimeRegistry,
    runtimes: Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    events: RuntimeEventSink,
) {
    tokio::spawn(async move {
        let started_at = tokio::time::Instant::now();
        let mut offline_polls = 0_u32;
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
            if stop_requested {
                let binding = stream_binding_from_handle(&handle).unwrap_or(StreamBinding {
                    schema: startup_probe.schema.clone(),
                    vhost: startup_probe.vhost.clone(),
                    app: startup_probe.app.clone(),
                    stream: startup_probe.stream.clone(),
                });
                cleanup_live_relay_runtime(&http_client, &settings, &handle, &binding).await;
                let exited_handle = registry
                    .update(runtime_id, |runtime| {
                        runtime.state = RuntimeState::Exited;
                        runtime.last_progress_at = Some(Utc::now());
                        runtime.metadata["stream_online"] = json!(false);
                        clear_source_reconnecting(runtime);
                    })
                    .unwrap_or_else(|| {
                        let mut handle = handle.clone();
                        handle.state = RuntimeState::Exited;
                        handle.last_progress_at = Some(Utc::now());
                        handle.metadata["stream_online"] = json!(false);
                        clear_source_reconnecting(&mut handle);
                        handle
                    });
                emit_live_relay_terminal_event(
                    &events,
                    &exited_handle,
                    LiveRelayEventStream::from(&binding),
                    live_relay_stopped_terminal_event(&exited_handle),
                    false,
                );
                let _ =
                    persist_runtime_state(&work_dir, &exited_handle, &SuccessCheck::ProcessExit);
                let _ = events.send(RuntimeNotification::TaskSnapshot(exited_handle.clone()));
                let _ = remove_managed_runtime(&runtimes, runtime_id);
                let _ = registry.remove(runtime_id);
                return;
            }
            let stream_status = zlm_stream_status(&http_client, &settings, &startup_probe).await;

            if live_relay_uses_recording_startup(&startup_probe, &handle) {
                let mut recording_started = false;
                let mut active_handle = handle.clone();
                if let (Ok(Some(stream_status)), Some(recording)) = (
                    stream_status.as_ref(),
                    live_relay_recording_from_handle(&handle)
                        .filter(should_start_live_relay_recording),
                ) {
                    let binding = stream_binding_from_handle(&handle)
                        .unwrap_or_else(|| stream_status.binding.clone());
                    match start_live_relay_recording_from_monitor(
                        LiveRelayRecordingStartContext {
                            runtime_id,
                            work_dir: &work_dir,
                            settings: &settings,
                            http_client: &http_client,
                            registry: &registry,
                            runtimes: &runtimes,
                            events: &events,
                        },
                        &handle,
                        &active_handle,
                        &binding,
                        &recording,
                        LiveRelayRecordingStartMode::RecordingStartup,
                    )
                    .await
                    {
                        LiveRelayRecordingStartOutcome::Updated {
                            handle,
                            recording_started: started,
                        } => {
                            recording_started = started;
                            active_handle = handle;
                        }
                        LiveRelayRecordingStartOutcome::Fatal => return,
                    }
                }

                let handle = registry.get(runtime_id).unwrap_or(active_handle);
                if stop_live_relay_for_record_duration_if_reached(
                    runtime_id,
                    &work_dir,
                    &startup_probe,
                    &settings,
                    &http_client,
                    &registry,
                    &runtimes,
                    &handle,
                )
                .await
                {
                    continue;
                }

                if matches!(
                    ensure_live_relay_running_if_ready(
                        LiveRelayRunningContext {
                            runtime_id,
                            work_dir: &work_dir,
                            registry: &registry,
                            events: &events,
                        },
                        &handle,
                        &startup_probe,
                        recording_started,
                        "ZLM live_relay recording is active",
                    ),
                    LiveRelayRunningReadiness::Ready
                ) {
                    sleep(STARTUP_PROBE_POLL_INTERVAL).await;
                    continue;
                }

                if started_at.elapsed() >= STARTUP_PROBE_TIMEOUT {
                    match handle_live_relay_startup_timeout(
                        LiveRelayStartupTimeoutContext {
                            runtime_id,
                            work_dir: &work_dir,
                            settings: &settings,
                            http_client: &http_client,
                            registry: &registry,
                            runtimes: &runtimes,
                            events: &events,
                        },
                        &handle,
                        &startup_probe,
                        LiveRelayStartupTimeoutMode::RecordingStartup,
                    )
                    .await
                    {
                        LiveRelayStartupTimeoutOutcome::Retry => {
                            sleep(STARTUP_PROBE_POLL_INTERVAL).await;
                            continue;
                        }
                        LiveRelayStartupTimeoutOutcome::Fatal => return,
                    }
                }

                sleep(STARTUP_PROBE_POLL_INTERVAL).await;
                continue;
            }

            let stream_state = stream_status
                .as_ref()
                .map(|status| status.is_some())
                .map_err(|_| ());
            let stream_was_online = stream_online(&handle);
            let (next_offline_polls, offline_threshold_reached) =
                next_live_relay_offline_polls(offline_polls, stream_was_online, stream_state);
            match stream_status {
                Ok(Some(stream_status)) => {
                    offline_polls = next_offline_polls;
                    let mut recording_started = false;
                    let binding = stream_binding_from_handle(&handle)
                        .unwrap_or_else(|| stream_status.binding.clone());
                    let mut active_handle = handle.clone();
                    if let Some(recording) = live_relay_recording_from_handle(&handle)
                        .filter(should_start_live_relay_recording)
                    {
                        match start_live_relay_recording_from_monitor(
                            LiveRelayRecordingStartContext {
                                runtime_id,
                                work_dir: &work_dir,
                                settings: &settings,
                                http_client: &http_client,
                                registry: &registry,
                                runtimes: &runtimes,
                                events: &events,
                            },
                            &handle,
                            &active_handle,
                            &binding,
                            &recording,
                            LiveRelayRecordingStartMode::OnlineMonitor,
                        )
                        .await
                        {
                            LiveRelayRecordingStartOutcome::Updated {
                                handle,
                                recording_started: started,
                            } => {
                                recording_started = started;
                                active_handle = handle;
                            }
                            LiveRelayRecordingStartOutcome::Fatal => return,
                        }
                    }
                    let handle = registry.get(runtime_id).unwrap_or(active_handle);
                    if stop_live_relay_for_record_duration_if_reached(
                        runtime_id,
                        &work_dir,
                        &startup_probe,
                        &settings,
                        &http_client,
                        &registry,
                        &runtimes,
                        &handle,
                    )
                    .await
                    {
                        continue;
                    }
                    let _ = ensure_live_relay_running_if_ready(
                        LiveRelayRunningContext {
                            runtime_id,
                            work_dir: &work_dir,
                            registry: &registry,
                            events: &events,
                        },
                        &handle,
                        &startup_probe,
                        recording_started,
                        "ZLM live_relay stream is online",
                    );
                }
                Ok(None)
                    if !stream_online(&handle) && started_at.elapsed() >= STARTUP_PROBE_TIMEOUT =>
                {
                    match handle_live_relay_startup_timeout(
                        LiveRelayStartupTimeoutContext {
                            runtime_id,
                            work_dir: &work_dir,
                            settings: &settings,
                            http_client: &http_client,
                            registry: &registry,
                            runtimes: &runtimes,
                            events: &events,
                        },
                        &handle,
                        &startup_probe,
                        LiveRelayStartupTimeoutMode::StreamOnline,
                    )
                    .await
                    {
                        LiveRelayStartupTimeoutOutcome::Retry => {
                            sleep(STARTUP_PROBE_POLL_INTERVAL).await;
                            continue;
                        }
                        LiveRelayStartupTimeoutOutcome::Fatal => return,
                    }
                }
                Ok(None) if stream_was_online => {
                    offline_polls = next_offline_polls;
                    if !offline_threshold_reached {
                        sleep(STARTUP_PROBE_POLL_INTERVAL).await;
                        continue;
                    }
                    match handle_live_relay_offline_after_threshold(
                        LiveRelayOfflineContext {
                            runtime_id,
                            work_dir: &work_dir,
                            settings: &settings,
                            registry: &registry,
                            runtimes: &runtimes,
                            events: &events,
                        },
                        &handle,
                        &startup_probe,
                        stop_requested,
                    ) {
                        LiveRelayOfflineOutcome::Retry => {
                            sleep(STARTUP_PROBE_POLL_INTERVAL).await;
                            continue;
                        }
                        LiveRelayOfflineOutcome::Fatal => return,
                    }
                }
                Ok(None) | Err(_) => {
                    offline_polls = next_offline_polls;
                }
            }

            sleep(STARTUP_PROBE_POLL_INTERVAL).await;
        }
    });
}
