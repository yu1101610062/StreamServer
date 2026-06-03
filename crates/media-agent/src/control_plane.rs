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
use media_domain::{
    AgentRegistration, NetworkMode, RecordingControlSpec, RuntimeHandle, TaskType, WorkerKind,
    normalize_output_mount_relative_prefix,
};
use media_rpc::control_plane::{
    AdoptOrphans, AgentEnvelope, CapabilitySnapshot as RpcCapabilitySnapshot, CoreEnvelope,
    GpuDevice as RpcGpuDevice, GpuRuntime as RpcGpuRuntime, Heartbeat as RpcHeartbeat,
    ProbeCapabilities, Register as RpcRegister, StartTask, StopTask, TaskEvent,
    TaskRecordingControl, TaskSnapshot, control_plane_client::ControlPlaneClient,
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
    artifact_cleanup::ArtifactCleanupManager,
    capability::{CapabilityProbe, binary_available, probe_gpu_runtime},
    config::Settings,
    heartbeat::HeartbeatSampler,
    runtime::{
        AdoptFilter, AdoptRuntimeFilter, LocalExecutor, LocalRuntimeRegistry,
        ManagedProcessExecutor, RecordingControlAction, RuntimeEventSink, RuntimeNotification,
        RuntimeTaskEvent, RuntimeTaskLogBatch, RuntimeTaskProgress, StartTaskRequest,
        StopTaskRequest, TaskRecordingControlRequest, TerminalRuntimeReplay, bounded_log_batches,
        cleanup_persisted_runtime_state, collect_terminal_runtime_replays,
        is_terminal_runtime_event, rejected_runtime_handle, runtime_session_epoch,
    },
};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const CONTROL_BACKOFF: [u64; 5] = [1, 2, 5, 10, 30];
const CONTROL_BUFFER: usize = 32;
const CONTROL_MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
const LOG_NOTIFICATION_BUFFER: usize = 128;
const START_TASK_CONCURRENCY_LIMIT: usize = 4;
const STOP_TASK_CONCURRENCY_LIMIT: usize = 8;
const RECORDING_CONTROL_CONCURRENCY_LIMIT: usize = 4;
const ADOPT_ORPHANS_CONCURRENCY_LIMIT: usize = 1;

#[derive(Clone)]
pub struct AgentController {
    settings: Arc<Settings>,
    node_id: Uuid,
    capability_probe: CapabilityProbe,
    runtime_registry: LocalRuntimeRegistry,
    artifact_cleanup: ArtifactCleanupManager,
    executor: Arc<dyn LocalExecutor>,
    runtime_priority_events: Arc<Mutex<mpsc::UnboundedReceiver<RuntimeNotification>>>,
    runtime_log_batches: Arc<Mutex<mpsc::Receiver<RuntimeTaskLogBatch>>>,
    start_task_permits: Arc<Semaphore>,
    stop_task_permits: Arc<Semaphore>,
    recording_control_permits: Arc<Semaphore>,
    adopt_orphans_permits: Arc<Semaphore>,
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
        let artifact_cleanup = ArtifactCleanupManager::with_executor(
            &settings.agent,
            runtime_registry.clone(),
            Some(executor.clone()),
        );

        Ok(Self {
            settings: Arc::new(settings),
            node_id,
            capability_probe: CapabilityProbe::new()?,
            runtime_registry,
            artifact_cleanup,
            executor,
            runtime_priority_events: Arc::new(Mutex::new(runtime_priority_rx)),
            runtime_log_batches: Arc::new(Mutex::new(runtime_log_rx)),
            start_task_permits: Arc::new(Semaphore::new(START_TASK_CONCURRENCY_LIMIT)),
            stop_task_permits: Arc::new(Semaphore::new(STOP_TASK_CONCURRENCY_LIMIT)),
            recording_control_permits: Arc::new(Semaphore::new(
                RECORDING_CONTROL_CONCURRENCY_LIMIT,
            )),
            adopt_orphans_permits: Arc::new(Semaphore::new(ADOPT_ORPHANS_CONCURRENCY_LIMIT)),
            session_epoch: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn node_id(&self) -> Uuid {
        self.node_id
    }

    pub async fn run(self) {
        self.artifact_cleanup.refresh_now().await;
        self.artifact_cleanup.start_background();
        let mut backoff_idx = 0usize;

        // Agent 到 Core 只有一条长期控制流；断开后退避重连，避免任务控制通道永久丢失。
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
        if let Some(reason) = self.artifact_cleanup.control_plane_block_reason() {
            anyhow::bail!("artifact cleanup is not ready for control-plane registration: {reason}");
        }
        // session_epoch 用来丢弃上一条连接残留的日志/事件，防止重连后旧任务消息串入新会话。
        let session_epoch = self.session_epoch.fetch_add(1, Ordering::SeqCst) + 1;
        let result = self.connect_once_active(session_epoch).await;
        self.invalidate_session_epoch(session_epoch);
        result
    }

    async fn connect_once_active(&self, session_epoch: u64) -> anyhow::Result<()> {
        let endpoint = build_endpoint(&self.settings.agent)?;
        let channel = endpoint.connect().await?;
        let mut client = ControlPlaneClient::new(channel)
            .max_decoding_message_size(CONTROL_MAX_MESSAGE_BYTES)
            .max_encoding_message_size(CONTROL_MAX_MESSAGE_BYTES);

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

        // 注册成功后立即上报能力快照，并回放本地持久化的终态运行时，补齐断线窗口事件。
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
            Some(self.artifact_cleanup.clone()),
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
                // Core 命令优先级最高，避免心跳或日志批次阻塞启动/停止任务。
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
        // 心跳中的 slot 与清理状态会直接影响 Core 调度，因此这里每次都取实时快照。
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
                        upload_disk_total_bytes: snapshot.upload_disk_total_bytes,
                        upload_disk_available_bytes: snapshot.upload_disk_available_bytes,
                        upload_disk_used_percent: snapshot.upload_disk_used_percent,
                        running_tasks: snapshot.running_tasks,
                        starting_tasks: snapshot.starting_tasks,
                        stopping_tasks: snapshot.stopping_tasks,
                        orphaned_tasks: snapshot.orphaned_tasks,
                        slot_usage: snapshot.slot_usage,
                        zlm_alive: snapshot.zlm_alive,
                        ffmpeg_alive: snapshot.ffmpeg_alive,
                        artifact_cleanup_blocked: snapshot.artifact_cleanup_blocked,
                        artifact_cleanup_block_reason: snapshot
                            .artifact_cleanup_block_reason
                            .clone()
                            .unwrap_or_default(),
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
                self.handle_stop_task(sender, command, session_epoch)
                    .await?;
            }
            media_rpc::control_plane::core_envelope::Payload::TaskRecordingControl(command) => {
                self.handle_task_recording_control(sender, command, session_epoch)
                    .await?;
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

        let sender = sender.clone();
        let executor = self.executor.clone();
        let adopt_orphans_permits = self.adopt_orphans_permits.clone();
        let session_guard = self.session_epoch.clone();

        tokio::spawn(async move {
            let Ok(_permit) = adopt_orphans_permits.acquire_owned().await else {
                return;
            };
            if !session_is_current(&session_guard, session_epoch) {
                return;
            }

            let adopted = executor
                .adopt_orphans(AdoptFilter {
                    session_epoch,
                    runtimes: runtimes.clone(),
                })
                .await;
            if !session_is_current(&session_guard, session_epoch) {
                return;
            }

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
                if !session_is_current(&session_guard, session_epoch) {
                    return;
                }
                let key = (
                    runtime.task_id,
                    runtime.attempt_no,
                    runtime.worker_kind,
                    runtime.lease_token.clone(),
                );
                if adopted_keys.contains(&key) {
                    continue;
                }
                let _ = send_task_event(
                    &sender,
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
                .await;
            }

            for handle in adopted {
                if !session_is_current(&session_guard, session_epoch)
                    || runtime_session_epoch(&handle) != session_epoch
                {
                    return;
                }
                let _ = send_task_snapshot(&sender, &handle).await;
            }
        });

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
        let artifact_cleanup = self.artifact_cleanup.clone();
        let start_task_permits = self.start_task_permits.clone();
        let session_guard = self.session_epoch.clone();

        tokio::spawn(async move {
            let Ok(_permit) = start_task_permits.acquire_owned().await else {
                return;
            };
            if session_guard.load(Ordering::SeqCst) != request.session_epoch {
                return;
            }

            if let Err(error) = artifact_cleanup.ensure_task_start_allowed(&request.resolved_spec) {
                if !session_is_current(&session_guard, request.session_epoch) {
                    return;
                }
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
                return;
            }

            match executor.start_task(request.clone()).await {
                Ok(handle) => {
                    if !session_is_current(&session_guard, request.session_epoch) {
                        let _ = executor
                            .stop_task(StopTaskRequest {
                                task_id: request.task_id,
                                attempt_no: request.attempt_no,
                                lease_token: request.lease_token.clone(),
                                reason: "stale_session_replaced".to_string(),
                                grace_period_sec: 0,
                                force_after_sec: 1,
                            })
                            .await;
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
                    if !session_is_current(&session_guard, request.session_epoch) {
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
                    if session_is_current(&session_guard, request.session_epoch)
                        && runtime_session_epoch(&handle) == request.session_epoch
                    {
                        let _ = send_task_snapshot(&sender, &handle).await;
                    }
                }
                Err(error) => {
                    if !session_is_current(&session_guard, request.session_epoch) {
                        return;
                    }
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
        session_epoch: u64,
    ) -> anyhow::Result<()> {
        let request = parse_stop_task(command)?;
        let sender = sender.clone();
        let executor = self.executor.clone();
        let runtime_registry = self.runtime_registry.clone();
        let stop_task_permits = self.stop_task_permits.clone();
        let session_guard = self.session_epoch.clone();

        tokio::spawn(async move {
            let Ok(_permit) = stop_task_permits.acquire_owned().await else {
                return;
            };
            if !session_is_current(&session_guard, session_epoch) {
                return;
            }

            match executor.stop_task(request.clone()).await {
                Ok(()) => {
                    if !session_is_current(&session_guard, session_epoch) {
                        return;
                    }
                    let _ = send_task_event(
                        &sender,
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
                    .await;
                    if let Some(handle) =
                        runtime_registry.find_by_task_attempt(request.task_id, request.attempt_no)
                    {
                        if session_is_current(&session_guard, session_epoch)
                            && runtime_session_epoch(&handle) == session_epoch
                        {
                            let _ = send_task_snapshot(&sender, &handle).await;
                        }
                    }
                }
                Err(error) => {
                    if !session_is_current(&session_guard, session_epoch) {
                        return;
                    }
                    let _ = send_task_event(
                        &sender,
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
                    .await;
                }
            }
        });

        Ok(())
    }

    async fn handle_task_recording_control(
        &self,
        sender: &mpsc::Sender<AgentEnvelope>,
        command: TaskRecordingControl,
        session_epoch: u64,
    ) -> anyhow::Result<()> {
        let request = parse_task_recording_control(command)?;
        let sender = sender.clone();
        let executor = self.executor.clone();
        let recording_control_permits = self.recording_control_permits.clone();
        let session_guard = self.session_epoch.clone();

        tokio::spawn(async move {
            let Ok(_permit) = recording_control_permits.acquire_owned().await else {
                return;
            };
            if !session_is_current(&session_guard, session_epoch) {
                return;
            }

            match executor.set_task_recording(request.clone()).await {
                Ok(handle) => {
                    if session_is_current(&session_guard, session_epoch)
                        && runtime_session_epoch(&handle) == session_epoch
                    {
                        let _ = send_task_snapshot(&sender, &handle).await;
                    }
                }
                Err(error) => {
                    if !session_is_current(&session_guard, session_epoch) {
                        return;
                    }
                    let _ = send_task_event(
                        &sender,
                        request.task_id,
                        request.attempt_no,
                        request.lease_token.clone(),
                        "recording_control_failed",
                        "error",
                        error.to_string(),
                        json!({
                            "command_id": request.command_id,
                            "action": recording_control_action_name(request.action),
                            "reason": request.reason,
                        }),
                    )
                    .await;
                }
            }
        });

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
        let output_mount_relative_prefix_mp4 = normalize_output_mount_relative_prefix(
            &self.settings.agent.output_mount_relative_prefix_mp4,
        )
        .map_err(|error| anyhow::anyhow!("invalid OUTPUT_MOUNT_RELATIVE_PREFIX_MP4: {error}"))?;
        let output_mount_relative_prefix_hls = normalize_output_mount_relative_prefix(
            &self.settings.agent.output_mount_relative_prefix_hls,
        )
        .map_err(|error| anyhow::anyhow!("invalid OUTPUT_MOUNT_RELATIVE_PREFIX_HLS: {error}"))?;
        let agent_http_base_url = build_agent_http_base_url(
            &self.settings.agent.agent_stream_addr,
            &self.settings.agent.http_addr,
        )?;

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
            agent_http_base_url,
            zlm_rtmp_port: self.settings.agent.zlm_rtmp_port,
            zlm_rtsp_port: self.settings.agent.zlm_rtsp_port,
            network_mode,
            ffmpeg_bin: self.settings.agent.ffmpeg_bin.clone(),
            ffprobe_bin: self.settings.agent.ffprobe_bin.clone(),
            zlm_server_id,
            output_mount_relative_prefix_mp4,
            output_mount_relative_prefix_hls,
        })
    }
}

fn session_is_current(session_guard: &Arc<AtomicU64>, session_epoch: u64) -> bool {
    session_guard.load(Ordering::SeqCst) == session_epoch
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
    for batch in bounded_log_batches(batch) {
        send_agent_message(sender, log_batch_envelope(batch)).await?;
    }
    Ok(())
}

fn try_send_runtime_log_batch(
    sender: &mpsc::Sender<AgentEnvelope>,
    mut batch: RuntimeTaskLogBatch,
    dropped_log_lines: &mut HashMap<(Uuid, i32, String), usize>,
) -> anyhow::Result<()> {
    let key = (batch.task_id, batch.attempt_no, batch.stream.clone());
    let suppressed = dropped_log_lines.remove(&key).unwrap_or(0);
    if suppressed > 0 {
        batch.lines.insert(
            0,
            format!("suppressed {suppressed} {} log lines", batch.stream),
        );
    }

    let batches = bounded_log_batches(batch);
    for (index, batch) in batches.iter().cloned().enumerate() {
        match sender.try_send(log_batch_envelope(batch)) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                let mut unsent = batches
                    .iter()
                    .skip(index)
                    .map(|batch| batch.source_line_count)
                    .sum::<usize>();
                if index == 0 {
                    unsent += suppressed;
                }
                *dropped_log_lines.entry(key).or_insert(0) += unsent;
                return Ok(());
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                return Err(anyhow::anyhow!("control-plane sender closed"));
            }
        }
    }

    Ok(())
}

fn log_batch_envelope(batch: RuntimeTaskLogBatch) -> AgentEnvelope {
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
        agent_http_base_url: registration.agent_http_base_url.clone(),
        zlm_rtmp_port: u32::from(registration.zlm_rtmp_port),
        zlm_rtsp_port: u32::from(registration.zlm_rtsp_port),
        network_mode: registration.network_mode.as_str().to_string(),
        ffmpeg_bin: registration.ffmpeg_bin.clone(),
        ffprobe_bin: registration.ffprobe_bin.clone(),
        zlm_server_id: registration.zlm_server_id.clone(),
        output_mount_relative_prefix_mp4: registration.output_mount_relative_prefix_mp4.clone(),
        output_mount_relative_prefix_hls: registration.output_mount_relative_prefix_hls.clone(),
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

fn parse_task_recording_control(
    command: TaskRecordingControl,
) -> anyhow::Result<TaskRecordingControlRequest> {
    let action = match command.action.trim() {
        "start" => RecordingControlAction::Start,
        "stop" => RecordingControlAction::Stop,
        other => anyhow::bail!("unsupported recording control action {other}"),
    };
    let record = if command.record_config_json.trim().is_empty() {
        None
    } else {
        Some(serde_json::from_str::<RecordingControlSpec>(
            &command.record_config_json,
        )?)
    };
    Ok(TaskRecordingControlRequest {
        task_id: Uuid::parse_str(command.task_id.trim())?,
        attempt_no: command.attempt_no,
        lease_token: command.lease_token.trim().to_string(),
        action,
        record,
        reason: command.reason.trim().to_string(),
        command_id: command.command_id.trim().to_string(),
    })
}

fn recording_control_action_name(action: RecordingControlAction) -> &'static str {
    match action {
        RecordingControlAction::Start => "start",
        RecordingControlAction::Stop => "stop",
    }
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

fn build_agent_http_base_url(agent_stream_addr: &str, http_addr: &str) -> anyhow::Result<String> {
    let stream_url = reqwest::Url::parse(agent_stream_addr.trim())
        .with_context(|| format!("invalid AGENT_STREAM_ADDR: {agent_stream_addr}"))?;
    let scheme = stream_url.scheme();
    let host = stream_url
        .host()
        .ok_or_else(|| anyhow::anyhow!("AGENT_STREAM_ADDR host missing"))?;
    let port = parse_http_addr_port(http_addr)?;
    let url = reqwest::Url::parse(&format!("{scheme}://{host}:{port}"))
        .with_context(|| format!("build agent http base url from {agent_stream_addr}"))?;
    Ok(url.to_string().trim_end_matches('/').to_string())
}

fn parse_http_addr_port(http_addr: &str) -> anyhow::Result<u16> {
    let trimmed = http_addr.trim();
    if let Ok(addr) = trimmed.parse::<std::net::SocketAddr>() {
        return Ok(addr.port());
    }

    let port = if let Some(close_bracket) = trimmed.rfind(']') {
        trimmed
            .get(close_bracket + 1..)
            .and_then(|suffix| suffix.strip_prefix(':'))
    } else {
        trimmed.rsplit_once(':').map(|(_, port)| port)
    }
    .ok_or_else(|| anyhow::anyhow!("AGENT_HTTP_ADDR must include a port: {http_addr}"))?;

    let port = port
        .parse::<u16>()
        .with_context(|| format!("invalid AGENT_HTTP_ADDR port: {http_addr}"))?;
    anyhow::ensure!(port > 0, "AGENT_HTTP_ADDR port must be greater than 0");
    Ok(port)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    use crate::runtime::ExecutorError;
    use chrono::Utc;
    use media_domain::{RuntimeState, WorkerKind};
    use tokio::{
        sync::{Mutex as TokioMutex, oneshot},
        time::{Duration as TokioDuration, timeout},
    };

    struct TestExecutor {
        start_gate: TokioMutex<Option<oneshot::Receiver<()>>>,
        stop_gate: TokioMutex<Option<oneshot::Receiver<()>>>,
        recording_gate: TokioMutex<Option<oneshot::Receiver<()>>>,
        adopt_gate: TokioMutex<Option<oneshot::Receiver<()>>>,
        adopt_result: TokioMutex<Vec<RuntimeHandle>>,
        start_calls: AtomicUsize,
        stop_calls: AtomicUsize,
        recording_calls: AtomicUsize,
        adopt_calls: AtomicUsize,
        adopt_active: AtomicUsize,
        max_adopt_active: AtomicUsize,
    }

    impl Default for TestExecutor {
        fn default() -> Self {
            Self {
                start_gate: TokioMutex::new(None),
                stop_gate: TokioMutex::new(None),
                recording_gate: TokioMutex::new(None),
                adopt_gate: TokioMutex::new(None),
                adopt_result: TokioMutex::new(Vec::new()),
                start_calls: AtomicUsize::new(0),
                stop_calls: AtomicUsize::new(0),
                recording_calls: AtomicUsize::new(0),
                adopt_calls: AtomicUsize::new(0),
                adopt_active: AtomicUsize::new(0),
                max_adopt_active: AtomicUsize::new(0),
            }
        }
    }

    impl TestExecutor {
        async fn block_start(&self) -> oneshot::Sender<()> {
            install_gate(&self.start_gate).await
        }

        async fn block_stop(&self) -> oneshot::Sender<()> {
            install_gate(&self.stop_gate).await
        }

        async fn block_recording(&self) -> oneshot::Sender<()> {
            install_gate(&self.recording_gate).await
        }

        async fn block_adopt(&self) -> oneshot::Sender<()> {
            install_gate(&self.adopt_gate).await
        }
    }

    #[tonic::async_trait]
    impl LocalExecutor for TestExecutor {
        async fn start_task(
            &self,
            request: StartTaskRequest,
        ) -> Result<RuntimeHandle, ExecutorError> {
            self.start_calls.fetch_add(1, AtomicOrdering::SeqCst);
            wait_gate(&self.start_gate).await;
            Ok(test_handle(
                request.task_id,
                request.attempt_no,
                request.lease_token,
                request.task_type.default_worker_kind(),
                request.session_epoch,
            ))
        }

        async fn stop_task(&self, _request: StopTaskRequest) -> Result<(), ExecutorError> {
            self.stop_calls.fetch_add(1, AtomicOrdering::SeqCst);
            wait_gate(&self.stop_gate).await;
            Ok(())
        }

        async fn set_task_recording(
            &self,
            request: TaskRecordingControlRequest,
        ) -> Result<RuntimeHandle, ExecutorError> {
            self.recording_calls.fetch_add(1, AtomicOrdering::SeqCst);
            wait_gate(&self.recording_gate).await;
            Ok(test_handle(
                request.task_id,
                request.attempt_no,
                request.lease_token,
                WorkerKind::ZlmProxy,
                1,
            ))
        }

        async fn adopt_orphans(&self, _filter: AdoptFilter) -> Vec<RuntimeHandle> {
            self.adopt_calls.fetch_add(1, AtomicOrdering::SeqCst);
            let active = self
                .adopt_active
                .fetch_add(1, AtomicOrdering::SeqCst)
                .saturating_add(1);
            record_max(&self.max_adopt_active, active);
            wait_gate(&self.adopt_gate).await;
            self.adopt_active.fetch_sub(1, AtomicOrdering::SeqCst);
            self.adopt_result.lock().await.clone()
        }
    }

    async fn install_gate(gate: &TokioMutex<Option<oneshot::Receiver<()>>>) -> oneshot::Sender<()> {
        let (tx, rx) = oneshot::channel();
        *gate.lock().await = Some(rx);
        tx
    }

    async fn wait_gate(gate: &TokioMutex<Option<oneshot::Receiver<()>>>) {
        let gate = { gate.lock().await.take() };
        if let Some(gate) = gate {
            let _ = gate.await;
        }
    }

    fn record_max(max: &AtomicUsize, value: usize) {
        let mut current = max.load(AtomicOrdering::SeqCst);
        while value > current {
            match max.compare_exchange(
                current,
                value,
                AtomicOrdering::SeqCst,
                AtomicOrdering::SeqCst,
            ) {
                Ok(_) => return,
                Err(next) => current = next,
            }
        }
    }

    async fn wait_for_counter(counter: &AtomicUsize, expected: usize) {
        timeout(TokioDuration::from_millis(500), async {
            loop {
                if counter.load(AtomicOrdering::SeqCst) >= expected {
                    break;
                }
                tokio::time::sleep(TokioDuration::from_millis(5)).await;
            }
        })
        .await
        .expect("counter should reach expected value");
    }

    fn test_controller(executor: Arc<TestExecutor>) -> AgentController {
        let agent = crate::config::AgentSettings {
            ffmpeg_bin: "true".to_string(),
            zlm_api_base: String::new(),
            work_root: ".".to_string(),
            zlm_output_mp4_root: "/tmp/streamserver-control-plane-test/mp4".to_string(),
            zlm_output_hls_root: "/tmp/streamserver-control-plane-test/hls".to_string(),
            artifact_cleanup: crate::config::AgentArtifactCleanupSettings {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let settings = Settings {
            environment: "test".to_string(),
            logging: crate::config::LoggingSettings::default(),
            agent,
        };
        let runtime_registry = LocalRuntimeRegistry::new();
        let (_runtime_priority_tx, runtime_priority_rx) = mpsc::unbounded_channel();
        let (_runtime_log_tx, runtime_log_rx) = mpsc::channel(LOG_NOTIFICATION_BUFFER);
        let executor: Arc<dyn LocalExecutor> = executor;
        let artifact_cleanup = ArtifactCleanupManager::with_executor(
            &settings.agent,
            runtime_registry.clone(),
            Some(executor.clone()),
        );

        AgentController {
            settings: Arc::new(settings),
            node_id: Uuid::now_v7(),
            capability_probe: CapabilityProbe::new().expect("capability probe should build"),
            runtime_registry,
            artifact_cleanup,
            executor,
            runtime_priority_events: Arc::new(Mutex::new(runtime_priority_rx)),
            runtime_log_batches: Arc::new(Mutex::new(runtime_log_rx)),
            start_task_permits: Arc::new(Semaphore::new(START_TASK_CONCURRENCY_LIMIT)),
            stop_task_permits: Arc::new(Semaphore::new(STOP_TASK_CONCURRENCY_LIMIT)),
            recording_control_permits: Arc::new(Semaphore::new(
                RECORDING_CONTROL_CONCURRENCY_LIMIT,
            )),
            adopt_orphans_permits: Arc::new(Semaphore::new(ADOPT_ORPHANS_CONCURRENCY_LIMIT)),
            session_epoch: Arc::new(AtomicU64::new(1)),
        }
    }

    fn test_handle(
        task_id: Uuid,
        attempt_no: i32,
        lease_token: String,
        worker_kind: WorkerKind,
        session_epoch: u64,
    ) -> RuntimeHandle {
        RuntimeHandle {
            runtime_id: Uuid::now_v7(),
            task_id,
            attempt_no,
            worker_kind,
            pid: Some(1),
            started_at: Utc::now(),
            last_progress_at: None,
            state: RuntimeState::Running,
            command_line: None,
            outputs: Vec::new(),
            metadata: json!({
                "lease_token": lease_token,
                "session_epoch": session_epoch,
            }),
        }
    }

    fn sender_pair() -> (mpsc::Sender<AgentEnvelope>, mpsc::Receiver<AgentEnvelope>) {
        mpsc::channel(CONTROL_BUFFER)
    }

    async fn recv_agent_envelope(receiver: &mut mpsc::Receiver<AgentEnvelope>) -> AgentEnvelope {
        timeout(TokioDuration::from_millis(500), receiver.recv())
            .await
            .expect("agent envelope should be sent")
            .expect("sender should stay open")
    }

    async fn assert_no_agent_envelope(receiver: &mut mpsc::Receiver<AgentEnvelope>) {
        assert!(
            timeout(TokioDuration::from_millis(100), receiver.recv())
                .await
                .is_err(),
            "stale session should not send an agent envelope"
        );
    }

    fn event_type(envelope: &AgentEnvelope) -> Option<&str> {
        match envelope.payload.as_ref()? {
            media_rpc::control_plane::agent_envelope::Payload::TaskEvent(event) => {
                Some(event.event_type.as_str())
            }
            _ => None,
        }
    }

    fn is_heartbeat(envelope: &AgentEnvelope) -> bool {
        matches!(
            envelope.payload.as_ref(),
            Some(media_rpc::control_plane::agent_envelope::Payload::Heartbeat(_))
        )
    }

    fn stop_envelope(task_id: Uuid) -> CoreEnvelope {
        CoreEnvelope {
            payload: Some(media_rpc::control_plane::core_envelope::Payload::StopTask(
                StopTask {
                    task_id: task_id.to_string(),
                    attempt_no: 1,
                    reason: "test".to_string(),
                    grace_period_sec: 1,
                    force_after_sec: 2,
                    lease_token: "lease".to_string(),
                },
            )),
        }
    }

    fn recording_envelope(task_id: Uuid) -> CoreEnvelope {
        CoreEnvelope {
            payload: Some(
                media_rpc::control_plane::core_envelope::Payload::TaskRecordingControl(
                    TaskRecordingControl {
                        task_id: task_id.to_string(),
                        attempt_no: 1,
                        lease_token: "lease".to_string(),
                        action: "start".to_string(),
                        record_config_json: "{}".to_string(),
                        reason: "test".to_string(),
                        command_id: Uuid::now_v7().to_string(),
                    },
                ),
            ),
        }
    }

    fn adopt_envelope(task_id: Uuid) -> CoreEnvelope {
        CoreEnvelope {
            payload: Some(
                media_rpc::control_plane::core_envelope::Payload::AdoptOrphans(AdoptOrphans {
                    runtimes: vec![media_rpc::control_plane::ReclaimRuntime {
                        task_id: task_id.to_string(),
                        attempt_no: 1,
                        lease_token: "lease".to_string(),
                        worker_kind: WorkerKind::ZlmProxy.as_str().to_string(),
                    }],
                }),
            ),
        }
    }

    fn start_envelope(task_id: Uuid) -> CoreEnvelope {
        CoreEnvelope {
            payload: Some(media_rpc::control_plane::core_envelope::Payload::StartTask(
                StartTask {
                    task_id: task_id.to_string(),
                    attempt_no: 1,
                    task_type: "stream_ingest".to_string(),
                    resolved_spec_json: json!({
                        "type": "stream_ingest",
                        "name": "test",
                        "input": {
                            "kind": "rtsp",
                            "source_mode": "live",
                            "url": "rtsp://127.0.0.1/live"
                        },
                        "stream": {
                            "app": "live",
                            "name": "test"
                        },
                        "record": {
                            "enabled": false
                        }
                    })
                    .to_string(),
                    execution_mode: "managed".to_string(),
                    lease_token: "lease".to_string(),
                    trace_context: String::new(),
                },
            )),
        }
    }

    #[test]
    fn agent_http_base_url_uses_stream_host_and_http_addr_port() {
        let base = build_agent_http_base_url("http://172.17.13.196:80", "0.0.0.0:18081")
            .expect("base url should build");

        assert_eq!(base, "http://172.17.13.196:18081");
    }

    #[test]
    fn agent_http_base_url_supports_ipv6_stream_hosts() {
        let base = build_agent_http_base_url("http://[2001:db8::1]:80", "[::]:8081")
            .expect("base url should build");

        assert_eq!(base, "http://[2001:db8::1]:8081");
    }

    #[test]
    fn parse_http_addr_port_rejects_missing_port() {
        assert!(parse_http_addr_port("0.0.0.0").is_err());
    }

    #[tokio::test]
    async fn stop_task_job_does_not_block_runtime_notifications() {
        let executor = Arc::new(TestExecutor::default());
        let release_stop = executor.block_stop().await;
        let controller = test_controller(executor.clone());
        let (sender, mut receiver) = sender_pair();
        let task_id = Uuid::now_v7();

        timeout(
            TokioDuration::from_millis(100),
            controller.handle_core_envelope(&sender, stop_envelope(task_id), 1),
        )
        .await
        .expect("stop handler should return immediately")
        .expect("stop handler should succeed");
        wait_for_counter(&executor.stop_calls, 1).await;

        controller
            .forward_runtime_notification(
                &sender,
                RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                    task_id,
                    attempt_no: 1,
                    lease_token: "lease".to_string(),
                    session_epoch: 1,
                    event_type: "runtime_notification".to_string(),
                    event_level: "info".to_string(),
                    message: "runtime notification forwarded".to_string(),
                    payload: json!({}),
                }),
                1,
            )
            .await
            .expect("runtime notification should forward");

        let envelope = recv_agent_envelope(&mut receiver).await;
        assert_eq!(event_type(&envelope), Some("runtime_notification"));
        let _ = release_stop.send(());
    }

    #[tokio::test]
    async fn recording_job_does_not_block_heartbeat_path() {
        let executor = Arc::new(TestExecutor::default());
        let release_recording = executor.block_recording().await;
        let controller = test_controller(executor.clone());
        let (sender, mut receiver) = sender_pair();
        let task_id = Uuid::now_v7();

        timeout(
            TokioDuration::from_millis(100),
            controller.handle_core_envelope(&sender, recording_envelope(task_id), 1),
        )
        .await
        .expect("recording handler should return immediately")
        .expect("recording handler should succeed");
        wait_for_counter(&executor.recording_calls, 1).await;

        let mut sampler = HeartbeatSampler::new(".", 2, None);
        controller
            .send_heartbeat(&sender, &mut sampler)
            .await
            .expect("heartbeat should send while recording job is blocked");
        let envelope = recv_agent_envelope(&mut receiver).await;
        assert!(is_heartbeat(&envelope));
        let _ = release_recording.send(());
    }

    #[tokio::test]
    async fn adopt_job_does_not_block_later_core_commands() {
        let executor = Arc::new(TestExecutor::default());
        let release_adopt = executor.block_adopt().await;
        let controller = test_controller(executor.clone());
        let (sender, _receiver) = sender_pair();
        let task_id = Uuid::now_v7();

        controller
            .handle_core_envelope(&sender, adopt_envelope(task_id), 1)
            .await
            .expect("adopt handler should succeed");
        wait_for_counter(&executor.adopt_calls, 1).await;

        timeout(
            TokioDuration::from_millis(100),
            controller.handle_core_envelope(&sender, stop_envelope(task_id), 1),
        )
        .await
        .expect("later stop handler should enter while adopt job is blocked")
        .expect("stop handler should succeed");
        wait_for_counter(&executor.stop_calls, 1).await;
        let _ = release_adopt.send(());
    }

    #[tokio::test]
    async fn adopt_jobs_are_limited_to_one_active_executor_call() {
        let executor = Arc::new(TestExecutor::default());
        let release_adopt = executor.block_adopt().await;
        let controller = test_controller(executor.clone());
        let (sender, _receiver) = sender_pair();

        controller
            .handle_core_envelope(&sender, adopt_envelope(Uuid::now_v7()), 1)
            .await
            .expect("first adopt handler should succeed");
        controller
            .handle_core_envelope(&sender, adopt_envelope(Uuid::now_v7()), 1)
            .await
            .expect("second adopt handler should succeed");
        wait_for_counter(&executor.adopt_calls, 1).await;
        tokio::time::sleep(TokioDuration::from_millis(50)).await;

        assert_eq!(executor.adopt_calls.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(executor.max_adopt_active.load(AtomicOrdering::SeqCst), 1);
        let _ = release_adopt.send(());
    }

    #[tokio::test]
    async fn stale_session_jobs_do_not_send_old_events() {
        let executor = Arc::new(TestExecutor::default());
        let controller = test_controller(executor.clone());
        let (sender, mut receiver) = sender_pair();
        let release_stop = executor.block_stop().await;

        controller
            .handle_core_envelope(&sender, stop_envelope(Uuid::now_v7()), 1)
            .await
            .expect("stop handler should succeed");
        wait_for_counter(&executor.stop_calls, 1).await;
        controller.session_epoch.store(2, Ordering::SeqCst);
        let _ = release_stop.send(());
        assert_no_agent_envelope(&mut receiver).await;

        let executor = Arc::new(TestExecutor::default());
        let controller = test_controller(executor.clone());
        let (sender, mut receiver) = sender_pair();
        let release_recording = executor.block_recording().await;

        controller
            .handle_core_envelope(&sender, recording_envelope(Uuid::now_v7()), 1)
            .await
            .expect("recording handler should succeed");
        wait_for_counter(&executor.recording_calls, 1).await;
        controller.session_epoch.store(2, Ordering::SeqCst);
        let _ = release_recording.send(());
        assert_no_agent_envelope(&mut receiver).await;

        let executor = Arc::new(TestExecutor::default());
        let controller = test_controller(executor.clone());
        let (sender, mut receiver) = sender_pair();
        let release_adopt = executor.block_adopt().await;

        controller
            .handle_core_envelope(&sender, adopt_envelope(Uuid::now_v7()), 1)
            .await
            .expect("adopt handler should succeed");
        wait_for_counter(&executor.adopt_calls, 1).await;
        controller.session_epoch.store(2, Ordering::SeqCst);
        let _ = release_adopt.send(());
        assert_no_agent_envelope(&mut receiver).await;

        let executor = Arc::new(TestExecutor::default());
        let controller = test_controller(executor.clone());
        let (sender, mut receiver) = sender_pair();
        let release_start = executor.block_start().await;

        controller
            .handle_core_envelope(&sender, start_envelope(Uuid::now_v7()), 1)
            .await
            .expect("start handler should succeed");
        wait_for_counter(&executor.start_calls, 1).await;
        controller.session_epoch.store(2, Ordering::SeqCst);
        let _ = release_start.send(());
        wait_for_counter(&executor.stop_calls, 1).await;
        assert_no_agent_envelope(&mut receiver).await;
    }

    #[test]
    fn runtime_command_job_semaphores_match_expected_limits() {
        let controller = test_controller(Arc::new(TestExecutor::default()));

        assert_eq!(
            controller.start_task_permits.available_permits(),
            START_TASK_CONCURRENCY_LIMIT
        );
        assert_eq!(
            controller.stop_task_permits.available_permits(),
            STOP_TASK_CONCURRENCY_LIMIT
        );
        assert_eq!(
            controller.recording_control_permits.available_permits(),
            RECORDING_CONTROL_CONCURRENCY_LIMIT
        );
        assert_eq!(
            controller.adopt_orphans_permits.available_permits(),
            ADOPT_ORPHANS_CONCURRENCY_LIMIT
        );
    }
}
