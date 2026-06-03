//! Runtime 收养恢复：在 agent 重连后重新挂接内存/持久化 runtime。
//!
//! 这里集中处理 control-plane 重新下发 adopt 请求后的恢复路径，包括活动 registry
//! runtime 重新标记、持久化进程收养、ZLM RTP server 和 ZLM proxy runtime 恢复或重启。

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    sync::{Arc, RwLock, atomic::AtomicBool},
};

use media_domain::{RuntimeHandle, RuntimeState};
use reqwest::Client;
use serde_json::json;
use tokio::task::JoinSet;
use uuid::Uuid;

use crate::{
    config::AgentSettings,
    runtime::{StartTaskRequest, StartupProbe, TaskRuntimeMode},
    runtime_events::{
        RuntimeEventSink, RuntimeNotification, RuntimeTaskEvent, runtime_session_epoch,
    },
    runtime_metadata::{
        RtpServerMetadata, attach_zlm_server_id, companion_process_identity_from_metadata,
        companion_recording_from_handle, live_relay_recording_from_handle,
        process_identity_from_handle, restart_request_from_handle, rtp_server_from_handle,
        runtime_lease_token, startup_probe_from_handle, stream_online,
        task_runtime_mode_from_handle,
    },
    runtime_monitors::{
        spawn_live_relay_monitor, spawn_rtp_receive_monitor, spawn_startup_probe_monitor,
    },
    runtime_persistence::{PersistedRuntimeState, persist_runtime_state, scan_persisted_runtimes},
    runtime_process::{ManagedRuntime, ProcessIdentity, RuntimeSlotLimiter, is_pid_running},
    runtime_process_monitors::{
        spawn_adopted_companion_process_monitor, spawn_adopted_runtime_monitor,
    },
    runtime_recording::should_start_live_relay_recording,
    runtime_registry::{AdoptFilter, LocalRuntimeRegistry},
};

const ADOPT_ZLM_PROBE_CONCURRENCY_LIMIT: usize = 8;

pub(crate) struct RuntimeAdoptionContext {
    pub(crate) filter: AdoptFilter,
    pub(crate) zlm_server_id: Option<String>,
    pub(crate) settings: AgentSettings,
    pub(crate) http_client: Client,
    pub(crate) registry: LocalRuntimeRegistry,
    pub(crate) runtimes: Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    pub(crate) slot_limiter: Arc<RuntimeSlotLimiter>,
    pub(crate) events: RuntimeEventSink,
}

#[derive(Debug)]
struct PendingZlmAdoption {
    persisted: PersistedRuntimeState,
    probe: ZlmProbe,
}

#[derive(Debug, Clone)]
enum ZlmProbe {
    RtpServer { rtp_server: RtpServerMetadata },
    ZlmProxy { startup_probe: StartupProbe },
}

#[derive(Debug)]
enum ZlmProbeDecision {
    RtpServer { local_port: Option<u16> },
    ZlmProxy { online: bool },
}

pub(crate) async fn adopt_orphan_runtimes<
    RestartTask,
    RestartFuture,
    StreamOnline,
    StreamFuture,
    RtpServerPort,
    RtpFuture,
>(
    context: RuntimeAdoptionContext,
    mut restart_task: RestartTask,
    zlm_stream_online: StreamOnline,
    rtp_server_port: RtpServerPort,
) -> Vec<RuntimeHandle>
where
    RestartTask: FnMut(StartTaskRequest) -> RestartFuture + Send,
    RestartFuture: Future<Output = Option<RuntimeHandle>> + Send,
    StreamOnline: Fn(StartupProbe) -> StreamFuture + Clone + Send + 'static,
    StreamFuture: Future<Output = bool> + Send + 'static,
    RtpServerPort: Fn(String) -> RtpFuture + Clone + Send + 'static,
    RtpFuture: Future<Output = Option<u16>> + Send + 'static,
{
    if context.filter.runtimes.is_empty() {
        return Vec::new();
    }

    let mut snapshots = Vec::new();
    for handle in context.registry.snapshots(&context.filter) {
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
    let mut pending_zlm_adoptions = Vec::new();

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
            let companion_processes = companion_recording_from_handle(&handle)
                .and_then(|companion| companion_process_identity_from_metadata(&companion))
                .filter(|companion_process| is_pid_running(companion_process.pid))
                .into_iter()
                .collect::<Vec<_>>();

            track_adopted_runtime(
                &context,
                handle.clone(),
                process_identity_from_handle(&handle),
                companion_processes.clone(),
            );
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
                pending_zlm_adoptions.push(PendingZlmAdoption {
                    persisted,
                    probe: ZlmProbe::RtpServer { rtp_server },
                });
                continue;
            }
            Some(TaskRuntimeMode::ZlmProxy) => {}
            _ => continue,
        }

        let Some(startup_probe) = startup_probe_from_handle(&persisted.handle) else {
            continue;
        };
        pending_zlm_adoptions.push(PendingZlmAdoption {
            persisted,
            probe: ZlmProbe::ZlmProxy { startup_probe },
        });
    }

    let probe_decisions =
        probe_pending_zlm_adoptions(&pending_zlm_adoptions, zlm_stream_online, rtp_server_port)
            .await;

    for (pending, decision) in pending_zlm_adoptions
        .into_iter()
        .zip(probe_decisions.into_iter())
    {
        let key = (
            pending.persisted.handle.task_id,
            pending.persisted.handle.attempt_no,
        );
        if seen.contains(&key) {
            continue;
        }

        match (pending.probe, decision) {
            (
                ZlmProbe::RtpServer { rtp_server },
                Some(ZlmProbeDecision::RtpServer {
                    local_port: Some(local_port),
                }),
            ) => {
                let handle = adopt_persisted_rtp_runtime(
                    &context,
                    pending.persisted,
                    rtp_server,
                    local_port,
                );
                snapshots.push(handle);
                seen.insert(key);
                continue;
            }
            (
                ZlmProbe::ZlmProxy { startup_probe },
                Some(ZlmProbeDecision::ZlmProxy { online: true }),
            ) => {
                let handle =
                    adopt_persisted_zlm_proxy_runtime(&context, pending.persisted, startup_probe);
                snapshots.push(handle);
                seen.insert(key);
                continue;
            }
            _ => {}
        }

        let Ok(request) = restart_request_from_handle(&pending.persisted.handle) else {
            continue;
        };
        let Some(handle) = restart_task(request).await else {
            continue;
        };
        snapshots.push(handle);
        seen.insert(key);
    }

    snapshots
}

async fn probe_pending_zlm_adoptions<StreamOnline, StreamFuture, RtpServerPort, RtpFuture>(
    candidates: &[PendingZlmAdoption],
    zlm_stream_online: StreamOnline,
    rtp_server_port: RtpServerPort,
) -> Vec<Option<ZlmProbeDecision>>
where
    StreamOnline: Fn(StartupProbe) -> StreamFuture + Clone + Send + 'static,
    StreamFuture: Future<Output = bool> + Send + 'static,
    RtpServerPort: Fn(String) -> RtpFuture + Clone + Send + 'static,
    RtpFuture: Future<Output = Option<u16>> + Send + 'static,
{
    let mut decisions = std::iter::repeat_with(|| None)
        .take(candidates.len())
        .collect::<Vec<_>>();
    let mut join_set = JoinSet::new();
    let mut next_index = 0usize;

    while next_index < candidates.len() || !join_set.is_empty() {
        while next_index < candidates.len() && join_set.len() < ADOPT_ZLM_PROBE_CONCURRENCY_LIMIT {
            let index = next_index;
            let probe = candidates[index].probe.clone();
            let zlm_stream_online = zlm_stream_online.clone();
            let rtp_server_port = rtp_server_port.clone();
            join_set.spawn(async move {
                let decision = match probe {
                    ZlmProbe::RtpServer { rtp_server } => ZlmProbeDecision::RtpServer {
                        local_port: rtp_server_port(rtp_server.stream_id).await,
                    },
                    ZlmProbe::ZlmProxy { startup_probe } => ZlmProbeDecision::ZlmProxy {
                        online: zlm_stream_online(startup_probe).await,
                    },
                };
                (index, decision)
            });
            next_index += 1;
        }

        let Some(result) = join_set.join_next().await else {
            break;
        };
        if let Ok((index, decision)) = result {
            decisions[index] = Some(decision);
        }
    }

    decisions
}

fn adopt_persisted_rtp_runtime(
    context: &RuntimeAdoptionContext,
    persisted: PersistedRuntimeState,
    rtp_server: RtpServerMetadata,
    local_port: u16,
) -> RuntimeHandle {
    let stream_id = rtp_server.stream_id.clone();
    let mut handle = mark_orphaned_handle(
        persisted.handle.clone(),
        context.filter.session_epoch,
        context.zlm_server_id.as_deref(),
    );
    handle.metadata["rtp_server"] = json!(RtpServerMetadata {
        local_port,
        ..rtp_server.clone()
    });

    track_adopted_runtime(context, handle.clone(), None, Vec::new());
    let _ = persist_runtime_state(&persisted.work_dir, &handle, &persisted.success_check);
    emit_adopted_event(
        &context.events,
        &handle,
        "reattached persisted stream_ingest rtp runtime",
        json!({
            "runtime_id": handle.runtime_id,
            "orphaned": true,
            "rtp_stream_id": stream_id.clone(),
            "local_port": local_port,
            "re_use_port": rtp_server.reuse_port,
            "ssrc": rtp_server.ssrc,
        }),
    );
    spawn_rtp_receive_monitor(
        handle.runtime_id,
        persisted.work_dir,
        stream_id,
        context.settings.clone(),
        context.http_client.clone(),
        context.registry.clone(),
        context.runtimes.clone(),
        context.events.clone(),
    );
    handle
}

fn adopt_persisted_zlm_proxy_runtime(
    context: &RuntimeAdoptionContext,
    persisted: PersistedRuntimeState,
    startup_probe: StartupProbe,
) -> RuntimeHandle {
    let vhost = startup_probe.vhost.clone();
    let app = startup_probe.app.clone();
    let stream = startup_probe.stream.clone();
    let mut handle = mark_orphaned_handle(
        persisted.handle.clone(),
        context.filter.session_epoch,
        context.zlm_server_id.as_deref(),
    );
    handle.metadata["stream_online"] = json!(true);
    handle.metadata["stream_binding"] = json!({
        "schema": startup_probe.schema.clone(),
        "vhost": startup_probe.vhost.clone(),
        "app": startup_probe.app.clone(),
        "stream": startup_probe.stream.clone(),
    });

    track_adopted_runtime(context, handle.clone(), None, Vec::new());
    let _ = persist_runtime_state(&persisted.work_dir, &handle, &persisted.success_check);
    emit_adopted_event(
        &context.events,
        &handle,
        "reattached persisted stream_ingest runtime",
        json!({
            "runtime_id": handle.runtime_id,
            "orphaned": true,
            "vhost": vhost,
            "app": app,
            "stream": stream,
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
    handle
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
    context: &RuntimeAdoptionContext,
    handle: RuntimeHandle,
    process: Option<ProcessIdentity>,
    companion_processes: Vec<ProcessIdentity>,
) {
    context.registry.track(handle.clone());
    let slot_permit = context.slot_limiter.attach_existing();
    {
        let mut runtimes = context.runtimes.write().expect("runtime map lock poisoned");
        runtimes.insert(
            handle.runtime_id,
            ManagedRuntime {
                process,
                companion_processes,
                _slot_permit: slot_permit,
                stop_requested: Arc::new(AtomicBool::new(false)),
                suppress_companion_events: Arc::new(AtomicBool::new(false)),
            },
        );
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use chrono::Utc;
    use media_domain::{RuntimeState, WorkerKind};
    use tokio::{sync::mpsc, time::Instant};

    use crate::{
        config::AgentSettings,
        runtime::{AdoptRuntimeFilter, SuccessCheck, runtime_session_epoch, task_runtime_mode},
    };

    fn test_context(
        work_root: &std::path::Path,
        registry: LocalRuntimeRegistry,
    ) -> RuntimeAdoptionContext {
        let (priority_tx, _priority_rx) = mpsc::unbounded_channel();
        let (log_tx, _log_rx) = mpsc::channel(8);
        let settings = AgentSettings {
            work_root: work_root.to_string_lossy().to_string(),
            zlm_api_base: String::new(),
            max_runtime_slots: 0,
            ..Default::default()
        };

        RuntimeAdoptionContext {
            filter: AdoptFilter::default(),
            zlm_server_id: Some("zlm-test".to_string()),
            settings,
            http_client: Client::new(),
            registry,
            runtimes: Arc::new(RwLock::new(HashMap::new())),
            slot_limiter: Arc::new(RuntimeSlotLimiter::new(0)),
            events: RuntimeEventSink::new(priority_tx, log_tx),
        }
    }

    fn filter_for(handles: &[RuntimeHandle], session_epoch: u64) -> AdoptFilter {
        AdoptFilter {
            session_epoch,
            runtimes: handles
                .iter()
                .map(|handle| AdoptRuntimeFilter {
                    task_id: handle.task_id,
                    attempt_no: handle.attempt_no,
                    lease_token: runtime_lease_token(handle).unwrap_or_default(),
                    worker_kind: handle.worker_kind,
                })
                .collect(),
        }
    }

    fn persist_test_runtime(work_root: &std::path::Path, handle: &RuntimeHandle) {
        let work_dir = work_root
            .join(handle.task_id.to_string())
            .join(format!("attempt-{}", handle.attempt_no));
        persist_runtime_state(&work_dir, handle, &SuccessCheck::ProcessExit)
            .expect("runtime should persist");
    }

    fn zlm_proxy_handle(stream: impl Into<String>) -> RuntimeHandle {
        let stream = stream.into();
        let task_id = Uuid::now_v7();
        let lease_token = format!("lease-{stream}");
        let resolved_spec = json!({
            "type": "stream_ingest",
            "name": stream,
            "input": {
                "kind": "rtsp",
                "source_mode": "live",
                "url": format!("rtsp://127.0.0.1/{stream}")
            },
            "stream": {
                "app": "live",
                "name": stream
            },
            "expose": {
                "enable_rtsp": true,
                "enable_rtmp": true
            },
            "record": {
                "enabled": false
            },
            "schedule": {
                "start_mode": "immediate"
            }
        });
        RuntimeHandle {
            runtime_id: Uuid::now_v7(),
            task_id,
            attempt_no: 1,
            worker_kind: WorkerKind::ZlmProxy,
            pid: None,
            started_at: Utc::now(),
            last_progress_at: None,
            state: RuntimeState::Running,
            command_line: None,
            outputs: Vec::new(),
            metadata: json!({
                "task_type": "stream_ingest",
                "execution_mode": "managed",
                "lease_token": lease_token,
                "resolved_spec": resolved_spec,
                "startup_probe": {
                    "schema": "rtsp",
                    "vhost": "__defaultVhost__",
                    "app": "live",
                    "stream": stream
                },
            }),
        }
    }

    fn rtp_runtime_handle(stream_id: impl Into<String>) -> RuntimeHandle {
        let stream_id = stream_id.into();
        let task_id = Uuid::now_v7();
        let lease_token = format!("lease-{stream_id}");
        let resolved_spec = json!({
            "type": "stream_ingest",
            "name": stream_id,
            "input": {
                "kind": "gb_rtp",
                "source_mode": "live",
                "port": 0,
                "tcp_mode": 0
            },
            "record": {
                "enabled": false
            },
            "schedule": {
                "start_mode": "immediate"
            }
        });
        RuntimeHandle {
            runtime_id: Uuid::now_v7(),
            task_id,
            attempt_no: 1,
            worker_kind: WorkerKind::ZlmRtpServer,
            pid: None,
            started_at: Utc::now(),
            last_progress_at: None,
            state: RuntimeState::Running,
            command_line: None,
            outputs: Vec::new(),
            metadata: json!({
                "task_type": "stream_ingest",
                "execution_mode": "managed",
                "lease_token": lease_token,
                "resolved_spec": resolved_spec,
                "rtp_server": RtpServerMetadata {
                    stream_id,
                    local_port: 10000,
                    requested_port: 0,
                    tcp_mode: 0,
                    reuse_port: Some(false),
                    ssrc: None,
                },
            }),
        }
    }

    fn record_max(max: &AtomicUsize, value: usize) {
        let mut current = max.load(Ordering::SeqCst);
        while value > current {
            match max.compare_exchange(current, value, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return,
                Err(next) => current = next,
            }
        }
    }

    #[tokio::test]
    async fn persisted_zlm_proxy_probes_are_bounded_and_parallel() {
        let temp_root =
            std::env::temp_dir().join(format!("streamserver-adopt-zlm-{}", Uuid::now_v7()));
        let handles = (0..10)
            .map(|index| zlm_proxy_handle(format!("stream-{index}")))
            .collect::<Vec<_>>();
        for handle in &handles {
            persist_test_runtime(&temp_root, handle);
        }

        let registry = LocalRuntimeRegistry::new();
        let mut context = test_context(&temp_root, registry.clone());
        context.filter = filter_for(&handles, 9);
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Instant::now();

        let adopted = adopt_orphan_runtimes(
            context,
            |_request| async { None },
            {
                let active = active.clone();
                let max_active = max_active.clone();
                let calls = calls.clone();
                move |_probe| {
                    let active = active.clone();
                    let max_active = max_active.clone();
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                        record_max(&max_active, current);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        active.fetch_sub(1, Ordering::SeqCst);
                        true
                    }
                }
            },
            |_stream_id| async { None },
        )
        .await;

        assert_eq!(adopted.len(), 10);
        assert_eq!(calls.load(Ordering::SeqCst), 10);
        assert!(max_active.load(Ordering::SeqCst) > 1);
        assert!(max_active.load(Ordering::SeqCst) <= ADOPT_ZLM_PROBE_CONCURRENCY_LIMIT);
        assert!(
            started.elapsed() < Duration::from_millis(400),
            "bounded parallel probing should be faster than serial probing"
        );
        assert_eq!(registry.count(), 10);

        let _ = fs::remove_dir_all(temp_root);
    }

    #[tokio::test]
    async fn failed_zlm_probe_does_not_pollute_registry() {
        let temp_root =
            std::env::temp_dir().join(format!("streamserver-adopt-zlm-failure-{}", Uuid::now_v7()));
        let online_a = zlm_proxy_handle("online-a");
        let offline = zlm_proxy_handle("offline");
        let online_b = zlm_proxy_handle("online-b");
        let handles = vec![online_a.clone(), offline.clone(), online_b.clone()];
        for handle in &handles {
            persist_test_runtime(&temp_root, handle);
        }

        let registry = LocalRuntimeRegistry::new();
        let mut context = test_context(&temp_root, registry.clone());
        context.filter = filter_for(&handles, 3);
        let restart_count = Arc::new(AtomicUsize::new(0));

        let adopted = adopt_orphan_runtimes(
            context,
            {
                let restart_count = restart_count.clone();
                move |_request| {
                    let restart_count = restart_count.clone();
                    async move {
                        restart_count.fetch_add(1, Ordering::SeqCst);
                        None
                    }
                }
            },
            |probe| async move { probe.stream != "offline" },
            |_stream_id| async { None },
        )
        .await;

        assert_eq!(adopted.len(), 2);
        assert_eq!(registry.count(), 2);
        assert!(
            registry
                .find_by_task_attempt(offline.task_id, offline.attempt_no)
                .is_none()
        );
        assert_eq!(restart_count.load(Ordering::SeqCst), 1);

        let _ = fs::remove_dir_all(temp_root);
    }

    #[tokio::test]
    async fn active_registry_runtime_is_returned_without_persisted_probe() {
        let temp_root = std::env::temp_dir().join(format!(
            "streamserver-adopt-active-first-{}",
            Uuid::now_v7()
        ));
        let handle = zlm_proxy_handle("already-active");
        persist_test_runtime(&temp_root, &handle);

        let registry = LocalRuntimeRegistry::new();
        registry.track(handle.clone());
        let mut context = test_context(&temp_root, registry.clone());
        context.filter = filter_for(std::slice::from_ref(&handle), 11);
        let probe_calls = Arc::new(AtomicUsize::new(0));

        let adopted = adopt_orphan_runtimes(
            context,
            |_request| async { None },
            {
                let probe_calls = probe_calls.clone();
                move |_probe| {
                    let probe_calls = probe_calls.clone();
                    async move {
                        probe_calls.fetch_add(1, Ordering::SeqCst);
                        true
                    }
                }
            },
            |_stream_id| async { None },
        )
        .await;

        assert_eq!(adopted.len(), 1);
        assert_eq!(runtime_session_epoch(&adopted[0]), 11);
        assert_eq!(probe_calls.load(Ordering::SeqCst), 0);
        assert_eq!(registry.count(), 1);

        let _ = fs::remove_dir_all(temp_root);
    }

    #[tokio::test]
    async fn rtp_adoption_refreshes_local_port_metadata() {
        let temp_root =
            std::env::temp_dir().join(format!("streamserver-adopt-rtp-{}", Uuid::now_v7()));
        let handle = rtp_runtime_handle("rtp-stream-1");
        persist_test_runtime(&temp_root, &handle);

        let registry = LocalRuntimeRegistry::new();
        let mut context = test_context(&temp_root, registry.clone());
        context.filter = filter_for(std::slice::from_ref(&handle), 5);

        let adopted = adopt_orphan_runtimes(
            context,
            |_request| async { None },
            |_probe| async { false },
            |_stream_id| async { Some(32000) },
        )
        .await;

        assert_eq!(adopted.len(), 1);
        assert_eq!(
            adopted[0].metadata["rtp_server"]["local_port"]
                .as_u64()
                .unwrap_or_default(),
            32000
        );
        assert_eq!(registry.count(), 1);
        let scanned = scan_persisted_runtimes(temp_root.to_string_lossy().as_ref());
        assert_eq!(scanned.len(), 1);
        assert_eq!(
            scanned[0].handle.metadata["rtp_server"]["local_port"]
                .as_u64()
                .unwrap_or_default(),
            32000
        );
        assert_eq!(
            task_runtime_mode(
                &serde_json::from_value(scanned[0].handle.metadata["resolved_spec"].clone())
                    .expect("resolved spec should decode")
            ),
            TaskRuntimeMode::ZlmRtpServer
        );

        let _ = fs::remove_dir_all(temp_root);
    }
}
