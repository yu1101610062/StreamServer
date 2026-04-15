use std::{
    collections::HashMap,
    ffi::CStr,
    fs,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    ptr,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::Context;
use media_domain::{AgentRegistration, NetworkMode, RuntimeHandle, TaskType, WorkerKind};
use media_rpc::control_plane::{
    AdoptOrphans, AgentEnvelope, CapabilitySnapshot as RpcCapabilitySnapshot, CoreEnvelope,
    GpuDevice as RpcGpuDevice, GpuRuntime as RpcGpuRuntime, Heartbeat as RpcHeartbeat,
    ProbeCapabilities, Register as RpcRegister, StartTask, StopTask, TaskEvent, TaskSnapshot,
    control_plane_client::ControlPlaneClient,
};
use serde_json::{Value, json};
use tokio::{
    sync::{Mutex, Semaphore, mpsc},
    time::{MissedTickBehavior, interval, sleep},
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    capability::{CapabilityProbe, binary_available, probe_gpu_runtime},
    config::Settings,
    heartbeat::HeartbeatSampler,
    runtime::{
        AdoptFilter, AdoptRuntimeFilter, LocalExecutor, LocalRuntimeRegistry,
        ManagedProcessExecutor, RuntimeEventSink, RuntimeNotification, RuntimeTaskEvent,
        RuntimeTaskLogBatch, RuntimeTaskProgress, StartTaskRequest, StopTaskRequest,
        TerminalRuntimeReplay, cleanup_persisted_runtime_state, collect_terminal_runtime_replays,
        is_terminal_runtime_event, rejected_runtime_handle, runtime_session_epoch,
    },
};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const CONTROL_BACKOFF: [u64; 5] = [1, 2, 5, 10, 30];
const CONTROL_BUFFER: usize = 32;
const LOG_NOTIFICATION_BUFFER: usize = 128;
const START_TASK_CONCURRENCY_LIMIT: usize = 4;

#[derive(Clone)]
pub struct AgentController {
    settings: Arc<Settings>,
    node_id: Uuid,
    capability_probe: CapabilityProbe,
    runtime_registry: LocalRuntimeRegistry,
    executor: Arc<dyn LocalExecutor>,
    runtime_priority_events: Arc<Mutex<mpsc::UnboundedReceiver<RuntimeNotification>>>,
    runtime_log_batches: Arc<Mutex<mpsc::Receiver<RuntimeTaskLogBatch>>>,
    start_task_permits: Arc<Semaphore>,
    session_epoch: Arc<AtomicU64>,
}

impl AgentController {
    pub fn new(settings: Settings) -> anyhow::Result<Self> {
        let node_id = if settings.agent.node_id.trim().is_empty() {
            Uuid::now_v7()
        } else {
            Uuid::parse_str(settings.agent.node_id.trim())?
        };
        let runtime_registry = LocalRuntimeRegistry::new();
        let (runtime_priority_tx, runtime_priority_rx) = mpsc::unbounded_channel();
        let (runtime_log_tx, runtime_log_rx) = mpsc::channel(LOG_NOTIFICATION_BUFFER);
        let executor = Arc::new(ManagedProcessExecutor::new(
            settings.agent.clone(),
            runtime_registry.clone(),
            RuntimeEventSink::new(runtime_priority_tx, runtime_log_tx),
        ));

        Ok(Self {
            settings: Arc::new(settings),
            node_id,
            capability_probe: CapabilityProbe::new()?,
            runtime_registry,
            executor,
            runtime_priority_events: Arc::new(Mutex::new(runtime_priority_rx)),
            runtime_log_batches: Arc::new(Mutex::new(runtime_log_rx)),
            start_task_permits: Arc::new(Semaphore::new(START_TASK_CONCURRENCY_LIMIT)),
            session_epoch: Arc::new(AtomicU64::new(0)),
        })
    }

    pub async fn run(self) {
        let mut backoff_idx = 0usize;

        loop {
            match self.connect_once().await {
                Ok(()) => {
                    warn!("control-plane stream closed, reconnecting");
                    backoff_idx = 0;
                }
                Err(error) => {
                    warn!(error = %error, "control-plane connection failed");
                    backoff_idx = (backoff_idx + 1).min(CONTROL_BACKOFF.len() - 1);
                }
            }

            sleep(Duration::from_secs(CONTROL_BACKOFF[backoff_idx])).await;
        }
    }

    async fn connect_once(&self) -> anyhow::Result<()> {
        let session_epoch = self.session_epoch.fetch_add(1, Ordering::SeqCst) + 1;
        let result = self.connect_once_active(session_epoch).await;
        self.invalidate_session_epoch(session_epoch);
        result
    }

    async fn connect_once_active(&self, session_epoch: u64) -> anyhow::Result<()> {
        let endpoint = build_endpoint(&self.settings.agent)?;
        let channel = endpoint.connect().await?;
        let mut client = ControlPlaneClient::new(channel);

        let (sender, receiver) = mpsc::channel(CONTROL_BUFFER);
        let registration = self.build_registration().await?;
        send_agent_message(
            &sender,
            AgentEnvelope {
                payload: Some(media_rpc::control_plane::agent_envelope::Payload::Register(
                    registration_to_rpc(&registration),
                )),
            },
        )
        .await?;

        let response = client.stream_connect(ReceiverStream::new(receiver)).await?;
        let mut inbound = response.into_inner();

        let snapshot = self.capability_probe.snapshot(&self.settings.agent).await;
        self.executor.set_zlm_rtmp_enhanced_enabled(
            self.capability_probe
                .zlm_rtmp_enhanced_enabled(&self.settings.agent)
                .await,
        );
        send_capability_snapshot(&sender, &snapshot).await?;
        self.replay_terminal_runtimes(&sender).await?;

        let mut heartbeat_sampler = HeartbeatSampler::new(
            self.settings.agent.work_root.clone(),
            self.settings.agent.max_runtime_slots,
        );
        let mut heartbeat = interval(HEARTBEAT_INTERVAL);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut dropped_log_lines = HashMap::new();

        info!(
            node_id = %registration.node_id,
            node_name = %registration.node_name,
            core_endpoint = %self.settings.agent.core_endpoint,
            "control-plane connected"
        );

        loop {
            tokio::select! {
                biased;
                message = inbound.message() => {
                    match message? {
                        Some(message) => self.handle_core_envelope(&sender, message, session_epoch).await?,
                        None => return Ok(()),
                    }
                }
                runtime_notification = recv_runtime_notification(self.runtime_priority_events.clone()) => {
                    if let Some(runtime_notification) = runtime_notification {
                        self.forward_runtime_notification(&sender, runtime_notification, session_epoch).await?;
                    }
                }
                _ = heartbeat.tick() => {
                    self.send_heartbeat(&sender, &mut heartbeat_sampler).await?;
                }
                log_batch = recv_runtime_log_batch(self.runtime_log_batches.clone()) => {
                    if let Some(log_batch) = log_batch {
                        if self.current_session_epoch() == session_epoch
                            && log_batch.session_epoch == session_epoch
                        {
                            try_send_runtime_log_batch(&sender, log_batch, &mut dropped_log_lines)?;
                        }
                    }
                }
            }
        }
    }

    fn current_session_epoch(&self) -> u64 {
        self.session_epoch.load(Ordering::SeqCst)
    }

    fn invalidate_session_epoch(&self, session_epoch: u64) {
        let _ = self.session_epoch.compare_exchange(
            session_epoch,
            session_epoch.saturating_add(1),
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }

    async fn send_heartbeat(
        &self,
        sender: &mpsc::Sender<AgentEnvelope>,
        sampler: &mut HeartbeatSampler,
    ) -> anyhow::Result<()> {
        let zlm_alive = self.capability_probe.zlm_alive(&self.settings.agent).await;
        let ffmpeg_alive = binary_available(&self.settings.agent.ffmpeg_bin);
        let runtime_counts = self.runtime_registry.state_counts();
        let snapshot = sampler.sample(
            runtime_counts.running,
            runtime_counts.starting,
            runtime_counts.stopping,
            runtime_counts.orphaned,
            zlm_alive,
            ffmpeg_alive,
            probe_gpu_runtime(&self.settings.agent),
        );

        send_agent_message(
            sender,
            AgentEnvelope {
                payload: Some(
                    media_rpc::control_plane::agent_envelope::Payload::Heartbeat(RpcHeartbeat {
                        node_time_ms: snapshot.node_time.timestamp_millis(),
                        cpu_percent: snapshot.cpu_percent,
                        mem_percent: snapshot.mem_percent,
                        disk_percent: snapshot.disk_percent,
                        running_tasks: snapshot.running_tasks,
                        starting_tasks: snapshot.starting_tasks,
                        stopping_tasks: snapshot.stopping_tasks,
                        orphaned_tasks: snapshot.orphaned_tasks,
                        slot_usage: snapshot.slot_usage,
                        zlm_alive: snapshot.zlm_alive,
                        ffmpeg_alive: snapshot.ffmpeg_alive,
                        gpu_runtime: snapshot
                            .gpu_runtime
                            .iter()
                            .map(|runtime| RpcGpuRuntime {
                                index: runtime.index,
                                gpu_util_percent: runtime.gpu_util_percent,
                                memory_used_mb: runtime.memory_used_mb,
                                memory_total_mb: runtime.memory_total_mb,
                                encoder_util_percent: runtime.encoder_util_percent,
                                decoder_util_percent: runtime.decoder_util_percent,
                            })
                            .collect(),
                    }),
                ),
            },
        )
        .await?;

        debug!(
            running_tasks = snapshot.running_tasks,
            slot_usage = snapshot.slot_usage,
            zlm_alive = snapshot.zlm_alive,
            ffmpeg_alive = snapshot.ffmpeg_alive,
            "heartbeat sent"
        );

        Ok(())
    }

    async fn handle_core_envelope(
        &self,
        sender: &mpsc::Sender<AgentEnvelope>,
        envelope: CoreEnvelope,
        session_epoch: u64,
    ) -> anyhow::Result<()> {
        let Some(payload) = envelope.payload else {
            return Ok(());
        };

        match payload {
            media_rpc::control_plane::core_envelope::Payload::ProbeCapabilities(
                ProbeCapabilities {},
            ) => {
                let snapshot = self.capability_probe.snapshot(&self.settings.agent).await;
                self.executor.set_zlm_rtmp_enhanced_enabled(
                    self.capability_probe
                        .zlm_rtmp_enhanced_enabled(&self.settings.agent)
                        .await,
                );
                send_capability_snapshot(sender, &snapshot).await?;
            }
            media_rpc::control_plane::core_envelope::Payload::AdoptOrphans(command) => {
                self.handle_adopt_orphans(sender, command, session_epoch)
                    .await?;
            }
            media_rpc::control_plane::core_envelope::Payload::StartTask(command) => {
                self.handle_start_task(sender, command, session_epoch)
                    .await?;
            }
            media_rpc::control_plane::core_envelope::Payload::StopTask(command) => {
                self.handle_stop_task(sender, command).await?;
            }
        }

        Ok(())
    }

    async fn replay_terminal_runtimes(
        &self,
        sender: &mpsc::Sender<AgentEnvelope>,
    ) -> anyhow::Result<()> {
        for replay in
            collect_terminal_runtime_replays(&self.settings.agent.work_root, &self.runtime_registry)
        {
            self.forward_terminal_runtime_replay(sender, replay).await?;
        }
        Ok(())
    }

    async fn forward_runtime_notification(
        &self,
        sender: &mpsc::Sender<AgentEnvelope>,
        notification: RuntimeNotification,
        session_epoch: u64,
    ) -> anyhow::Result<()> {
        match notification {
            RuntimeNotification::TaskEvent(event) => {
                if event.session_epoch != session_epoch
                    || self.current_session_epoch() != session_epoch
                {
                    return Ok(());
                }
                let is_terminal = is_terminal_runtime_event(&event.event_type);
                let task_id = event.task_id;
                let attempt_no = event.attempt_no;
                send_runtime_task_event(sender, event).await?;
                if is_terminal {
                    cleanup_persisted_runtime_state(
                        &self.settings.agent.work_root,
                        task_id,
                        attempt_no,
                    );
                }
            }
            RuntimeNotification::TaskLogBatch(batch) => {
                if batch.session_epoch != session_epoch
                    || self.current_session_epoch() != session_epoch
                {
                    return Ok(());
                }
                send_runtime_log_batch(sender, batch).await?;
            }
            RuntimeNotification::TaskProgress(progress) => {
                if progress.session_epoch != session_epoch
                    || self.current_session_epoch() != session_epoch
                {
                    return Ok(());
                }
                send_runtime_progress(sender, progress).await?;
            }
            RuntimeNotification::TaskSnapshot(handle) => {
                if runtime_session_epoch(&handle) != session_epoch
                    || self.current_session_epoch() != session_epoch
                {
                    return Ok(());
                }
                send_task_snapshot(sender, &handle).await?;
            }
        }

        Ok(())
    }

    async fn forward_terminal_runtime_replay(
        &self,
        sender: &mpsc::Sender<AgentEnvelope>,
        replay: TerminalRuntimeReplay,
    ) -> anyhow::Result<()> {
        send_task_snapshot(sender, &replay.handle).await?;
        send_runtime_task_event(sender, replay.event.clone()).await?;
        cleanup_persisted_runtime_state(
            &self.settings.agent.work_root,
            replay.handle.task_id,
            replay.handle.attempt_no,
        );
        Ok(())
    }

    async fn handle_adopt_orphans(
        &self,
        sender: &mpsc::Sender<AgentEnvelope>,
        command: AdoptOrphans,
        session_epoch: u64,
    ) -> anyhow::Result<()> {
        let runtimes = command
            .runtimes
            .into_iter()
            .map(|runtime| {
                Ok(AdoptRuntimeFilter {
                    task_id: Uuid::parse_str(runtime.task_id.trim())
                        .context("invalid adopt_orphans.runtimes.task_id")?,
                    attempt_no: runtime.attempt_no,
                    lease_token: runtime.lease_token.trim().to_string(),
                    worker_kind: WorkerKind::from_str(runtime.worker_kind.trim())
                        .context("invalid adopt_orphans.runtimes.worker_kind")?,
                })
            })
            .collect::<Result<Vec<_>, anyhow::Error>>()?;

        if runtimes.is_empty() {
            return Ok(());
        }

        let adopted = self.executor.adopt_orphans(&AdoptFilter {
            session_epoch,
            runtimes: runtimes.clone(),
        });
        let adopted_keys = adopted
            .iter()
            .map(|handle| {
                (
                    handle.task_id,
                    handle.attempt_no,
                    handle.worker_kind,
                    handle
                        .metadata
                        .get("lease_token")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                )
            })
            .collect::<std::collections::HashSet<_>>();

        for runtime in runtimes {
            let key = (
                runtime.task_id,
                runtime.attempt_no,
                runtime.worker_kind,
                runtime.lease_token.clone(),
            );
            if adopted_keys.contains(&key) {
                continue;
            }
            send_task_event(
                sender,
                runtime.task_id,
                runtime.attempt_no,
                runtime.lease_token,
                "orphaned",
                "warn",
                "authorized runtime was not found locally",
                json!({
                    "worker_kind": runtime.worker_kind,
                    "reason": "runtime_not_found",
                }),
            )
            .await?;
        }

        for handle in adopted {
            send_task_snapshot(sender, &handle).await?;
        }

        Ok(())
    }

    async fn handle_start_task(
        &self,
        sender: &mpsc::Sender<AgentEnvelope>,
        command: StartTask,
        session_epoch: u64,
    ) -> anyhow::Result<()> {
        let mut request = parse_start_task(command)?;
        request.session_epoch = session_epoch;
        let sender = sender.clone();
        let executor = self.executor.clone();
        let start_task_permits = self.start_task_permits.clone();
        let session_guard = self.session_epoch.clone();

        tokio::spawn(async move {
            let Ok(_permit) = start_task_permits.acquire_owned().await else {
                return;
            };
            if session_guard.load(Ordering::SeqCst) != request.session_epoch {
                return;
            }

            let _ = send_task_event(
                &sender,
                request.task_id,
                request.attempt_no,
                request.lease_token.clone(),
                "accepted",
                "info",
                "task accepted by local executor",
                json!({
                    "worker_kind": request.task_type.default_worker_kind(),
                }),
            )
            .await;

            if session_guard.load(Ordering::SeqCst) != request.session_epoch {
                return;
            }

            match executor.start_task(&request) {
                Ok(handle) => {
                    if session_guard.load(Ordering::SeqCst) != request.session_epoch {
                        let _ = executor.stop_task(&StopTaskRequest {
                            task_id: request.task_id,
                            attempt_no: request.attempt_no,
                            lease_token: request.lease_token.clone(),
                            reason: "stale_session_replaced".to_string(),
                            grace_period_sec: 0,
                            force_after_sec: 1,
                        });
                        return;
                    }
                    let _ = send_task_event(
                        &sender,
                        request.task_id,
                        request.attempt_no,
                        request.lease_token.clone(),
                        "starting",
                        "info",
                        "runtime handle created",
                        json!({
                            "runtime_id": handle.runtime_id,
                            "worker_kind": handle.worker_kind,
                        }),
                    )
                    .await;
                    let _ = send_task_snapshot(&sender, &handle).await;
                }
                Err(error) => {
                    let handle = rejected_runtime_handle(&request);
                    let _ = send_task_event(
                        &sender,
                        request.task_id,
                        request.attempt_no,
                        request.lease_token.clone(),
                        "start_rejected",
                        "error",
                        error.to_string(),
                        json!({
                            "runtime_id": handle.runtime_id,
                            "worker_kind": handle.worker_kind,
                            "resolved_spec": request.resolved_spec,
                        }),
                    )
                    .await;
                }
            }
        });

        Ok(())
    }

    async fn handle_stop_task(
        &self,
        sender: &mpsc::Sender<AgentEnvelope>,
        command: StopTask,
    ) -> anyhow::Result<()> {
        let request = parse_stop_task(command)?;
        match self.executor.stop_task(&request) {
            Ok(()) => {
                send_task_event(
                    sender,
                    request.task_id,
                    request.attempt_no,
                    request.lease_token.clone(),
                    "stopping",
                    "info",
                    "stop request accepted",
                    json!({
                        "reason": request.reason,
                        "grace_period_sec": request.grace_period_sec,
                        "force_after_sec": request.force_after_sec,
                    }),
                )
                .await?;
                if let Some(handle) = self
                    .runtime_registry
                    .find_by_task_attempt(request.task_id, request.attempt_no)
                {
                    send_task_snapshot(sender, &handle).await?;
                }
            }
            Err(error) => {
                send_task_event(
                    sender,
                    request.task_id,
                    request.attempt_no,
                    request.lease_token.clone(),
                    "stop_rejected",
                    "error",
                    error.to_string(),
                    json!({
                        "reason": request.reason,
                    }),
                )
                .await?;
            }
        }

        Ok(())
    }

    async fn build_registration(&self) -> anyhow::Result<AgentRegistration> {
        let hostname = detect_hostname().unwrap_or_else(|| self.settings.agent.node_name.clone());
        let interfaces = discover_interfaces();
        let network_mode = NetworkMode::from_str(self.settings.agent.network_mode.trim())?;
        let zlm_server_id = self
            .capability_probe
            .zlm_server_id(&self.settings.agent)
            .await
            .unwrap_or_else(|| self.node_id.to_string());
        self.executor.set_zlm_server_id(zlm_server_id.clone());
        self.executor.set_zlm_rtmp_enhanced_enabled(
            self.capability_probe
                .zlm_rtmp_enhanced_enabled(&self.settings.agent)
                .await,
        );

        Ok(AgentRegistration {
            node_id: self.node_id,
            node_name: self.settings.agent.node_name.clone(),
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            hostname,
            labels: self.settings.agent.labels.clone(),
            interfaces,
            zlm_api_base: self.settings.agent.zlm_api_base.clone(),
            zlm_api_secret: self.settings.agent.zlm_api_secret.clone(),
            agent_stream_addr: self.settings.agent.agent_stream_addr.clone(),
            network_mode,
            ffmpeg_bin: self.settings.agent.ffmpeg_bin.clone(),
            ffprobe_bin: self.settings.agent.ffprobe_bin.clone(),
            zlm_server_id,
        })
    }
}

async fn send_capability_snapshot(
    sender: &mpsc::Sender<AgentEnvelope>,
    snapshot: &media_domain::CapabilitySnapshot,
) -> anyhow::Result<()> {
    send_agent_message(
        sender,
        AgentEnvelope {
            payload: Some(
                media_rpc::control_plane::agent_envelope::Payload::CapabilitySnapshot(
                    RpcCapabilitySnapshot {
                        ffmpeg_protocols: snapshot.ffmpeg_protocols.clone(),
                        ffmpeg_formats: snapshot.ffmpeg_formats.clone(),
                        ffmpeg_encoders: snapshot.ffmpeg_encoders.clone(),
                        ffmpeg_decoders: snapshot.ffmpeg_decoders.clone(),
                        zlm_version: snapshot.zlm_version.clone().unwrap_or_default(),
                        zlm_api_list: snapshot.zlm_api_list.clone(),
                        gpu: snapshot.gpu.clone(),
                        gpu_devices: snapshot
                            .gpu_devices
                            .iter()
                            .map(|device| RpcGpuDevice {
                                index: device.index,
                                uuid: device.uuid.clone(),
                                name: device.name.clone(),
                                memory_total_mb: device.memory_total_mb,
                            })
                            .collect(),
                    },
                ),
            ),
        },
    )
    .await
}

async fn send_task_snapshot(
    sender: &mpsc::Sender<AgentEnvelope>,
    handle: &RuntimeHandle,
) -> anyhow::Result<()> {
    send_agent_message(
        sender,
        AgentEnvelope {
            payload: Some(
                media_rpc::control_plane::agent_envelope::Payload::TaskSnapshot(TaskSnapshot {
                    runtime_id: handle.runtime_id.to_string(),
                    task_id: handle.task_id.to_string(),
                    attempt_no: handle.attempt_no,
                    lease_token: handle
                        .metadata
                        .get("lease_token")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    worker_kind: handle.worker_kind.as_str().to_string(),
                    pid: handle.pid.unwrap_or_default(),
                    state: handle.state.as_str().to_string(),
                    command_line: handle.command_line.clone().unwrap_or_default(),
                    outputs: handle.outputs.clone(),
                    metadata_json: serde_json::to_string(&handle.metadata)?,
                }),
            ),
        },
    )
    .await
}

async fn send_runtime_task_event(
    sender: &mpsc::Sender<AgentEnvelope>,
    event: RuntimeTaskEvent,
) -> anyhow::Result<()> {
    send_task_event(
        sender,
        event.task_id,
        event.attempt_no,
        event.lease_token,
        &event.event_type,
        &event.event_level,
        event.message,
        event.payload,
    )
    .await
}

async fn send_runtime_log_batch(
    sender: &mpsc::Sender<AgentEnvelope>,
    batch: RuntimeTaskLogBatch,
) -> anyhow::Result<()> {
    send_agent_message(
        sender,
        AgentEnvelope {
            payload: Some(
                media_rpc::control_plane::agent_envelope::Payload::TaskLogBatch(
                    media_rpc::control_plane::TaskLogBatch {
                        task_id: batch.task_id.to_string(),
                        attempt_no: batch.attempt_no,
                        lease_token: batch.lease_token,
                        stream: batch.stream,
                        lines: batch.lines,
                    },
                ),
            ),
        },
    )
    .await
}

fn try_send_runtime_log_batch(
    sender: &mpsc::Sender<AgentEnvelope>,
    mut batch: RuntimeTaskLogBatch,
    dropped_log_lines: &mut HashMap<(Uuid, i32, String), usize>,
) -> anyhow::Result<()> {
    let key = (batch.task_id, batch.attempt_no, batch.stream.clone());
    let source_line_count = batch.source_line_count;
    let suppressed = dropped_log_lines.remove(&key).unwrap_or(0);
    if suppressed > 0 {
        batch.lines.insert(
            0,
            format!("suppressed {suppressed} {} log lines", batch.stream),
        );
    }

    let envelope = AgentEnvelope {
        payload: Some(
            media_rpc::control_plane::agent_envelope::Payload::TaskLogBatch(
                media_rpc::control_plane::TaskLogBatch {
                    task_id: batch.task_id.to_string(),
                    attempt_no: batch.attempt_no,
                    lease_token: batch.lease_token.clone(),
                    stream: batch.stream.clone(),
                    lines: batch.lines,
                },
            ),
        ),
    };

    match sender.try_send(envelope) {
        Ok(()) => Ok(()),
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            *dropped_log_lines.entry(key).or_insert(0) += suppressed + source_line_count;
            Ok(())
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
            Err(anyhow::anyhow!("control-plane sender closed"))
        }
    }
}

async fn send_runtime_progress(
    sender: &mpsc::Sender<AgentEnvelope>,
    progress: RuntimeTaskProgress,
) -> anyhow::Result<()> {
    send_agent_message(
        sender,
        AgentEnvelope {
            payload: Some(
                media_rpc::control_plane::agent_envelope::Payload::TaskProgress(
                    media_rpc::control_plane::TaskProgress {
                        task_id: progress.task_id.to_string(),
                        attempt_no: progress.attempt_no,
                        lease_token: progress.lease_token,
                        frame: progress.frame,
                        fps: progress.fps,
                        bitrate_kbps: progress.bitrate_kbps,
                        speed: progress.speed,
                        out_time_ms: progress.out_time_ms,
                        dup_frames: progress.dup_frames,
                        drop_frames: progress.drop_frames,
                    },
                ),
            ),
        },
    )
    .await
}

async fn send_task_event(
    sender: &mpsc::Sender<AgentEnvelope>,
    task_id: Uuid,
    attempt_no: i32,
    lease_token: String,
    event_type: &str,
    event_level: &str,
    message: impl Into<String>,
    payload: Value,
) -> anyhow::Result<()> {
    send_agent_message(
        sender,
        AgentEnvelope {
            payload: Some(
                media_rpc::control_plane::agent_envelope::Payload::TaskEvent(TaskEvent {
                    task_id: task_id.to_string(),
                    attempt_no,
                    lease_token,
                    event_type: event_type.to_string(),
                    event_level: event_level.to_string(),
                    message: message.into(),
                    payload_json: serde_json::to_string(&payload)?,
                }),
            ),
        },
    )
    .await
}

async fn send_agent_message(
    sender: &mpsc::Sender<AgentEnvelope>,
    envelope: AgentEnvelope,
) -> anyhow::Result<()> {
    sender
        .send(envelope)
        .await
        .map_err(|_| anyhow::anyhow!("control-plane sender closed"))
}

fn registration_to_rpc(registration: &AgentRegistration) -> RpcRegister {
    RpcRegister {
        node_id: registration.node_id.to_string(),
        node_name: registration.node_name.clone(),
        agent_version: registration.agent_version.clone(),
        hostname: registration.hostname.clone(),
        labels: registration.labels.clone(),
        interfaces: registration.interfaces.clone(),
        zlm_api_base: registration.zlm_api_base.clone(),
        zlm_api_secret: registration.zlm_api_secret.clone(),
        agent_stream_addr: registration.agent_stream_addr.clone(),
        network_mode: registration.network_mode.as_str().to_string(),
        ffmpeg_bin: registration.ffmpeg_bin.clone(),
        ffprobe_bin: registration.ffprobe_bin.clone(),
        zlm_server_id: registration.zlm_server_id.clone(),
    }
}

fn parse_start_task(command: StartTask) -> anyhow::Result<StartTaskRequest> {
    Ok(StartTaskRequest {
        task_id: Uuid::parse_str(command.task_id.trim())?,
        attempt_no: command.attempt_no,
        task_type: TaskType::from_str(command.task_type.trim())?,
        resolved_spec: parse_json_field(&command.resolved_spec_json)?,
        execution_mode: command.execution_mode,
        lease_token: command.lease_token,
        trace_context: non_empty(command.trace_context),
        session_epoch: 0,
    })
}

fn parse_stop_task(command: StopTask) -> anyhow::Result<StopTaskRequest> {
    Ok(StopTaskRequest {
        task_id: Uuid::parse_str(command.task_id.trim())?,
        attempt_no: command.attempt_no,
        lease_token: command.lease_token.trim().to_string(),
        reason: command.reason,
        grace_period_sec: command.grace_period_sec,
        force_after_sec: command.force_after_sec,
    })
}

fn parse_json_field(value: &str) -> anyhow::Result<Value> {
    if value.trim().is_empty() {
        Ok(Value::Null)
    } else {
        Ok(serde_json::from_str(value)?)
    }
}

fn non_empty(value: String) -> Option<String> {
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn build_endpoint(settings: &crate::config::AgentSettings) -> anyhow::Result<Endpoint> {
    let mut endpoint = Endpoint::from_shared(settings.core_endpoint.clone())?
        .connect_timeout(Duration::from_secs(5))
        .tcp_keepalive(Some(Duration::from_secs(30)));

    if settings.core_endpoint.starts_with("https://") {
        let ca_pem = fs::read(&settings.ca_path)
            .with_context(|| format!("failed to read CA certificate {}", settings.ca_path))?;
        let cert_pem = fs::read(&settings.cert_path)
            .with_context(|| format!("failed to read client certificate {}", settings.cert_path))?;
        let key_pem = fs::read(&settings.key_path)
            .with_context(|| format!("failed to read client key {}", settings.key_path))?;

        let mut tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(ca_pem))
            .identity(Identity::from_pem(cert_pem, key_pem))
            .assume_http2(true);
        if !settings.tls_domain_name.trim().is_empty() {
            tls = tls.domain_name(settings.tls_domain_name.clone());
        }
        endpoint = endpoint.tls_config(tls)?;
    }

    Ok(endpoint)
}

fn detect_hostname() -> Option<String> {
    std::env::var("HOSTNAME")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            fs::read_to_string("/etc/hostname")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
}

fn discover_interfaces() -> Vec<String> {
    let mut interfaces = discover_interface_cidrs();
    if interfaces.is_empty() {
        interfaces = fs::read_dir("/sys/class/net")
            .ok()
            .into_iter()
            .flat_map(|entries| entries.filter_map(Result::ok))
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>();
    }
    interfaces.sort();
    interfaces.dedup();
    interfaces
}

fn discover_interface_cidrs() -> Vec<String> {
    let mut result = Vec::new();
    unsafe {
        let mut addrs: *mut libc::ifaddrs = ptr::null_mut();
        if libc::getifaddrs(&mut addrs) != 0 || addrs.is_null() {
            return result;
        }

        let mut current = addrs;
        while !current.is_null() {
            let ifa = &*current;
            if !ifa.ifa_addr.is_null() && !ifa.ifa_netmask.is_null() && !ifa.ifa_name.is_null() {
                let family = (*ifa.ifa_addr).sa_family as i32;
                let name = CStr::from_ptr(ifa.ifa_name).to_string_lossy().to_string();
                match family {
                    libc::AF_INET => {
                        let addr = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                        let netmask = &*(ifa.ifa_netmask as *const libc::sockaddr_in);
                        let ip = Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr));
                        let prefix = u32::from_be(netmask.sin_addr.s_addr).count_ones() as u8;
                        result.push(format!("{name}|{}/{}", IpAddr::V4(ip), prefix));
                    }
                    libc::AF_INET6 => {
                        let addr = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                        let netmask = &*(ifa.ifa_netmask as *const libc::sockaddr_in6);
                        let ip = Ipv6Addr::from(addr.sin6_addr.s6_addr);
                        let prefix = netmask
                            .sin6_addr
                            .s6_addr
                            .iter()
                            .map(|octet| octet.count_ones())
                            .sum::<u32>() as u8;
                        result.push(format!("{name}|{}/{}", IpAddr::V6(ip), prefix));
                    }
                    _ => {}
                }
            }
            current = ifa.ifa_next;
        }
        libc::freeifaddrs(addrs);
    }

    result
}

async fn recv_runtime_notification(
    receiver: Arc<Mutex<mpsc::UnboundedReceiver<RuntimeNotification>>>,
) -> Option<RuntimeNotification> {
    let mut receiver = receiver.lock().await;
    receiver.recv().await
}

async fn recv_runtime_log_batch(
    receiver: Arc<Mutex<mpsc::Receiver<RuntimeTaskLogBatch>>>,
) -> Option<RuntimeTaskLogBatch> {
    let mut receiver = receiver.lock().await;
    receiver.recv().await
}
