//! Runtime 收养恢复：在 agent 重连后重新挂接内存/持久化 runtime。
//!
//! 这里集中处理 control-plane 重新下发 adopt 请求后的恢复路径，包括活动 registry
//! runtime 重新标记、持久化进程收养、ZLM RTP server 和 ZLM proxy runtime 恢复或重启。

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, RwLock, atomic::AtomicBool},
};

use media_domain::{RuntimeHandle, RuntimeState};
use reqwest::Client;
use serde_json::json;
use uuid::Uuid;

use crate::{
    config::AgentSettings,
    runtime::{StartTaskRequest, StartupProbe, TaskRuntimeMode},
    runtime_events::{
        RuntimeEventSink, RuntimeNotification, RuntimeTaskEvent, runtime_session_epoch,
    },
    runtime_metadata::{
        RtpServerMetadata, attach_zlm_server_id, companion_recording_from_handle,
        live_relay_recording_from_handle, restart_request_from_handle, rtp_server_from_handle,
        runtime_lease_token, startup_probe_from_handle, stream_online,
        task_runtime_mode_from_handle,
    },
    runtime_monitors::{
        spawn_live_relay_monitor, spawn_rtp_receive_monitor, spawn_startup_probe_monitor,
    },
    runtime_persistence::{persist_runtime_state, scan_persisted_runtimes},
    runtime_process::{ManagedRuntime, RuntimeSlotLimiter, is_pid_running},
    runtime_process_monitors::{
        spawn_adopted_companion_process_monitor, spawn_adopted_runtime_monitor,
    },
    runtime_recording::should_start_live_relay_recording,
    runtime_registry::{AdoptFilter, LocalRuntimeRegistry},
};

pub(crate) struct RuntimeAdoptionContext<'a> {
    pub(crate) filter: &'a AdoptFilter,
    pub(crate) zlm_server_id: Option<String>,
    pub(crate) settings: AgentSettings,
    pub(crate) http_client: Client,
    pub(crate) registry: LocalRuntimeRegistry,
    pub(crate) runtimes: Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    pub(crate) slot_limiter: Arc<RuntimeSlotLimiter>,
    pub(crate) events: RuntimeEventSink,
}

pub(crate) fn adopt_orphan_runtimes(
    context: RuntimeAdoptionContext<'_>,
    mut restart_task: impl FnMut(&StartTaskRequest) -> Option<RuntimeHandle>,
    zlm_stream_online: impl Fn(&StartupProbe) -> bool,
    rtp_server_port: impl Fn(&str) -> Option<u16>,
) -> Vec<RuntimeHandle> {
    if context.filter.runtimes.is_empty() {
        return Vec::new();
    }

    let mut snapshots = Vec::new();
    for handle in context.registry.snapshots(context.filter) {
        let updated = context
            .registry
            .update(handle.runtime_id, |runtime| {
                runtime.metadata["session_epoch"] = json!(context.filter.session_epoch);
                attach_zlm_server_id(&mut runtime.metadata, context.zlm_server_id.as_deref());
            })
            .unwrap_or_else(|| {
                let mut handle = handle.clone();
                handle.metadata["session_epoch"] = json!(context.filter.session_epoch);
                attach_zlm_server_id(&mut handle.metadata, context.zlm_server_id.as_deref());
                handle
            });
        emit_adopted_event(
            &context.events,
            &updated,
            "reattached active runtime after control-plane reconnect",
            json!({
                "runtime_id": updated.runtime_id,
                "orphaned": false,
            }),
        );
        snapshots.push(updated);
    }
    let mut seen = snapshots
        .iter()
        .map(|handle| (handle.task_id, handle.attempt_no))
        .collect::<HashSet<_>>();

    for persisted in scan_persisted_runtimes(&context.settings.work_root) {
        let key = (persisted.handle.task_id, persisted.handle.attempt_no);
        if seen.contains(&key) || !context.filter.matches(&persisted.handle) {
            continue;
        }

        if let Some(pid) = persisted.handle.pid {
            if !is_pid_running(pid) {
                continue;
            }

            let handle = mark_orphaned_handle(
                persisted.handle.clone(),
                context.filter.session_epoch,
                context.zlm_server_id.as_deref(),
            );
            let companion_pids = companion_recording_from_handle(&handle)
                .and_then(|companion| companion.pid)
                .filter(|companion_pid| is_pid_running(*companion_pid))
                .into_iter()
                .collect::<Vec<_>>();

            track_adopted_runtime(&context, handle.clone(), Some(pid), companion_pids.clone());
            let _ = persist_runtime_state(&persisted.work_dir, &handle, &persisted.success_check);
            emit_adopted_event(
                &context.events,
                &handle,
                "reattached persisted child process",
                json!({
                    "runtime_id": handle.runtime_id,
                    "orphaned": true,
                    "pid": pid,
                }),
            );
            let needs_startup_probe = !stream_online(&handle)
                || live_relay_recording_from_handle(&handle)
                    .is_some_and(|recording| should_start_live_relay_recording(&recording));
            if let Some(startup_probe) =
                startup_probe_from_handle(&handle).filter(|_| needs_startup_probe)
            {
                spawn_startup_probe_monitor(
                    handle.runtime_id,
                    persisted.work_dir.clone(),
                    persisted.success_check.clone(),
                    startup_probe,
                    context.settings.clone(),
                    context.http_client.clone(),
                    context.registry.clone(),
                    context.runtimes.clone(),
                    context.events.clone(),
                );
            }
            let adopted_work_dir = persisted.work_dir.clone();
            let adopted_success_check = persisted.success_check.clone();
            spawn_adopted_runtime_monitor(
                handle.clone(),
                persisted.work_dir,
                persisted.success_check,
                context.registry.clone(),
                context.runtimes.clone(),
                context.events.clone(),
            );
            if let Some(companion) =
                companion_recording_from_handle(&handle).filter(|companion| companion.pid.is_some())
            {
                if let Some(companion_pid) = companion.pid.filter(|value| is_pid_running(*value)) {
                    spawn_adopted_companion_process_monitor(
                        handle.runtime_id,
                        companion_pid,
                        companion,
                        adopted_work_dir,
                        adopted_success_check,
                        context.registry.clone(),
                        context.runtimes.clone(),
                        context.events.clone(),
                    );
                }
            }
            snapshots.push(handle);
            seen.insert(key);
            continue;
        }

        match task_runtime_mode_from_handle(&persisted.handle) {
            Some(TaskRuntimeMode::ZlmRtpServer) => {
                let Some(rtp_server) = rtp_server_from_handle(&persisted.handle) else {
                    continue;
                };

                if let Some(local_port) = rtp_server_port(&rtp_server.stream_id) {
                    let mut handle = mark_orphaned_handle(
                        persisted.handle.clone(),
                        context.filter.session_epoch,
                        context.zlm_server_id.as_deref(),
                    );
                    handle.metadata["rtp_server"] = json!(RtpServerMetadata {
                        local_port,
                        ..rtp_server.clone()
                    });

                    track_adopted_runtime(&context, handle.clone(), None, Vec::new());
                    let _ = persist_runtime_state(
                        &persisted.work_dir,
                        &handle,
                        &persisted.success_check,
                    );
                    emit_adopted_event(
                        &context.events,
                        &handle,
                        "reattached persisted stream_ingest rtp runtime",
                        json!({
                            "runtime_id": handle.runtime_id,
                            "orphaned": true,
                            "rtp_stream_id": rtp_server.stream_id,
                            "local_port": local_port,
                            "re_use_port": rtp_server.reuse_port,
                            "ssrc": rtp_server.ssrc,
                        }),
                    );
                    spawn_rtp_receive_monitor(
                        handle.runtime_id,
                        persisted.work_dir,
                        rtp_server.stream_id,
                        context.settings.clone(),
                        context.http_client.clone(),
                        context.registry.clone(),
                        context.runtimes.clone(),
                        context.events.clone(),
                    );
                    snapshots.push(handle);
                    seen.insert(key);
                    continue;
                }

                let Ok(request) = restart_request_from_handle(&persisted.handle) else {
                    continue;
                };
                let Some(handle) = restart_task(&request) else {
                    continue;
                };
                snapshots.push(handle);
                seen.insert(key);
                continue;
            }
            Some(TaskRuntimeMode::ZlmProxy) => {}
            _ => continue,
        }

        let Some(startup_probe) = startup_probe_from_handle(&persisted.handle) else {
            continue;
        };

        if zlm_stream_online(&startup_probe) {
            let mut handle = mark_orphaned_handle(
                persisted.handle.clone(),
                context.filter.session_epoch,
                context.zlm_server_id.as_deref(),
            );
            handle.metadata["stream_online"] = json!(true);
            handle.metadata["stream_binding"] = json!({
                "schema": startup_probe.schema,
                "vhost": startup_probe.vhost,
                "app": startup_probe.app,
                "stream": startup_probe.stream,
            });

            track_adopted_runtime(&context, handle.clone(), None, Vec::new());
            let _ = persist_runtime_state(&persisted.work_dir, &handle, &persisted.success_check);
            emit_adopted_event(
                &context.events,
                &handle,
                "reattached persisted stream_ingest runtime",
                json!({
                    "runtime_id": handle.runtime_id,
                    "orphaned": true,
                    "vhost": startup_probe.vhost,
                    "app": startup_probe.app,
                    "stream": startup_probe.stream,
                }),
            );
            spawn_live_relay_monitor(
                handle.runtime_id,
                persisted.work_dir,
                startup_probe,
                context.settings.clone(),
                context.http_client.clone(),
                context.registry.clone(),
                context.runtimes.clone(),
                context.events.clone(),
            );
            snapshots.push(handle);
            seen.insert(key);
            continue;
        }

        let Ok(request) = restart_request_from_handle(&persisted.handle) else {
            continue;
        };
        let Some(handle) = restart_task(&request) else {
            continue;
        };
        snapshots.push(handle);
        seen.insert(key);
    }

    snapshots
}

fn mark_orphaned_handle(
    mut handle: RuntimeHandle,
    session_epoch: u64,
    zlm_server_id: Option<&str>,
) -> RuntimeHandle {
    handle.state = RuntimeState::Orphaned;
    handle.metadata["orphaned"] = json!(true);
    handle.metadata["session_epoch"] = json!(session_epoch);
    attach_zlm_server_id(&mut handle.metadata, zlm_server_id);
    handle
}

fn track_adopted_runtime(
    context: &RuntimeAdoptionContext<'_>,
    handle: RuntimeHandle,
    pid: Option<i32>,
    companion_pids: Vec<i32>,
) {
    context.registry.track(handle.clone());
    let slot_permit = context.slot_limiter.attach_existing();
    context
        .runtimes
        .write()
        .expect("runtime map lock poisoned")
        .insert(
            handle.runtime_id,
            ManagedRuntime {
                pid,
                companion_pids,
                _slot_permit: slot_permit,
                stop_requested: Arc::new(AtomicBool::new(false)),
                suppress_companion_events: Arc::new(AtomicBool::new(false)),
            },
        );
}

fn emit_adopted_event(
    events: &RuntimeEventSink,
    handle: &RuntimeHandle,
    message: &str,
    payload: serde_json::Value,
) {
    let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
        task_id: handle.task_id,
        attempt_no: handle.attempt_no,
        lease_token: runtime_lease_token(handle).unwrap_or_default(),
        session_epoch: runtime_session_epoch(handle),
        event_type: "adopted".to_string(),
        event_level: "info".to_string(),
        message: message.to_string(),
        payload,
    }));
}
