use std::{
    collections::HashMap,
    ffi::CStr,
    fs,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    pin::Pin,
    ptr,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::Poll,
    time::Duration,
};

use anyhow::Context;
use axum::http::StatusCode;
use media_domain::{
    AgentRegistration, NetworkMode, RecordingControlSpec, RuntimeHandle, RuntimeSlotLoad,
    RuntimeState, SourceMode, TaskType, WorkerKind, normalize_output_mount_relative_prefix,
};
use media_rpc::control_plane::{
    AdoptOrphans, AgentEnvelope, CapabilitySnapshot as RpcCapabilitySnapshot, CoreEnvelope,
    GpuDevice as RpcGpuDevice, GpuRuntime as RpcGpuRuntime, Heartbeat as RpcHeartbeat,
    ProbeCapabilities, Register as RpcRegister, RuntimeSlotLoad as RpcRuntimeSlotLoad, StartTask,
    StopTask, TaskEvent, TaskRecordingControl, TaskSnapshot, ZlmHookRequest as RpcZlmHookRequest,
    ZlmHookResponse as RpcZlmHookResponse, control_plane_client::ControlPlaneClient,
};
use serde_json::{Value, json};
use tokio::{
    sync::{Mutex, mpsc},
    time::{Instant, sleep, sleep_until},
};
use tokio_stream::{Stream, StreamExt, wrappers::ReceiverStream};
use tonic::{
    Status,
    transport::{Certificate, ClientTlsConfig, Endpoint, Identity},
};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    artifact_cleanup::ArtifactCleanupManager,
    capability::{CapabilityProbe, binary_available, probe_gpu_runtime},
    config::{AgentEnvironment, AgentSettings, Settings},
    heartbeat::{HeartbeatSampleInput, HeartbeatSampler},
    identity::{
        AgentIdentityLoadError, AgentIdentityStore, AuthenticatedRotationAction,
        CertificateRotationActivatedData, CertificateRotationRequestData, LoadedIdentity,
        RotationCommitOutcome,
    },
    runtime::{
        AdoptFilter, AdoptRuntimeFilter, RecordingControlAction, RuntimeEventSink, RuntimeManager,
        RuntimeManagerHandle, RuntimeManagerRequestOutcome, RuntimeNotification, RuntimeReadHandle,
        RuntimeReadModel, RuntimeTaskEvent, RuntimeTaskLogBatch, RuntimeTaskProgress,
        StartTaskRequest, StopTaskRequest, TaskRecordingControlRequest, TerminalRuntimeReplay,
        bounded_log_batches, cleanup_persisted_runtime_state, collect_terminal_runtime_replays,
        is_terminal_runtime_event, rejected_runtime_handle, runtime_session_epoch,
    },
    runtime_events::cleanup_expired_runtime_logs,
    runtime_executor::ManagedProcessExecutor,
    runtime_manager::{RuntimeManagerLimits, RuntimeManagerOptions},
    runtime_metadata::source_mode_from_handle,
    zlm_debug::ZlmDebugExecutor,
    zlm_hook::{ZlmHookRelayRequest, ZlmHookRequestReceiver},
};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const CERTIFICATE_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(30);
const CONTROL_BACKOFF: [u64; 5] = [1, 2, 5, 10, 30];
const CONTROL_BUFFER: usize = 32;
const CONTROL_MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
const LOG_NOTIFICATION_BUFFER: usize = 128;
const ZLM_HOOK_PENDING_MAINTENANCE_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug)]
struct PendingZlmHook {
    request: ZlmHookRelayRequest,
    expires_at: Instant,
}

#[derive(Debug)]
struct PendingZlmHooks {
    entries: HashMap<String, PendingZlmHook>,
    capacity: usize,
    timeout: Duration,
}

impl PendingZlmHooks {
    fn new(capacity: usize, timeout: Duration) -> Self {
        Self {
            entries: HashMap::with_capacity(capacity),
            capacity,
            timeout,
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    fn queue(&mut self, request: ZlmHookRelayRequest, now: Instant) -> Option<RpcZlmHookRequest> {
        if request.response_is_closed() {
            return None;
        }
        if self.entries.len() >= self.capacity || self.entries.contains_key(&request.request_id) {
            let _ = request.respond(
                StatusCode::SERVICE_UNAVAILABLE,
                zlm_hook_local_error_json(
                    "ZLM_HOOK_PENDING_FULL",
                    "too many hooks are awaiting a Core response",
                ),
            );
            return None;
        }
        let rpc = RpcZlmHookRequest {
            request_id: request.request_id.clone(),
            hook_name: request.hook_name.clone(),
            body_json: request.body_json.clone(),
        };
        self.entries.insert(
            request.request_id.clone(),
            PendingZlmHook {
                request,
                expires_at: now + self.timeout,
            },
        );
        Some(rpc)
    }

    fn resolve(&mut self, response: RpcZlmHookResponse) -> bool {
        let Some(pending) = self.entries.remove(&response.request_id) else {
            return false;
        };
        let (status, body) = match u16::try_from(response.http_status)
            .ok()
            .and_then(|status| StatusCode::from_u16(status).ok())
        {
            Some(status) => (status, response.body_json),
            None => (
                StatusCode::BAD_GATEWAY,
                zlm_hook_local_error_json(
                    "ZLM_HOOK_INVALID_CORE_RESPONSE",
                    "Core returned an invalid HTTP status",
                ),
            ),
        };
        let _ = pending.request.respond(status, body);
        true
    }

    fn fail_one(&mut self, request_id: &str) {
        if let Some(pending) = self.entries.remove(request_id) {
            let _ = pending.request.respond(
                StatusCode::SERVICE_UNAVAILABLE,
                zlm_hook_local_error_json(
                    "ZLM_HOOK_CONTROL_DISCONNECTED",
                    "control-plane session disconnected",
                ),
            );
        }
    }

    fn expire(&mut self, now: Instant) {
        let expired = self
            .entries
            .iter()
            .filter(|(_, pending)| {
                pending.expires_at <= now || pending.request.response_is_closed()
            })
            .map(|(request_id, _)| request_id.clone())
            .collect::<Vec<_>>();
        for request_id in expired {
            let Some(pending) = self.entries.remove(&request_id) else {
                continue;
            };
            if !pending.request.response_is_closed() {
                let _ = pending.request.respond(
                    StatusCode::GATEWAY_TIMEOUT,
                    zlm_hook_local_error_json(
                        "ZLM_HOOK_RESPONSE_TIMEOUT",
                        "Core did not answer the hook request in time",
                    ),
                );
            }
        }
    }
}

impl Drop for PendingZlmHooks {
    fn drop(&mut self) {
        for (_, pending) in self.entries.drain() {
            let _ = pending.request.respond(
                StatusCode::SERVICE_UNAVAILABLE,
                zlm_hook_local_error_json(
                    "ZLM_HOOK_CONTROL_DISCONNECTED",
                    "control-plane session disconnected",
                ),
            );
        }
    }
}

fn zlm_hook_local_error_json(code: &str, message: &str) -> String {
    json!({"code": code, "message": message}).to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentControllerExit {
    RestartRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlSessionExit {
    StreamClosed,
    RestartRequired,
}

type InboundControlRead = Result<Option<CoreEnvelope>, Status>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlLane {
    Inbound,
    ZlmHook,
    RuntimePriority,
    RuntimeLog,
}

impl ControlLane {
    const ALL: [Self; 4] = [
        Self::Inbound,
        Self::ZlmHook,
        Self::RuntimePriority,
        Self::RuntimeLog,
    ];

    const fn index(self) -> usize {
        match self {
            Self::Inbound => 0,
            Self::ZlmHook => 1,
            Self::RuntimePriority => 2,
            Self::RuntimeLog => 3,
        }
    }
}

#[derive(Debug)]
enum ControlWork {
    Inbound(InboundControlRead),
    ZlmHook(ZlmHookRelayRequest),
    RuntimePriority(RuntimeNotification),
    RuntimeLog(RuntimeTaskLogBatch),
    LaneClosed(ControlLane),
}

impl ControlWork {
    fn lane(&self) -> ControlLane {
        match self {
            Self::Inbound(_) => ControlLane::Inbound,
            Self::ZlmHook(_) => ControlLane::ZlmHook,
            Self::RuntimePriority(_) => ControlLane::RuntimePriority,
            Self::RuntimeLog(_) => ControlLane::RuntimeLog,
            Self::LaneClosed(lane) => *lane,
        }
    }
}

#[derive(Debug)]
struct ControlWorkArbiter {
    next_lane: usize,
    lane_open: [bool; 4],
}

impl ControlWorkArbiter {
    fn new(zlm_hook_open: bool) -> Self {
        Self {
            next_lane: 0,
            lane_open: [true, zlm_hook_open, true, true],
        }
    }

    fn is_open(&self, lane: ControlLane) -> bool {
        self.lane_open[lane.index()]
    }

    fn close(&mut self, lane: ControlLane) {
        self.lane_open[lane.index()] = false;
    }

    fn mark_selected(&mut self, lane: ControlLane) {
        self.next_lane = (lane.index() + 1) % ControlLane::ALL.len();
    }

    async fn take_ready<S>(
        &mut self,
        inbound: &mut S,
        mut zlm_hooks: Option<&mut ZlmHookRequestReceiver>,
        runtime_priority: &mut mpsc::UnboundedReceiver<RuntimeNotification>,
        runtime_logs: &mut mpsc::Receiver<RuntimeTaskLogBatch>,
    ) -> Option<ControlWork>
    where
        S: Stream<Item = Result<CoreEnvelope, Status>> + Unpin,
    {
        for offset in 0..ControlLane::ALL.len() {
            let lane_index = (self.next_lane + offset) % ControlLane::ALL.len();
            let lane = ControlLane::ALL[lane_index];
            if !self.is_open(lane) {
                continue;
            }

            let work = match lane {
                ControlLane::Inbound => match poll_stream_once(inbound).await {
                    Poll::Ready(Some(read)) => Some(ControlWork::Inbound(read.map(Some))),
                    Poll::Ready(None) => Some(ControlWork::Inbound(Ok(None))),
                    Poll::Pending => None,
                },
                ControlLane::ZlmHook => {
                    let Some(receiver) = zlm_hooks.as_deref_mut() else {
                        self.close(lane);
                        continue;
                    };
                    match receiver.try_recv() {
                        Ok(request) => Some(ControlWork::ZlmHook(request)),
                        Err(mpsc::error::TryRecvError::Empty) => None,
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            self.close(lane);
                            None
                        }
                    }
                }
                ControlLane::RuntimePriority => match runtime_priority.try_recv() {
                    Ok(notification) => Some(ControlWork::RuntimePriority(notification)),
                    Err(mpsc::error::TryRecvError::Empty) => None,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        self.close(lane);
                        None
                    }
                },
                ControlLane::RuntimeLog => match runtime_logs.try_recv() {
                    Ok(batch) => Some(ControlWork::RuntimeLog(batch)),
                    Err(mpsc::error::TryRecvError::Empty) => None,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        self.close(lane);
                        None
                    }
                },
            };
            if let Some(work) = work {
                self.mark_selected(lane);
                return Some(work);
            }
        }
        None
    }
}

async fn poll_stream_once<S>(stream: &mut S) -> Poll<Option<S::Item>>
where
    S: Stream + Unpin,
{
    // The outer poll is always ready and returns the inner readiness verbatim.
    // This polls the tonic stream with the current task Context, registers its
    // real waker on Pending, and performs no read-ahead.
    std::future::poll_fn(|context| Poll::Ready(Pin::new(&mut *stream).poll_next(context))).await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlTimer {
    Heartbeat,
    CertificateMaintenance,
    ZlmHookMaintenance,
}

#[derive(Debug)]
struct ControlTimers {
    heartbeat: Instant,
    certificate_maintenance: Instant,
    zlm_hook_maintenance: Instant,
}

impl ControlTimers {
    fn new(now: Instant) -> Self {
        Self {
            heartbeat: now,
            certificate_maintenance: now + CERTIFICATE_MAINTENANCE_INTERVAL,
            zlm_hook_maintenance: now,
        }
    }

    fn due(&self, now: Instant) -> Option<ControlTimer> {
        if self.heartbeat <= now {
            Some(ControlTimer::Heartbeat)
        } else if self.certificate_maintenance <= now {
            Some(ControlTimer::CertificateMaintenance)
        } else if self.zlm_hook_maintenance <= now {
            Some(ControlTimer::ZlmHookMaintenance)
        } else {
            None
        }
    }

    fn mark_fired(&mut self, timer: ControlTimer, now: Instant) {
        let (deadline, period) = match timer {
            ControlTimer::Heartbeat => (&mut self.heartbeat, HEARTBEAT_INTERVAL),
            ControlTimer::CertificateMaintenance => (
                &mut self.certificate_maintenance,
                CERTIFICATE_MAINTENANCE_INTERVAL,
            ),
            ControlTimer::ZlmHookMaintenance => (
                &mut self.zlm_hook_maintenance,
                ZLM_HOOK_PENDING_MAINTENANCE_INTERVAL,
            ),
        };
        loop {
            *deadline += period;
            if *deadline > now {
                break;
            }
        }
    }

    fn next_deadline(&self) -> Instant {
        self.heartbeat
            .min(self.certificate_maintenance)
            .min(self.zlm_hook_maintenance)
    }
}

#[derive(Debug)]
enum ImmediateControlAction {
    Timer(ControlTimer),
    Work(ControlWork),
}

async fn take_immediate_control_action<S>(
    now: Instant,
    timers: &ControlTimers,
    arbiter: &mut ControlWorkArbiter,
    inbound: &mut S,
    zlm_hooks: Option<&mut ZlmHookRequestReceiver>,
    runtime_priority: &mut mpsc::UnboundedReceiver<RuntimeNotification>,
    runtime_logs: &mut mpsc::Receiver<RuntimeTaskLogBatch>,
) -> Option<ImmediateControlAction>
where
    S: Stream<Item = Result<CoreEnvelope, Status>> + Unpin,
{
    // A due heartbeat is selected before any ready queue. This gives it a hard
    // scheduler bound; an already-running handler or a backpressured outbound
    // send can still delay delivery and is deliberately outside this arbiter.
    if let Some(timer) = timers.due(now) {
        return Some(ImmediateControlAction::Timer(timer));
    }
    arbiter
        .take_ready(inbound, zlm_hooks, runtime_priority, runtime_logs)
        .await
        .map(ImmediateControlAction::Work)
}

#[derive(Clone)]
pub struct AgentController {
    settings: Arc<Settings>,
    node_id: Uuid,
    identity: Option<Arc<LoadedIdentity>>,
    identity_store: Option<AgentIdentityStore>,
    capability_probe: CapabilityProbe,
    runtime_read_handle: RuntimeReadHandle,
    artifact_cleanup: ArtifactCleanupManager,
    runtime_manager: RuntimeManagerHandle,
    runtime_priority_events: Arc<Mutex<mpsc::UnboundedReceiver<RuntimeNotification>>>,
    runtime_log_batches: Arc<Mutex<mpsc::Receiver<RuntimeTaskLogBatch>>>,
    session_epoch: Arc<AtomicU64>,
    zlm_debug_executor: ZlmDebugExecutor,
    zlm_hook_requests: Option<Arc<Mutex<ZlmHookRequestReceiver>>>,
}

impl AgentController {
    #[cfg(test)]
    pub fn new(settings: Settings) -> anyhow::Result<Self> {
        Self::new_inner(settings, None)
    }

    pub(crate) fn new_with_zlm_hook_requests(
        settings: Settings,
        requests: ZlmHookRequestReceiver,
    ) -> anyhow::Result<Self> {
        Self::new_inner(settings, Some(requests))
    }

    fn new_inner(
        settings: Settings,
        zlm_hook_requests: Option<ZlmHookRequestReceiver>,
    ) -> anyhow::Result<Self> {
        let environment = settings.environment_kind()?;
        let identity_path = settings.agent.identity_dir.trim();
        let (identity_store, identity) = if identity_path.is_empty() {
            (None, None)
        } else {
            let store = AgentIdentityStore::new(identity_path);
            let identity = match store.load_current_for_startup(chrono::Utc::now()) {
                Ok(identity) => Some(Arc::new(identity)),
                Err(AgentIdentityLoadError::NotEnrolled)
                    if environment == AgentEnvironment::Development =>
                {
                    None
                }
                Err(error) => {
                    return Err(anyhow::Error::new(error))
                        .context("failed to load enrolled Agent identity");
                }
            };
            (Some(store), identity)
        };
        if environment == AgentEnvironment::Production {
            identity
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("production Agent requires an enrolled identity"))?
                .ensure_production_complete()
                .context("production Agent identity bundle is incomplete")?;
        }
        let node_id = resolve_node_id(
            &settings.environment,
            &settings.agent.node_id,
            identity.as_deref().map(LoadedIdentity::node_id),
        )?;
        let (runtime_priority_tx, runtime_priority_rx) = mpsc::unbounded_channel();
        let (runtime_log_tx, runtime_log_rx) = mpsc::channel(LOG_NOTIFICATION_BUFFER);
        let managed_executor = Arc::new(ManagedProcessExecutor::new_for_manager(
            settings.agent.clone(),
            RuntimeEventSink::new(runtime_priority_tx, runtime_log_tx),
        ));
        let runtime_manager: RuntimeManagerHandle = RuntimeManager::spawn_managed_with_options(
            managed_executor,
            RuntimeManagerOptions {
                limits: RuntimeManagerLimits {
                    start: settings.agent.runtime_manager_start_limit,
                    stop: settings.agent.runtime_manager_stop_limit,
                    recording: settings.agent.runtime_manager_recording_limit,
                    adopt: settings.agent.runtime_manager_adopt_limit,
                },
            },
        );
        let runtime_read_handle = runtime_manager.read_handle();
        let runtime_read_model: Arc<dyn RuntimeReadModel> = Arc::new(runtime_read_handle.clone());
        let artifact_cleanup = ArtifactCleanupManager::with_executor(
            &settings.agent,
            runtime_read_model.clone(),
            Some(runtime_manager.clone()),
        );
        let zlm_debug_executor = ZlmDebugExecutor::new(&settings.agent)?;

        Ok(Self {
            settings: Arc::new(settings),
            node_id,
            identity,
            identity_store,
            capability_probe: CapabilityProbe::new()?,
            runtime_read_handle,
            artifact_cleanup,
            runtime_manager,
            runtime_priority_events: Arc::new(Mutex::new(runtime_priority_rx)),
            runtime_log_batches: Arc::new(Mutex::new(runtime_log_rx)),
            session_epoch: Arc::new(AtomicU64::new(0)),
            zlm_debug_executor,
            zlm_hook_requests: zlm_hook_requests.map(|requests| Arc::new(Mutex::new(requests))),
        })
    }

    pub fn node_id(&self) -> Uuid {
        self.node_id
    }

    pub(crate) fn loaded_identity(&self) -> Option<Arc<LoadedIdentity>> {
        self.identity.clone()
    }

    pub async fn run(self) -> anyhow::Result<AgentControllerExit> {
        let removed_logs = cleanup_expired_runtime_logs(
            &self.settings.agent.work_root,
            self.settings.agent.runtime_log_retention_days,
        );
        if removed_logs > 0 {
            info!(removed_logs, "expired runtime diagnostic logs cleaned");
        }
        self.artifact_cleanup.refresh_now().await;
        self.artifact_cleanup.start_background();
        let mut backoff_idx = 0usize;

        // Agent 到 Core 只有一条长期控制流；断开后退避重连，避免任务控制通道永久丢失。
        loop {
            match self.connect_once().await {
                Ok(ControlSessionExit::StreamClosed) => {
                    warn!("control-plane stream closed, reconnecting");
                    backoff_idx = 0;
                }
                Ok(ControlSessionExit::RestartRequired) => {
                    return Ok(AgentControllerExit::RestartRequired);
                }
                Err(error) => {
                    warn!(error = %error, "control-plane connection failed");
                    backoff_idx = (backoff_idx + 1).min(CONTROL_BACKOFF.len() - 1);
                }
            }

            sleep(Duration::from_secs(CONTROL_BACKOFF[backoff_idx])).await;
        }
    }

    async fn connect_once(&self) -> anyhow::Result<ControlSessionExit> {
        if let Some(reason) = self.artifact_cleanup.control_plane_block_reason() {
            anyhow::bail!("artifact cleanup is not ready for control-plane registration: {reason}");
        }
        // session_epoch 用来丢弃上一条连接残留的日志/事件，防止重连后旧任务消息串入新会话。
        let session_epoch = self.session_epoch.fetch_add(1, Ordering::SeqCst) + 1;
        self.runtime_manager.begin_session(session_epoch).await?;
        let result = self.connect_once_active(session_epoch).await;
        let _ = self.runtime_manager.end_session(session_epoch).await;
        self.invalidate_session_epoch(session_epoch);
        result
    }

    async fn connect_once_active(&self, session_epoch: u64) -> anyhow::Result<ControlSessionExit> {
        let endpoint = build_endpoint(&self.settings.agent, self.identity.as_deref())?;
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
                    registration_to_rpc(
                        &registration,
                        self.settings
                            .agent
                            .management_addr
                            .parse::<std::net::SocketAddr>()?
                            .port(),
                        self.settings.agent.upload_max_bytes,
                    ),
                )),
            },
        )
        .await?;

        let response = client.stream_connect(ReceiverStream::new(receiver)).await?;
        let mut inbound = response.into_inner();

        let mut sent_rotation_request = None;
        let mut sent_activation_ack = None;
        if self
            .maintain_certificate_identity(
                &sender,
                &mut sent_rotation_request,
                &mut sent_activation_ack,
            )
            .await?
        {
            return Ok(ControlSessionExit::RestartRequired);
        }

        // 注册成功后立即上报能力快照，并回放本地持久化的终态运行时，补齐断线窗口事件。
        let snapshot = self.capability_probe.snapshot(&self.settings.agent).await;
        self.runtime_manager.set_zlm_rtmp_enhanced_enabled(
            self.capability_probe
                .zlm_rtmp_enhanced_enabled(&self.settings.agent)
                .await,
        );
        send_capability_snapshot(&sender, &snapshot).await?;
        self.replay_terminal_runtimes(&sender).await?;

        let mut heartbeat_sampler = HeartbeatSampler::new(
            self.settings.agent.work_root.clone(),
            Some(self.artifact_cleanup.clone()),
        );
        let mut pending_zlm_hooks = PendingZlmHooks::new(
            self.settings.agent.zlm_hook_queue_capacity,
            Duration::from_secs(self.settings.agent.zlm_hook_timeout_sec),
        );
        let mut dropped_log_lines = HashMap::new();
        let mut runtime_priority_events = self.runtime_priority_events.lock().await;
        let mut runtime_log_batches = self.runtime_log_batches.lock().await;
        let mut zlm_hook_requests = match self.zlm_hook_requests.as_ref() {
            Some(receiver) => Some(receiver.lock().await),
            None => None,
        };
        let mut work_arbiter = ControlWorkArbiter::new(zlm_hook_requests.is_some());
        let mut timers = ControlTimers::new(Instant::now());

        info!(
            node_id = %registration.node_id,
            node_name = %registration.node_name,
            core_endpoint = %self.settings.agent.core_endpoint,
            "control-plane connected"
        );

        async {
            loop {
                let immediate = take_immediate_control_action(
                    Instant::now(),
                    &timers,
                    &mut work_arbiter,
                    &mut inbound,
                    zlm_hook_requests.as_deref_mut(),
                    &mut runtime_priority_events,
                    &mut runtime_log_batches,
                )
                .await;
                let work = match immediate {
                    Some(ImmediateControlAction::Timer(timer)) => {
                        match timer {
                            ControlTimer::Heartbeat => {
                                self.send_heartbeat(&sender, &mut heartbeat_sampler).await?;
                            }
                            ControlTimer::CertificateMaintenance => {
                                if self
                                    .maintain_certificate_identity(
                                        &sender,
                                        &mut sent_rotation_request,
                                        &mut sent_activation_ack,
                                    )
                                    .await?
                                {
                                    return Ok(ControlSessionExit::RestartRequired);
                                }
                            }
                            ControlTimer::ZlmHookMaintenance => {
                                pending_zlm_hooks.expire(Instant::now());
                            }
                        }
                        timers.mark_fired(timer, Instant::now());
                        continue;
                    }
                    Some(ImmediateControlAction::Work(work)) => work,
                    None => {
                        let timer_deadline = timers.next_deadline();
                        let work = tokio::select! {
                            biased;
                            _ = sleep_until(timer_deadline) => {
                                continue;
                            }
                            read = inbound.next(), if work_arbiter.is_open(ControlLane::Inbound) => {
                                ControlWork::Inbound(match read {
                                    Some(read) => read.map(Some),
                                    None => Ok(None),
                                })
                            }
                            hook_request = recv_optional_zlm_hook(zlm_hook_requests.as_deref_mut()),
                                if work_arbiter.is_open(ControlLane::ZlmHook) =>
                            {
                                match hook_request {
                                    Some(request) => ControlWork::ZlmHook(request),
                                    None => ControlWork::LaneClosed(ControlLane::ZlmHook),
                                }
                            }
                            runtime_notification = runtime_priority_events.recv(),
                                if work_arbiter.is_open(ControlLane::RuntimePriority) =>
                            {
                                match runtime_notification {
                                    Some(notification) => ControlWork::RuntimePriority(notification),
                                    None => ControlWork::LaneClosed(ControlLane::RuntimePriority),
                                }
                            }
                            log_batch = runtime_log_batches.recv(),
                                if work_arbiter.is_open(ControlLane::RuntimeLog) =>
                            {
                                match log_batch {
                                    Some(batch) => ControlWork::RuntimeLog(batch),
                                    None => ControlWork::LaneClosed(ControlLane::RuntimeLog),
                                }
                            }
                        };
                        work_arbiter.mark_selected(work.lane());
                        work
                    }
                };

                match work {
                    ControlWork::Inbound(read) => match read? {
                        Some(message) => {
                            if self
                                .handle_core_envelope(
                                    &sender,
                                    message,
                                    session_epoch,
                                    &mut sent_rotation_request,
                                    &mut sent_activation_ack,
                                    &mut pending_zlm_hooks,
                                )
                                .await?
                                .is_some()
                            {
                                return Ok(ControlSessionExit::RestartRequired);
                            }
                        }
                        None => return Ok(ControlSessionExit::StreamClosed),
                    },
                    ControlWork::ZlmHook(hook_request) => {
                        if let Some(request) = pending_zlm_hooks.queue(hook_request, Instant::now()) {
                            let request_id = request.request_id.clone();
                            if let Err(error) = send_agent_message(
                                &sender,
                                AgentEnvelope {
                                    payload: Some(
                                        media_rpc::control_plane::agent_envelope::Payload::ZlmHookRequest(
                                            request,
                                        ),
                                    ),
                                },
                            )
                            .await
                            {
                                pending_zlm_hooks.fail_one(&request_id);
                                return Err(error);
                            }
                        }
                    }
                    ControlWork::RuntimePriority(runtime_notification) => {
                        self.forward_runtime_notification(
                            &sender,
                            runtime_notification,
                            session_epoch,
                        )
                        .await?;
                    }
                    ControlWork::RuntimeLog(log_batch) => {
                        if self.current_session_epoch() == session_epoch
                            && log_batch.session_epoch == session_epoch
                        {
                            try_send_runtime_log_batch(
                                &sender,
                                log_batch,
                                &mut dropped_log_lines,
                            )?;
                        }
                    }
                    ControlWork::LaneClosed(lane) => work_arbiter.close(lane),
                }
            }
        }
        .await
    }

    async fn maintain_certificate_identity(
        &self,
        sender: &mpsc::Sender<AgentEnvelope>,
        sent_rotation_request: &mut Option<Uuid>,
        sent_activation_ack: &mut Option<Uuid>,
    ) -> anyhow::Result<bool> {
        let (Some(store), Some(identity)) = (&self.identity_store, &self.identity) else {
            return Ok(false);
        };
        match store.on_authenticated_session(identity.generation_id(), chrono::Utc::now())? {
            AuthenticatedRotationAction::None => {}
            AuthenticatedRotationAction::SendRequest(request) => {
                if rotation_message_is_unsent(sent_rotation_request, request.rotation_id()) {
                    send_certificate_rotation_request(sender, &request).await?;
                    *sent_rotation_request = Some(request.rotation_id());
                }
            }
            AuthenticatedRotationAction::RestartRequired => return Ok(true),
        }
        if let Some(activated) = store.replayable_activation_ack(identity.generation_id())? {
            if rotation_message_is_unsent(sent_activation_ack, activated.rotation_id()) {
                send_certificate_rotation_activated(sender, &activated).await?;
                *sent_activation_ack = Some(activated.rotation_id());
            }
        }
        Ok(false)
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
        let runtime_counts = self.runtime_read_handle.state_counts();
        let runtime_slot_loads = build_runtime_slot_loads(
            &self.settings.agent,
            &self.runtime_read_handle.active_handles(),
        );
        let snapshot = sampler.sample(HeartbeatSampleInput {
            running_tasks: runtime_counts.running,
            starting_tasks: runtime_counts.starting,
            stopping_tasks: runtime_counts.stopping,
            orphaned_tasks: runtime_counts.orphaned,
            runtime_slot_loads,
            zlm_alive,
            ffmpeg_alive,
            gpu_runtime: probe_gpu_runtime(&self.settings.agent),
        });

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
                        runtime_slot_loads: snapshot
                            .runtime_slot_loads
                            .iter()
                            .map(|load| RpcRuntimeSlotLoad {
                                source_mode: load.source_mode.as_str().to_string(),
                                max_runtime_slots: load.max_runtime_slots,
                                running_tasks: load.running_tasks,
                                starting_tasks: load.starting_tasks,
                                stopping_tasks: load.stopping_tasks,
                                orphaned_tasks: load.orphaned_tasks,
                                slot_usage: load.slot_usage,
                            })
                            .collect(),
                    }),
                ),
            },
        )
        .await?;

        debug!(
            running_tasks = snapshot.running_tasks,
            runtime_slot_loads = ?snapshot.runtime_slot_loads,
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
        sent_rotation_request: &mut Option<Uuid>,
        sent_activation_ack: &mut Option<Uuid>,
        pending_zlm_hooks: &mut PendingZlmHooks,
    ) -> anyhow::Result<Option<AgentControllerExit>> {
        let Some(payload) = envelope.payload else {
            return Ok(None);
        };

        match payload {
            media_rpc::control_plane::core_envelope::Payload::ProbeCapabilities(
                ProbeCapabilities {},
            ) => {
                let snapshot = self.capability_probe.snapshot(&self.settings.agent).await;
                self.runtime_manager.set_zlm_rtmp_enhanced_enabled(
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
            media_rpc::control_plane::core_envelope::Payload::CertificateRotationBundle(bundle) => {
                let store = self.identity_store.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("certificate rotation requires an enrolled identity store")
                })?;
                let identity = self.identity.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("certificate rotation requires an enrolled identity")
                })?;
                match store.commit_rotation_bundle(
                    identity.generation_id(),
                    &bundle,
                    chrono::Utc::now(),
                )? {
                    RotationCommitOutcome::RestartRequired => {
                        return Ok(Some(AgentControllerExit::RestartRequired));
                    }
                }
            }
            media_rpc::control_plane::core_envelope::Payload::ActivateCertificateRotation(
                command,
            ) => {
                let store = self.identity_store.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("certificate rotation requires an enrolled identity store")
                })?;
                let identity = self.identity.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("certificate rotation requires an enrolled identity")
                })?;
                let activated = store.activate_rotation(
                    identity.generation_id(),
                    &command,
                    chrono::Utc::now(),
                )?;
                send_certificate_rotation_activated(sender, &activated).await?;
                *sent_activation_ack = Some(activated.rotation_id());
            }
            media_rpc::control_plane::core_envelope::Payload::CertificateRotationReset(reset) => {
                ensure_expired_rotation_reset_reason(reset.reason)?;
                let store = self.identity_store.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "certificate rotation reset requires an enrolled identity store"
                    )
                })?;
                store.reset_requested_rotation(&reset.rotation_id)?;
                if self
                    .maintain_certificate_identity(
                        sender,
                        sent_rotation_request,
                        sent_activation_ack,
                    )
                    .await?
                {
                    return Ok(Some(AgentControllerExit::RestartRequired));
                }
            }
            media_rpc::control_plane::core_envelope::Payload::ZlmDebugRequest(request) => {
                let executor = self.zlm_debug_executor.clone();
                let sender = sender.clone();
                tokio::spawn(async move {
                    let response = executor.execute(request).await;
                    let _ = send_agent_message(
                        &sender,
                        AgentEnvelope {
                            payload: Some(
                                media_rpc::control_plane::agent_envelope::Payload::ZlmDebugResponse(
                                    response,
                                ),
                            ),
                        },
                    )
                    .await;
                });
            }
            media_rpc::control_plane::core_envelope::Payload::ZlmHookResponse(response) => {
                if !pending_zlm_hooks.resolve(response) {
                    debug!("ignored late or duplicate ZLM hook response");
                }
            }
        }

        Ok(None)
    }

    async fn replay_terminal_runtimes(
        &self,
        sender: &mpsc::Sender<AgentEnvelope>,
    ) -> anyhow::Result<()> {
        for replay in collect_terminal_runtime_replays(
            &self.settings.agent.work_root,
            &self.runtime_read_handle,
        ) {
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
                self.runtime_manager.observe_runtime_snapshot(handle);
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
        let runtime_manager = self.runtime_manager.clone();

        tokio::spawn(async move {
            let adopted = match runtime_manager
                .adopt_orphans_in_session(
                    session_epoch,
                    AdoptFilter {
                        session_epoch,
                        runtimes: runtimes.clone(),
                    },
                )
                .await
            {
                Ok(RuntimeManagerRequestOutcome::Completed(adopted)) => adopted,
                Ok(RuntimeManagerRequestOutcome::StaleSession) => return,
                Err(error) => {
                    warn!(error = %error, "runtime adopt command failed before execution");
                    return;
                }
            };

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
                let _ = send_task_event(
                    &sender,
                    OutboundTaskEvent::new(
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
                    ),
                )
                .await;
            }

            for handle in adopted {
                if runtime_session_epoch(&handle) != session_epoch {
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
        let runtime_manager = self.runtime_manager.clone();
        let artifact_cleanup = self.artifact_cleanup.clone();

        tokio::spawn(async move {
            if let Err(error) = artifact_cleanup.ensure_task_start_allowed(&request.resolved_spec) {
                match runtime_manager.check_session(request.session_epoch).await {
                    Ok(RuntimeManagerRequestOutcome::Completed(())) => {}
                    Ok(RuntimeManagerRequestOutcome::StaleSession) => return,
                    Err(check_error) => {
                        warn!(error = %check_error, "runtime session check failed");
                    }
                }
                let handle = rejected_runtime_handle(&request);
                let _ = send_task_event(
                    &sender,
                    OutboundTaskEvent::new(
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
                    ),
                )
                .await;
                return;
            }

            match runtime_manager
                .start_task_in_session(request.session_epoch, request.clone())
                .await
            {
                Ok(RuntimeManagerRequestOutcome::Completed(Ok(handle))) => {
                    let _ = send_task_event(
                        &sender,
                        OutboundTaskEvent::new(
                            request.task_id,
                            request.attempt_no,
                            request.lease_token.clone(),
                            "accepted",
                            "info",
                            "task accepted by local executor",
                            json!({
                                "worker_kind": request.task_type.default_worker_kind(),
                            }),
                        ),
                    )
                    .await;
                    let _ = send_task_event(
                        &sender,
                        OutboundTaskEvent::new(
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
                        ),
                    )
                    .await;
                    if runtime_session_epoch(&handle) == request.session_epoch {
                        let _ = send_task_snapshot(&sender, &handle).await;
                    }
                }
                Ok(RuntimeManagerRequestOutcome::Completed(Err(error))) | Err(error) => {
                    let handle = rejected_runtime_handle(&request);
                    let _ = send_task_event(
                        &sender,
                        OutboundTaskEvent::new(
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
                        ),
                    )
                    .await;
                }
                Ok(RuntimeManagerRequestOutcome::StaleSession) => {}
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
        let runtime_manager = self.runtime_manager.clone();
        let runtime_read_handle = self.runtime_read_handle.clone();

        tokio::spawn(async move {
            match runtime_manager
                .stop_task_in_session(session_epoch, request.clone())
                .await
            {
                Ok(RuntimeManagerRequestOutcome::Completed(Ok(()))) => {
                    let _ = send_task_event(
                        &sender,
                        OutboundTaskEvent::new(
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
                        ),
                    )
                    .await;
                    if let Some(handle) = runtime_read_handle
                        .find_by_task_attempt(request.task_id, request.attempt_no)
                    {
                        if runtime_session_epoch(&handle) == session_epoch {
                            let _ = send_task_snapshot(&sender, &handle).await;
                        }
                    }
                }
                Ok(RuntimeManagerRequestOutcome::Completed(Err(error))) | Err(error) => {
                    let _ = send_task_event(
                        &sender,
                        OutboundTaskEvent::new(
                            request.task_id,
                            request.attempt_no,
                            request.lease_token.clone(),
                            "stop_rejected",
                            "error",
                            error.to_string(),
                            json!({
                                "reason": request.reason,
                            }),
                        ),
                    )
                    .await;
                }
                Ok(RuntimeManagerRequestOutcome::StaleSession) => {}
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
        let runtime_manager = self.runtime_manager.clone();

        tokio::spawn(async move {
            match runtime_manager
                .set_task_recording_in_session(session_epoch, request.clone())
                .await
            {
                Ok(RuntimeManagerRequestOutcome::Completed(Ok(handle))) => {
                    if runtime_session_epoch(&handle) == session_epoch {
                        let _ = send_task_snapshot(&sender, &handle).await;
                    }
                }
                Ok(RuntimeManagerRequestOutcome::Completed(Err(error))) | Err(error) => {
                    let _ = send_task_event(
                        &sender,
                        OutboundTaskEvent::new(
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
                        ),
                    )
                    .await;
                }
                Ok(RuntimeManagerRequestOutcome::StaleSession) => {}
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
        self.runtime_manager
            .set_zlm_server_id(zlm_server_id.clone());
        self.runtime_manager.set_zlm_rtmp_enhanced_enabled(
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
        Ok(AgentRegistration {
            node_id: self.node_id,
            node_name: self.settings.agent.node_name.clone(),
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            hostname,
            labels: self.settings.agent.labels.clone(),
            interfaces,
            // Deprecated control endpoints are deliberately blank. Core must use the
            // certificate-bound management listener instead of Agent self-reporting.
            zlm_api_base: String::new(),
            zlm_api_secret: String::new(),
            agent_stream_addr: self.settings.agent.agent_stream_addr.clone(),
            agent_http_base_url: String::new(),
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

fn rotation_message_is_unsent(sent: &Option<Uuid>, rotation_id: Uuid) -> bool {
    *sent != Some(rotation_id)
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
        OutboundTaskEvent {
            task_id: event.task_id,
            attempt_no: event.attempt_no,
            lease_token: event.lease_token,
            event_type: event.event_type,
            event_level: event.event_level,
            message: event.message,
            payload: event.payload,
        },
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

#[derive(Default)]
struct SlotLoadCounts {
    running: u32,
    starting: u32,
    stopping: u32,
    orphaned: u32,
}

fn build_runtime_slot_loads(
    settings: &AgentSettings,
    handles: &[RuntimeHandle],
) -> Vec<RuntimeSlotLoad> {
    let mut live = SlotLoadCounts::default();
    let mut vod = SlotLoadCounts::default();

    for handle in handles {
        let Some(source_mode) = source_mode_from_handle(handle) else {
            continue;
        };
        let target = match source_mode {
            SourceMode::Live => &mut live,
            SourceMode::Vod => &mut vod,
        };
        match handle.state {
            RuntimeState::Pending | RuntimeState::Starting => {
                target.starting = target.starting.saturating_add(1);
            }
            RuntimeState::Running => {
                target.running = target.running.saturating_add(1);
            }
            RuntimeState::Stopping => {
                target.stopping = target.stopping.saturating_add(1);
            }
            RuntimeState::Orphaned => {
                target.orphaned = target.orphaned.saturating_add(1);
            }
            RuntimeState::Exited => {}
        }
    }

    vec![
        runtime_slot_load(SourceMode::Live, settings.max_live_runtime_slots, live),
        runtime_slot_load(SourceMode::Vod, settings.max_vod_runtime_slots, vod),
    ]
}

fn runtime_slot_load(
    source_mode: SourceMode,
    max_runtime_slots: u32,
    counts: SlotLoadCounts,
) -> RuntimeSlotLoad {
    let occupied = counts
        .running
        .saturating_add(counts.starting)
        .saturating_add(counts.stopping)
        .saturating_add(counts.orphaned);
    let slot_usage = if max_runtime_slots == 0 {
        0.0
    } else {
        (occupied as f64 / max_runtime_slots as f64).clamp(0.0, 1.0)
    };

    RuntimeSlotLoad {
        source_mode,
        max_runtime_slots,
        running_tasks: counts.running,
        starting_tasks: counts.starting,
        stopping_tasks: counts.stopping,
        orphaned_tasks: counts.orphaned,
        slot_usage,
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

struct OutboundTaskEvent {
    task_id: Uuid,
    attempt_no: i32,
    lease_token: String,
    event_type: String,
    event_level: String,
    message: String,
    payload: Value,
}

impl OutboundTaskEvent {
    fn new(
        task_id: Uuid,
        attempt_no: i32,
        lease_token: String,
        event_type: &str,
        event_level: &str,
        message: impl Into<String>,
        payload: Value,
    ) -> Self {
        Self {
            task_id,
            attempt_no,
            lease_token,
            event_type: event_type.to_string(),
            event_level: event_level.to_string(),
            message: message.into(),
            payload,
        }
    }
}

async fn send_task_event(
    sender: &mpsc::Sender<AgentEnvelope>,
    event: OutboundTaskEvent,
) -> anyhow::Result<()> {
    send_agent_message(
        sender,
        AgentEnvelope {
            payload: Some(
                media_rpc::control_plane::agent_envelope::Payload::TaskEvent(TaskEvent {
                    task_id: event.task_id.to_string(),
                    attempt_no: event.attempt_no,
                    lease_token: event.lease_token,
                    event_type: event.event_type,
                    event_level: event.event_level,
                    message: event.message,
                    payload_json: serde_json::to_string(&event.payload)?,
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

async fn send_certificate_rotation_request(
    sender: &mpsc::Sender<AgentEnvelope>,
    request: &CertificateRotationRequestData,
) -> anyhow::Result<()> {
    send_agent_message(
        sender,
        AgentEnvelope {
            payload: Some(
                media_rpc::control_plane::agent_envelope::Payload::CertificateRotationRequest(
                    media_rpc::control_plane::CertificateRotationRequest {
                        rotation_id: request.rotation_id().to_string(),
                        control_csr_pem: request.control_csr_pem().to_string(),
                        management_csr_pem: request.management_csr_pem().to_string(),
                    },
                ),
            ),
        },
    )
    .await
}

async fn send_certificate_rotation_activated(
    sender: &mpsc::Sender<AgentEnvelope>,
    activated: &CertificateRotationActivatedData,
) -> anyhow::Result<()> {
    send_agent_message(
        sender,
        AgentEnvelope {
            payload: Some(
                media_rpc::control_plane::agent_envelope::Payload::CertificateRotationActivated(
                    media_rpc::control_plane::CertificateRotationActivated {
                        rotation_id: activated.rotation_id().to_string(),
                        activated_at_ms: activated.activated_at_ms(),
                        control_fingerprint_sha256: activated
                            .control_fingerprint_sha256()
                            .to_string(),
                        management_fingerprint_sha256: activated
                            .management_fingerprint_sha256()
                            .to_string(),
                    },
                ),
            ),
        },
    )
    .await
}

fn ensure_expired_rotation_reset_reason(reason: i32) -> anyhow::Result<()> {
    let reason = media_rpc::control_plane::CertificateRotationResetReason::try_from(reason)
        .map_err(|_| anyhow::anyhow!("unknown certificate rotation reset reason"))?;
    anyhow::ensure!(
        reason == media_rpc::control_plane::CertificateRotationResetReason::Expired,
        "certificate rotation reset reason must be EXPIRED"
    );
    Ok(())
}

fn registration_to_rpc(
    registration: &AgentRegistration,
    management_port: u16,
    management_upload_max_bytes: u64,
) -> RpcRegister {
    RpcRegister {
        node_id: registration.node_id.to_string(),
        node_name: registration.node_name.clone(),
        agent_version: registration.agent_version.clone(),
        hostname: registration.hostname.clone(),
        labels: registration.labels.clone(),
        interfaces: registration.interfaces.clone(),
        agent_stream_addr: registration.agent_stream_addr.clone(),
        zlm_rtmp_port: u32::from(registration.zlm_rtmp_port),
        zlm_rtsp_port: u32::from(registration.zlm_rtsp_port),
        network_mode: registration.network_mode.as_str().to_string(),
        ffmpeg_bin: registration.ffmpeg_bin.clone(),
        ffprobe_bin: registration.ffprobe_bin.clone(),
        zlm_server_id: registration.zlm_server_id.clone(),
        output_mount_relative_prefix_mp4: registration.output_mount_relative_prefix_mp4.clone(),
        output_mount_relative_prefix_hls: registration.output_mount_relative_prefix_hls.clone(),
        management_port: u32::from(management_port),
        management_upload_max_bytes,
        ..RpcRegister::default()
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

fn resolve_node_id(
    environment: &str,
    configured_node_id: &str,
    certificate_node_id: Option<Uuid>,
) -> anyhow::Result<Uuid> {
    let environment = AgentEnvironment::parse(environment)?;
    let configured = (!configured_node_id.trim().is_empty())
        .then(|| Uuid::parse_str(configured_node_id.trim()))
        .transpose()
        .context("AGENT_NODE_ID must be a UUID")?;
    if let Some(configured) = configured {
        anyhow::ensure!(
            !configured.is_nil(),
            "AGENT_NODE_ID must not be the nil UUID"
        );
    }
    if let Some(certificate_node_id) = certificate_node_id {
        if let Some(configured) = configured {
            anyhow::ensure!(
                configured == certificate_node_id,
                "AGENT_NODE_ID does not match the enrolled certificate identity"
            );
        }
        return Ok(certificate_node_id);
    }
    anyhow::ensure!(
        environment == AgentEnvironment::Development,
        "production Agent requires an enrolled certificate identity"
    );
    Ok(configured.unwrap_or_else(Uuid::now_v7))
}

fn build_endpoint(
    settings: &AgentSettings,
    enrolled_identity: Option<&LoadedIdentity>,
) -> anyhow::Result<Endpoint> {
    let mut endpoint = Endpoint::from_shared(settings.core_endpoint.clone())?
        .connect_timeout(Duration::from_secs(5))
        .tcp_keepalive(Some(Duration::from_secs(30)));

    if settings.core_endpoint.starts_with("https://") {
        let mut tls = if let Some(identity) = enrolled_identity {
            ClientTlsConfig::new()
                .ca_certificate(Certificate::from_pem(
                    identity.control_plane_server_ca_pem()?,
                ))
                .identity(identity.tonic_identity())
                .assume_http2(true)
        } else {
            let ca_pem = fs::read(&settings.ca_path)
                .with_context(|| format!("failed to read CA certificate {}", settings.ca_path))?;
            let cert_pem = fs::read(&settings.cert_path).with_context(|| {
                format!("failed to read client certificate {}", settings.cert_path)
            })?;
            let key_pem = fs::read(&settings.key_path)
                .with_context(|| format!("failed to read client key {}", settings.key_path))?;
            ClientTlsConfig::new()
                .ca_certificate(Certificate::from_pem(ca_pem))
                .identity(Identity::from_pem(cert_pem, key_pem))
                .assume_http2(true)
        };
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

async fn recv_optional_zlm_hook(
    receiver: Option<&mut ZlmHookRequestReceiver>,
) -> Option<ZlmHookRelayRequest> {
    let Some(receiver) = receiver else {
        return std::future::pending().await;
    };
    receiver.recv().await
}

#[cfg(test)]
mod tests {
    use std::{pin::Pin, sync::atomic::AtomicBool};

    use media_rpc::control_plane::control_plane_server::{
        ControlPlane as TestControlPlane, ControlPlaneServer,
    };
    use tokio::sync::{oneshot, watch};
    use tokio_stream::Stream;
    use tonic::{Request, Response, Status, Streaming, transport::Server};

    use super::*;

    #[derive(Clone)]
    struct FloodControlPlane {
        observed: mpsc::UnboundedSender<AgentEnvelope>,
        flood_enabled: watch::Receiver<bool>,
        flood_ready: Arc<AtomicBool>,
    }

    #[tonic::async_trait]
    impl TestControlPlane for FloodControlPlane {
        type StreamConnectStream =
            Pin<Box<dyn Stream<Item = Result<CoreEnvelope, Status>> + Send + 'static>>;

        async fn stream_connect(
            &self,
            request: Request<Streaming<AgentEnvelope>>,
        ) -> Result<Response<Self::StreamConnectStream>, Status> {
            let observed = self.observed.clone();
            let mut agent_stream = request.into_inner();
            tokio::spawn(async move {
                while let Ok(Some(envelope)) = agent_stream.message().await {
                    if observed.send(envelope).is_err() {
                        break;
                    }
                }
            });

            let (core_tx, core_rx) = mpsc::channel(4096);
            let mut flood_enabled = self.flood_enabled.clone();
            let flood_ready = self.flood_ready.clone();
            tokio::spawn(async move {
                while !*flood_enabled.borrow() {
                    if flood_enabled.changed().await.is_err() {
                        return;
                    }
                }
                loop {
                    if core_tx
                        .send(Ok(CoreEnvelope { payload: None }))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    flood_ready.store(true, Ordering::SeqCst);
                }
            });

            Ok(Response::new(Box::pin(ReceiverStream::new(core_rx))))
        }
    }

    struct ControlFloodHarness {
        _work_root: tempfile::TempDir,
        controller_task: tokio::task::JoinHandle<anyhow::Result<ControlSessionExit>>,
        server_task: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
        server_shutdown: Option<oneshot::Sender<()>>,
        observed: mpsc::UnboundedReceiver<AgentEnvelope>,
        flood_enabled: watch::Sender<bool>,
        flood_ready: Arc<AtomicBool>,
        runtime_tx: mpsc::UnboundedSender<RuntimeNotification>,
        hook_tx: mpsc::Sender<ZlmHookRelayRequest>,
    }

    impl ControlFloodHarness {
        async fn start() -> Self {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = probe.local_addr().unwrap();
            drop(probe);

            let (observed_tx, observed) = mpsc::unbounded_channel();
            let (flood_enabled, flood_rx) = watch::channel(false);
            let flood_ready = Arc::new(AtomicBool::new(false));
            let service = FloodControlPlane {
                observed: observed_tx,
                flood_enabled: flood_rx,
                flood_ready: flood_ready.clone(),
            };
            let (server_shutdown, shutdown_rx) = oneshot::channel();
            let server_task = tokio::spawn(
                Server::builder()
                    .add_service(ControlPlaneServer::new(service))
                    .serve_with_shutdown(address, async move {
                        let _ = shutdown_rx.await;
                    }),
            );
            tokio::task::yield_now().await;

            let work_root = tempfile::tempdir().unwrap();
            let mut settings = Settings {
                environment: "development".to_string(),
                logging: crate::config::LoggingSettings::default(),
                agent: AgentSettings::default(),
            };
            settings.agent.node_id = Uuid::now_v7().to_string();
            settings.agent.identity_dir = work_root
                .path()
                .join("missing-identity")
                .display()
                .to_string();
            settings.agent.work_root = work_root.path().display().to_string();
            settings.agent.core_endpoint = format!("http://{address}");
            settings.agent.zlm_api_base = "http://127.0.0.1:1".to_string();
            settings.agent.ffmpeg_bin = "/definitely/missing/ffmpeg".to_string();
            settings.agent.ffprobe_bin = "/definitely/missing/ffprobe".to_string();

            let mut controller = AgentController::new(settings).unwrap();
            let (runtime_tx, runtime_rx) = mpsc::unbounded_channel();
            controller.runtime_priority_events = Arc::new(Mutex::new(runtime_rx));
            let (hook_tx, hook_rx) = mpsc::channel(8);
            controller.zlm_hook_requests = Some(Arc::new(Mutex::new(hook_rx)));
            controller.session_epoch.store(1, Ordering::SeqCst);
            let controller_task =
                tokio::spawn(async move { controller.connect_once_active(1).await });

            Self {
                _work_root: work_root,
                controller_task,
                server_task,
                server_shutdown: Some(server_shutdown),
                observed,
                flood_enabled,
                flood_ready,
                runtime_tx,
                hook_tx,
            }
        }

        async fn wait_for_initial_heartbeat(&mut self) {
            let deadline = tokio::time::sleep(Duration::from_secs(60));
            tokio::pin!(deadline);
            loop {
                let envelope = tokio::select! {
                    envelope = self.observed.recv() => envelope
                        .expect("Agent stream closed before its initial heartbeat"),
                    result = &mut self.controller_task => {
                        panic!("Agent controller exited before its initial heartbeat: {result:?}")
                    }
                    result = &mut self.server_task => {
                        panic!("test Core server exited before the initial heartbeat: {result:?}")
                    }
                    _ = &mut deadline => {
                        panic!("Agent did not connect and report its initial heartbeat within 60s")
                    }
                };
                if matches!(
                    envelope.payload,
                    Some(media_rpc::control_plane::agent_envelope::Payload::Heartbeat(_))
                ) {
                    return;
                }
            }
        }

        async fn start_flood(&self) {
            self.flood_enabled.send(true).unwrap();
            while !self.flood_ready.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
            for _ in 0..64 {
                tokio::task::yield_now().await;
            }
        }

        async fn collect_after_flood(&mut self) -> Vec<AgentEnvelope> {
            for _ in 0..2048 {
                tokio::task::yield_now().await;
            }
            let mut envelopes = Vec::new();
            while let Ok(envelope) = self.observed.try_recv() {
                envelopes.push(envelope);
            }
            envelopes
        }
    }

    impl Drop for ControlFloodHarness {
        fn drop(&mut self) {
            self.controller_task.abort();
            if let Some(shutdown) = self.server_shutdown.take() {
                let _ = shutdown.send(());
            }
            self.server_task.abort();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn deterministic_ready_inbound_cannot_starve_due_heartbeat() {
        let (inbound_tx, inbound_rx) = mpsc::channel::<Result<CoreEnvelope, Status>>(64);
        for _ in 0..64 {
            inbound_tx
                .try_send(Ok(CoreEnvelope { payload: None }))
                .unwrap();
        }
        let mut inbound = ReceiverStream::new(inbound_rx);
        let (_hook_tx, mut hook_rx) = mpsc::channel(1);
        let (_runtime_tx, mut runtime_rx) = mpsc::unbounded_channel();
        let (_log_tx, mut log_rx) = mpsc::channel(1);
        let start = Instant::now();
        let mut timers = ControlTimers::new(start);
        let mut arbiter = ControlWorkArbiter::new(true);
        assert_eq!(timers.due(start), Some(ControlTimer::Heartbeat));
        timers.mark_fired(ControlTimer::Heartbeat, start);
        assert_eq!(timers.due(start), Some(ControlTimer::ZlmHookMaintenance));
        timers.mark_fired(ControlTimer::ZlmHookMaintenance, start);

        tokio::time::advance(Duration::from_secs(31)).await;
        let now = Instant::now();
        let action = take_immediate_control_action(
            now,
            &timers,
            &mut arbiter,
            &mut inbound,
            Some(&mut hook_rx),
            &mut runtime_rx,
            &mut log_rx,
        )
        .await;
        assert!(
            matches!(
                action,
                Some(ImmediateControlAction::Timer(ControlTimer::Heartbeat))
            ),
            "a heartbeat overdue across the 30-second lease must preempt ready inbound work"
        );
        timers.mark_fired(ControlTimer::Heartbeat, now);
        assert!(timers.heartbeat <= now + HEARTBEAT_INTERVAL);
    }

    #[tokio::test(start_paused = true)]
    async fn deterministic_ready_inbound_cannot_starve_local_control_sources() {
        let (inbound_tx, inbound_rx) = mpsc::channel::<Result<CoreEnvelope, Status>>(64);
        let hook_response_id = Uuid::now_v7().to_string();
        inbound_tx
            .try_send(Ok(CoreEnvelope {
                payload: Some(
                    media_rpc::control_plane::core_envelope::Payload::ZlmHookResponse(
                        RpcZlmHookResponse {
                            request_id: hook_response_id.clone(),
                            http_status: 200,
                            body_json: r#"{"code":0}"#.to_string(),
                        },
                    ),
                ),
            }))
            .unwrap();
        for _ in 1..64 {
            inbound_tx
                .try_send(Ok(CoreEnvelope { payload: None }))
                .unwrap();
        }
        let mut inbound = ReceiverStream::new(inbound_rx);
        let (hook_tx, mut hook_rx) = mpsc::channel(1);
        let (hook_request, _hook_response) = zlm_hook_request("on_publish");
        hook_tx.send(hook_request).await.unwrap();
        let (runtime_tx, mut runtime_rx) = mpsc::unbounded_channel();
        runtime_tx
            .send(RuntimeNotification::TaskProgress(RuntimeTaskProgress {
                task_id: Uuid::now_v7(),
                attempt_no: 1,
                lease_token: "lease".to_string(),
                session_epoch: 1,
                frame: 1,
                fps: 25.0,
                bitrate_kbps: 1_000.0,
                speed: 1.0,
                out_time_ms: 40,
                dup_frames: 0,
                drop_frames: 0,
            }))
            .unwrap();
        let (log_tx, mut log_rx) = mpsc::channel(1);
        log_tx
            .send(RuntimeTaskLogBatch {
                task_id: Uuid::now_v7(),
                attempt_no: 1,
                lease_token: "lease".to_string(),
                session_epoch: 1,
                stream: "stderr".to_string(),
                lines: vec!["line".to_string()],
                source_line_count: 1,
            })
            .await
            .unwrap();
        let now = Instant::now();
        let mut timers = ControlTimers::new(now);
        timers.mark_fired(ControlTimer::Heartbeat, now);
        timers.mark_fired(ControlTimer::ZlmHookMaintenance, now);
        let mut arbiter = ControlWorkArbiter::new(true);
        let mut lanes = Vec::new();
        let mut hook_response_seen = false;

        for _ in 0..4 {
            let action = take_immediate_control_action(
                now,
                &timers,
                &mut arbiter,
                &mut inbound,
                Some(&mut hook_rx),
                &mut runtime_rx,
                &mut log_rx,
            )
            .await
            .expect("one of the continuously ready lanes must be selected");
            let ImmediateControlAction::Work(work) = action else {
                panic!("no timer is due during the ready-lane sweep");
            };
            if let ControlWork::Inbound(Ok(Some(CoreEnvelope {
                payload:
                    Some(media_rpc::control_plane::core_envelope::Payload::ZlmHookResponse(response)),
            }))) = &work
            {
                hook_response_seen = response.request_id == hook_response_id;
            }
            lanes.push(work.lane());
        }

        assert_eq!(lanes, ControlLane::ALL);
        assert!(
            hook_response_seen,
            "a Core ZLM hook response was starved by continuously ready local sources"
        );
    }

    #[tokio::test]
    async fn inbound_flood_cannot_delay_heartbeat_beyond_its_interval() {
        let mut harness = ControlFloodHarness::start().await;
        harness.wait_for_initial_heartbeat().await;
        tokio::time::pause();
        harness.start_flood().await;

        tokio::time::advance(HEARTBEAT_INTERVAL + Duration::from_millis(1)).await;
        let envelopes = harness.collect_after_flood().await;
        assert!(
            envelopes.iter().any(|envelope| matches!(
                envelope.payload,
                Some(media_rpc::control_plane::agent_envelope::Payload::Heartbeat(_))
            )),
            "a continuously ready Core inbound stream starved the next heartbeat"
        );
    }

    #[tokio::test]
    async fn inbound_flood_cannot_starve_runtime_and_hook_notifications() {
        let mut harness = ControlFloodHarness::start().await;
        harness.wait_for_initial_heartbeat().await;
        tokio::time::pause();
        harness.start_flood().await;

        let progress_task_id = Uuid::now_v7();
        harness
            .runtime_tx
            .send(RuntimeNotification::TaskProgress(RuntimeTaskProgress {
                task_id: progress_task_id,
                attempt_no: 1,
                lease_token: "test-lease".to_string(),
                session_epoch: 1,
                frame: 42,
                fps: 25.0,
                bitrate_kbps: 800.0,
                speed: 1.0,
                out_time_ms: 1_000,
                dup_frames: 0,
                drop_frames: 0,
            }))
            .unwrap();
        let (hook_request, _hook_response) = zlm_hook_request("on_publish");
        let hook_request_id = hook_request.request_id.clone();
        harness.hook_tx.send(hook_request).await.unwrap();

        let envelopes = harness.collect_after_flood().await;
        assert!(
            envelopes.iter().any(|envelope| matches!(
                &envelope.payload,
                Some(media_rpc::control_plane::agent_envelope::Payload::TaskProgress(progress))
                    if progress.task_id == progress_task_id.to_string()
            )),
            "a continuously ready Core inbound stream starved a runtime notification"
        );
        assert!(
            envelopes.iter().any(|envelope| matches!(
                &envelope.payload,
                Some(media_rpc::control_plane::agent_envelope::Payload::ZlmHookRequest(request))
                    if request.request_id == hook_request_id
            )),
            "a continuously ready Core inbound stream starved an Agent ZLM hook request"
        );
    }

    fn zlm_hook_request(
        hook_name: &str,
    ) -> (
        crate::zlm_hook::ZlmHookRelayRequest,
        tokio::sync::oneshot::Receiver<crate::zlm_hook::ZlmHookRelayResponse>,
    ) {
        crate::zlm_hook::ZlmHookRelayRequest::new(
            Uuid::now_v7().to_string(),
            hook_name.to_string(),
            r#"{"app":"live"}"#.to_string(),
        )
    }

    #[tokio::test]
    async fn pending_zlm_hook_response_resolves_exactly_once() {
        let mut pending = PendingZlmHooks::new(2, Duration::from_secs(4));
        let (request, response) = zlm_hook_request("on_publish");
        let request_id = request.request_id.clone();
        let rpc = pending
            .queue(request, Instant::now())
            .expect("hook should enter an empty pending registry");
        assert_eq!(rpc.request_id, request_id);
        assert_eq!(rpc.hook_name, "on_publish");
        assert_eq!(rpc.body_json, r#"{"app":"live"}"#);

        assert!(pending.resolve(media_rpc::control_plane::ZlmHookResponse {
            request_id: request_id.clone(),
            http_status: 202,
            body_json: r#"{"code":0}"#.to_string(),
        }));
        assert!(!pending.resolve(media_rpc::control_plane::ZlmHookResponse {
            request_id,
            http_status: 200,
            body_json: r#"{"code":1}"#.to_string(),
        }));
        let response = response.await.unwrap();
        assert_eq!(response.http_status, axum::http::StatusCode::ACCEPTED);
        assert_eq!(response.body_json, r#"{"code":0}"#);
        assert_eq!(pending.len(), 0);
    }

    #[tokio::test]
    async fn pending_zlm_hooks_are_bounded_and_clean_timeout_or_abort() {
        let mut pending = PendingZlmHooks::new(1, Duration::from_millis(20));
        let now = Instant::now();
        let (first, first_response) = zlm_hook_request("on_publish");
        pending.queue(first, now).unwrap();
        let (overflow, overflow_response) = zlm_hook_request("on_publish");
        assert!(pending.queue(overflow, now).is_none());
        assert_eq!(
            overflow_response.await.unwrap().http_status,
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );

        drop(first_response);
        pending.expire(now);
        assert_eq!(pending.len(), 0, "aborted HTTP waiter was retained");

        let (expiring, expiring_response) = zlm_hook_request("on_publish");
        pending.queue(expiring, now).unwrap();
        pending.expire(now + Duration::from_millis(21));
        assert_eq!(
            expiring_response.await.unwrap().http_status,
            axum::http::StatusCode::GATEWAY_TIMEOUT
        );
        assert_eq!(pending.len(), 0);
    }

    #[tokio::test]
    async fn dropping_a_control_session_drains_pending_zlm_hooks() {
        let (request, response) = zlm_hook_request("on_server_keepalive");
        {
            let mut pending = PendingZlmHooks::new(2, Duration::from_secs(4));
            pending.queue(request, Instant::now()).unwrap();
        }
        assert_eq!(
            response.await.unwrap().http_status,
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn rotation_reset_accepts_only_the_expired_reason() {
        assert!(
            ensure_expired_rotation_reset_reason(
                media_rpc::control_plane::CertificateRotationResetReason::Expired as i32,
            )
            .is_ok()
        );
        assert!(
            ensure_expired_rotation_reset_reason(
                media_rpc::control_plane::CertificateRotationResetReason::Unspecified as i32,
            )
            .is_err()
        );
        assert!(ensure_expired_rotation_reset_reason(i32::MAX).is_err());
    }

    #[test]
    fn healthy_session_sends_each_persisted_rotation_id_only_once() {
        let first = Uuid::now_v7();
        let replacement = Uuid::now_v7();
        let mut sent = None;
        assert!(rotation_message_is_unsent(&sent, first));
        sent = Some(first);
        assert!(!rotation_message_is_unsent(&sent, first));
        assert!(rotation_message_is_unsent(&sent, replacement));
    }

    #[test]
    fn certificate_identity_is_authoritative_for_node_id() {
        let certificate_node = Uuid::now_v7();
        assert_eq!(
            resolve_node_id("production", "", Some(certificate_node)).unwrap(),
            certificate_node
        );
        assert!(
            resolve_node_id(
                "production",
                &Uuid::now_v7().to_string(),
                Some(certificate_node)
            )
            .is_err()
        );
        assert!(resolve_node_id("production", "", None).is_err());
        assert_eq!(
            resolve_node_id("development", &certificate_node.to_string(), None).unwrap(),
            certificate_node
        );
        assert!(resolve_node_id("development", &Uuid::nil().to_string(), None).is_err());
        for invalid in ["prod", "Production", "production ", "staging"] {
            assert!(resolve_node_id(invalid, "", None).is_err());
        }
    }

    #[tokio::test]
    async fn development_without_identity_uses_configured_node_without_creating_store() {
        let parent = tempfile::tempdir().unwrap();
        let identity_dir = parent.path().join("missing-identity");
        let expected_node = Uuid::now_v7();
        let mut settings = Settings {
            environment: "development".to_string(),
            logging: crate::config::LoggingSettings::default(),
            agent: AgentSettings::default(),
        };
        settings.agent.node_id = expected_node.to_string();
        settings.agent.identity_dir = identity_dir.display().to_string();

        let controller = AgentController::new(settings).unwrap();
        assert_eq!(controller.node_id(), expected_node);
        assert!(!identity_dir.exists());
    }

    #[test]
    fn registration_advertises_only_the_certificate_bound_management_listener() {
        let node_id = Uuid::now_v7();
        let registration = AgentRegistration {
            node_id,
            node_name: "agent-a".to_string(),
            agent_version: "test".to_string(),
            hostname: "agent-a.test".to_string(),
            labels: Vec::new(),
            interfaces: Vec::new(),
            zlm_api_base: "must-not-be-sent".to_string(),
            zlm_api_secret: "must-not-be-sent".to_string(),
            agent_stream_addr: String::new(),
            agent_http_base_url: "must-not-be-sent".to_string(),
            zlm_rtmp_port: 1935,
            zlm_rtsp_port: 554,
            network_mode: NetworkMode::Host,
            ffmpeg_bin: "ffmpeg".to_string(),
            ffprobe_bin: "ffprobe".to_string(),
            zlm_server_id: node_id.to_string(),
            output_mount_relative_prefix_mp4: "output/mp4".to_string(),
            output_mount_relative_prefix_hls: "output/hls".to_string(),
        };

        let rpc = registration_to_rpc(&registration, 9443, 64 * 1024 * 1024);
        assert_eq!(rpc.management_port, 9443);
        assert_eq!(rpc.management_upload_max_bytes, 64 * 1024 * 1024);
        #[allow(deprecated)]
        {
            assert!(rpc.zlm_api_base.is_empty());
            assert!(rpc.zlm_api_secret.is_empty());
            assert!(rpc.agent_http_base_url.is_empty());
        }
    }

    #[tokio::test]
    async fn zlm_debug_failure_returns_typed_response_without_closing_session() {
        let parent = tempfile::tempdir().unwrap();
        let node_id = Uuid::now_v7();
        let mut settings = Settings {
            environment: "development".to_string(),
            logging: crate::config::LoggingSettings::default(),
            agent: AgentSettings::default(),
        };
        settings.agent.node_id = node_id.to_string();
        settings.agent.identity_dir = parent.path().join("missing-identity").display().to_string();
        settings.agent.zlm_api_base = "http://127.0.0.1:1".to_string();

        let controller = AgentController::new(settings).unwrap();
        let (sender, mut receiver) = mpsc::channel(2);
        let request_id = Uuid::now_v7();
        let mut sent_rotation_request = None;
        let mut sent_activation_ack = None;
        let mut pending_zlm_hooks = PendingZlmHooks::new(2, Duration::from_secs(4));

        let outcome = controller
            .handle_core_envelope(
                &sender,
                CoreEnvelope {
                    payload: Some(
                        media_rpc::control_plane::core_envelope::Payload::ZlmDebugRequest(
                            media_rpc::control_plane::ZlmDebugRequest {
                                request_id: request_id.to_string(),
                                operation: media_rpc::control_plane::ZlmDebugOperation::GetStatistic
                                    as i32,
                                parameters: None,
                            },
                        ),
                    ),
                },
                1,
                &mut sent_rotation_request,
                &mut sent_activation_ack,
                &mut pending_zlm_hooks,
            )
            .await
            .unwrap();

        assert_eq!(outcome, None);
        let response = tokio::time::timeout(Duration::from_secs(3), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        let response = match response.payload.unwrap() {
            media_rpc::control_plane::agent_envelope::Payload::ZlmDebugResponse(response) => {
                response
            }
            other => panic!("unexpected Agent response: {other:?}"),
        };
        assert_eq!(response.request_id, request_id.to_string());
        assert_eq!(
            response.operation,
            media_rpc::control_plane::ZlmDebugOperation::GetStatistic as i32
        );
        assert_eq!(
            response.status,
            media_rpc::control_plane::ZlmDebugResponseStatus::Failed as i32
        );
        assert!(matches!(
            response.payload,
            Some(media_rpc::control_plane::zlm_debug_response::Payload::Error(_))
        ));
    }

    #[tokio::test]
    async fn zlm_debug_executes_fixed_local_operation_with_agent_held_secret() {
        async fn statistic(
            axum::extract::Query(query): axum::extract::Query<HashMap<String, String>>,
        ) -> axum::Json<Value> {
            assert_eq!(query.get("secret").map(String::as_str), Some("local-only"));
            axum::Json(json!({"code": 0, "data": {"alive": true}}))
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route("/index/api/getStatistic", axum::routing::get(statistic)),
            )
            .await
            .unwrap();
        });

        let parent = tempfile::tempdir().unwrap();
        let node_id = Uuid::now_v7();
        let mut settings = Settings {
            environment: "development".to_string(),
            logging: crate::config::LoggingSettings::default(),
            agent: AgentSettings::default(),
        };
        settings.agent.node_id = node_id.to_string();
        settings.agent.identity_dir = parent.path().join("missing-identity").display().to_string();
        settings.agent.zlm_api_base = format!("http://{address}");
        settings.agent.zlm_api_secret = "local-only".to_string();

        let controller = AgentController::new(settings).unwrap();
        let (sender, mut receiver) = mpsc::channel(2);
        let request_id = Uuid::now_v7();
        let mut sent_rotation_request = None;
        let mut sent_activation_ack = None;
        let mut pending_zlm_hooks = PendingZlmHooks::new(2, Duration::from_secs(4));
        let outcome = controller
            .handle_core_envelope(
                &sender,
                CoreEnvelope {
                    payload: Some(
                        media_rpc::control_plane::core_envelope::Payload::ZlmDebugRequest(
                            media_rpc::control_plane::ZlmDebugRequest {
                                request_id: request_id.to_string(),
                                operation: media_rpc::control_plane::ZlmDebugOperation::GetStatistic
                                    as i32,
                                parameters: None,
                            },
                        ),
                    ),
                },
                1,
                &mut sent_rotation_request,
                &mut sent_activation_ack,
                &mut pending_zlm_hooks,
            )
            .await
            .unwrap();

        assert_eq!(outcome, None);
        let response = tokio::time::timeout(Duration::from_secs(3), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        let response = match response.payload.unwrap() {
            media_rpc::control_plane::agent_envelope::Payload::ZlmDebugResponse(response) => {
                response
            }
            other => panic!("unexpected Agent response: {other:?}"),
        };
        assert_eq!(response.request_id, request_id.to_string());
        assert_eq!(
            response.status,
            media_rpc::control_plane::ZlmDebugResponseStatus::Succeeded as i32
        );
        let json_payload = match response.payload.unwrap() {
            media_rpc::control_plane::zlm_debug_response::Payload::JsonPayload(value) => value,
            other => panic!("unexpected ZLM payload: {other:?}"),
        };
        assert_eq!(
            serde_json::from_str::<Value>(&json_payload).unwrap(),
            json!({"code": 0, "data": {"alive": true}})
        );

        server.abort();
    }

    #[tokio::test]
    async fn zlm_debug_execution_does_not_block_the_control_stream() {
        async fn slow_statistic() -> axum::Json<Value> {
            tokio::time::sleep(Duration::from_millis(200)).await;
            axum::Json(json!({"code": 0}))
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route(
                    "/index/api/getStatistic",
                    axum::routing::get(slow_statistic),
                ),
            )
            .await
            .unwrap();
        });
        let parent = tempfile::tempdir().unwrap();
        let mut settings = Settings {
            environment: "development".to_string(),
            logging: crate::config::LoggingSettings::default(),
            agent: AgentSettings::default(),
        };
        settings.agent.node_id = Uuid::now_v7().to_string();
        settings.agent.identity_dir = parent.path().join("missing-identity").display().to_string();
        settings.agent.zlm_api_base = format!("http://{address}");
        let controller = AgentController::new(settings).unwrap();
        let (sender, mut receiver) = mpsc::channel(2);
        let mut sent_rotation_request = None;
        let mut sent_activation_ack = None;
        let mut pending_zlm_hooks = PendingZlmHooks::new(2, Duration::from_secs(4));

        let outcome = tokio::time::timeout(
            Duration::from_millis(50),
            controller.handle_core_envelope(
                &sender,
                CoreEnvelope {
                    payload: Some(
                        media_rpc::control_plane::core_envelope::Payload::ZlmDebugRequest(
                            media_rpc::control_plane::ZlmDebugRequest {
                                request_id: Uuid::now_v7().to_string(),
                                operation: media_rpc::control_plane::ZlmDebugOperation::GetStatistic
                                    as i32,
                                parameters: None,
                            },
                        ),
                    ),
                },
                1,
                &mut sent_rotation_request,
                &mut sent_activation_ack,
                &mut pending_zlm_hooks,
            ),
        )
        .await
        .expect("ZLM execution must be detached from the inbound control loop")
        .unwrap();
        assert_eq!(outcome, None);
        let response = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            response.payload,
            Some(media_rpc::control_plane::agent_envelope::Payload::ZlmDebugResponse(_))
        ));

        server.abort();
    }
}
