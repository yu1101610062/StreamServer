//! Runtime 收养恢复：在 agent 重连后重新挂接内存/持久化 runtime。
//!
//! 这里集中处理 control-plane 重新下发 adopt 请求后的恢复路径，包括活动 registry
//! runtime 重新标记、持久化进程收养、ZLM RTP server 和 ZLM proxy runtime 恢复或重启。

use std::{collections::HashSet, future::Future};

use media_domain::{RuntimeHandle, RuntimeState};
use serde_json::json;
use tokio::task::JoinSet;

use crate::{
    config::AgentSettings,
    runtime::{StartTaskRequest, StartupProbe, TaskRuntimeMode},
    runtime_events::{RuntimeNotification, RuntimeTaskEvent, runtime_session_epoch},
    runtime_metadata::{
        RtpServerMetadata, attach_zlm_server_id, companion_process_identity_from_metadata,
        companion_recording_from_handle, live_relay_recording_from_handle,
        process_identity_from_handle, restart_request_from_handle, rtp_server_from_handle,
        runtime_lease_token, startup_probe_from_handle, stream_online,
        task_runtime_mode_from_handle,
    },
    runtime_persistence::{PersistedRuntimeState, scan_persisted_runtimes},
    runtime_process::{ProcessIdentity, is_process_running_for_command_line},
    runtime_recording::should_start_live_relay_recording,
    runtime_registry::AdoptFilter,
};

const ADOPT_ZLM_PROBE_CONCURRENCY_LIMIT: usize = 8;

pub(crate) struct RuntimeAdoptionWorkerContext {
    pub(crate) filter: AdoptFilter,
    pub(crate) zlm_server_id: Option<String>,
    pub(crate) settings: AgentSettings,
}

pub(crate) enum RuntimeAdoptionOutcome<Restart> {
    Adopted(RuntimeAdoptionCommit),
    Restart(Restart),
}

pub(crate) struct RuntimeAdoptionCommit {
    pub(crate) handle: RuntimeHandle,
    pub(crate) work_dir: std::path::PathBuf,
    pub(crate) success_check: crate::runtime::SuccessCheck,
    pub(crate) backend: RuntimeAdoptionBackend,
    pub(crate) notifications: Vec<RuntimeNotification>,
    pub(crate) monitors: Vec<RuntimeAdoptionMonitor>,
}

pub(crate) struct RuntimeAdoptionBackend {
    pub(crate) process: Option<ProcessIdentity>,
    pub(crate) companion_processes: Vec<ProcessIdentity>,
}

pub(crate) enum RuntimeAdoptionMonitor {
    StartupProbe {
        startup_probe: StartupProbe,
    },
    AdoptedRuntime,
    AdoptedCompanion {
        process: ProcessIdentity,
        companion: crate::runtime_metadata::CompanionProcessMetadata,
    },
    LiveRelay {
        startup_probe: StartupProbe,
    },
    RtpReceive {
        stream_id: String,
    },
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

pub(crate) async fn prepare_adopt_orphan_runtimes_for_manager<
    RestartTask,
    RestartFuture,
    StreamOnline,
    StreamFuture,
    RtpServerPort,
    RtpFuture,
    Restart,
>(
    context: RuntimeAdoptionWorkerContext,
    mut restart_task: RestartTask,
    zlm_stream_online: StreamOnline,
    rtp_server_port: RtpServerPort,
) -> Vec<RuntimeAdoptionOutcome<Restart>>
where
    RestartTask: FnMut(StartTaskRequest) -> RestartFuture + Send,
    RestartFuture: Future<Output = Option<Restart>> + Send,
    StreamOnline: Fn(StartupProbe) -> StreamFuture + Clone + Send + 'static,
    StreamFuture: Future<Output = bool> + Send + 'static,
    RtpServerPort: Fn(String) -> RtpFuture + Clone + Send + 'static,
    RtpFuture: Future<Output = Option<u16>> + Send + 'static,
{
    if context.filter.runtimes.is_empty() {
        return Vec::new();
    }

    let mut outcomes = Vec::new();
    let mut seen = HashSet::new();
    let mut pending_zlm_adoptions = Vec::new();

    for persisted in scan_persisted_runtimes(&context.settings.work_root) {
        let key = (persisted.handle.task_id, persisted.handle.attempt_no);
        if seen.contains(&key) || !context.filter.matches(&persisted.handle) {
            continue;
        }

        if let Some(process) = process_identity_from_handle(&persisted.handle) {
            if !is_process_running_for_command_line(
                &process,
                persisted.handle.command_line.as_deref(),
            ) {
                continue;
            }
            let pid = process.pid;
            let handle = mark_orphaned_handle(
                persisted.handle.clone(),
                context.filter.session_epoch,
                context.zlm_server_id.as_deref(),
            );
            let companion_processes = companion_recording_from_handle(&handle)
                .and_then(|companion| {
                    companion_process_identity_from_metadata(&companion).filter(
                        |companion_process| {
                            is_process_running_for_command_line(
                                companion_process,
                                companion.command_line.as_deref(),
                            )
                        },
                    )
                })
                .into_iter()
                .collect::<Vec<_>>();

            let mut monitors = Vec::new();
            let needs_startup_probe = !stream_online(&handle)
                || live_relay_recording_from_handle(&handle)
                    .is_some_and(|recording| should_start_live_relay_recording(&recording));
            if let Some(startup_probe) =
                startup_probe_from_handle(&handle).filter(|_| needs_startup_probe)
            {
                monitors.push(RuntimeAdoptionMonitor::StartupProbe { startup_probe });
            }
            monitors.push(RuntimeAdoptionMonitor::AdoptedRuntime);
            if let Some(companion) =
                companion_recording_from_handle(&handle).filter(|companion| companion.pid.is_some())
            {
                if let Some(companion_process) =
                    companion_process_identity_from_metadata(&companion).filter(|process| {
                        is_process_running_for_command_line(
                            process,
                            companion.command_line.as_deref(),
                        )
                    })
                {
                    monitors.push(RuntimeAdoptionMonitor::AdoptedCompanion {
                        process: companion_process,
                        companion,
                    });
                }
            }

            outcomes.push(RuntimeAdoptionOutcome::Adopted(RuntimeAdoptionCommit {
                handle: handle.clone(),
                work_dir: persisted.work_dir,
                success_check: persisted.success_check,
                backend: RuntimeAdoptionBackend {
                    process: Some(process),
                    companion_processes,
                },
                notifications: vec![adopted_event_notification(
                    &handle,
                    "reattached persisted child process",
                    json!({
                        "runtime_id": handle.runtime_id,
                        "orphaned": true,
                        "pid": pid,
                    }),
                )],
                monitors,
            }));
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
                let handle = build_adopted_rtp_handle(
                    &context,
                    &pending.persisted.handle,
                    &rtp_server,
                    local_port,
                );
                outcomes.push(RuntimeAdoptionOutcome::Adopted(RuntimeAdoptionCommit {
                    handle: handle.clone(),
                    work_dir: pending.persisted.work_dir,
                    success_check: pending.persisted.success_check,
                    backend: RuntimeAdoptionBackend {
                        process: None,
                        companion_processes: Vec::new(),
                    },
                    notifications: vec![adopted_event_notification(
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
                    )],
                    monitors: vec![RuntimeAdoptionMonitor::RtpReceive {
                        stream_id: handle
                            .metadata
                            .get("rtp_stream_id")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    }],
                }));
                seen.insert(key);
                continue;
            }
            (
                ZlmProbe::ZlmProxy { startup_probe },
                Some(ZlmProbeDecision::ZlmProxy { online: true }),
            ) => {
                let handle = build_adopted_zlm_proxy_handle(
                    &context,
                    &pending.persisted.handle,
                    &startup_probe,
                );
                outcomes.push(RuntimeAdoptionOutcome::Adopted(RuntimeAdoptionCommit {
                    handle: handle.clone(),
                    work_dir: pending.persisted.work_dir,
                    success_check: pending.persisted.success_check,
                    backend: RuntimeAdoptionBackend {
                        process: None,
                        companion_processes: Vec::new(),
                    },
                    notifications: vec![adopted_event_notification(
                        &handle,
                        "reattached persisted stream_ingest runtime",
                        json!({
                            "runtime_id": handle.runtime_id,
                            "orphaned": true,
                            "vhost": startup_probe.vhost,
                            "app": startup_probe.app,
                            "stream": startup_probe.stream,
                        }),
                    )],
                    monitors: vec![RuntimeAdoptionMonitor::LiveRelay { startup_probe }],
                }));
                seen.insert(key);
                continue;
            }
            _ => {}
        }

        let Ok(request) = restart_request_from_handle(&pending.persisted.handle) else {
            continue;
        };
        let Some(restart) = restart_task(request).await else {
            continue;
        };
        outcomes.push(RuntimeAdoptionOutcome::Restart(restart));
        seen.insert(key);
    }

    outcomes
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

fn build_adopted_rtp_handle(
    context: &RuntimeAdoptionWorkerContext,
    persisted_handle: &RuntimeHandle,
    rtp_server: &RtpServerMetadata,
    local_port: u16,
) -> RuntimeHandle {
    let mut handle = mark_orphaned_handle(
        persisted_handle.clone(),
        context.filter.session_epoch,
        context.zlm_server_id.as_deref(),
    );
    handle.metadata["rtp_server"] = json!(RtpServerMetadata {
        local_port,
        ..rtp_server.clone()
    });
    handle
}

fn build_adopted_zlm_proxy_handle(
    context: &RuntimeAdoptionWorkerContext,
    persisted_handle: &RuntimeHandle,
    startup_probe: &StartupProbe,
) -> RuntimeHandle {
    let mut handle = mark_orphaned_handle(
        persisted_handle.clone(),
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

pub(crate) fn adopted_event_notification(
    handle: &RuntimeHandle,
    message: &str,
    payload: serde_json::Value,
) -> RuntimeNotification {
    RuntimeNotification::TaskEvent(RuntimeTaskEvent {
        task_id: handle.task_id,
        attempt_no: handle.attempt_no,
        lease_token: runtime_lease_token(handle).unwrap_or_default(),
        session_epoch: runtime_session_epoch(handle),
        event_type: "adopted".to_string(),
        event_level: "info".to_string(),
        message: message.to_string(),
        payload,
    })
}
