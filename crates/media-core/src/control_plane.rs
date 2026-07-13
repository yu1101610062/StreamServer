// tonic::Status intentionally carries the complete wire error contract through
// the parsing boundary; boxing every small helper error would only add churn at
// each call site without reducing the service response allocation.
#![allow(clippy::result_large_err)]

#[cfg(test)]
#[path = "tests/control_plane.rs"]
mod tests;

use std::{
    cmp::Ordering as CmpOrdering,
    collections::{HashMap, VecDeque},
    future::Future,
    net::IpAddr,
    pin::Pin,
    sync::{
        Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
    },
    time::Duration,
};

use chrono::{DateTime, Utc};
use media_domain::{
    AgentRegistration, CapabilitySnapshot, GpuDeviceInfo, GpuRuntimeStats, HeartbeatSnapshot,
    InputKind, NetworkMode, RuntimeSlotLoad, SourceMode, TaskSpec, TaskType,
    normalize_output_mount_relative_prefix,
};
use media_rpc::control_plane::{
    ActivateCertificateRotation, AdoptOrphans, AgentEnvelope,
    CapabilitySnapshot as RpcCapabilitySnapshot, CertificateRotationBundle,
    CertificateRotationRequest as RpcCertificateRotationRequest, CertificateRotationReset,
    CertificateRotationResetReason, CoreEnvelope, GpuDevice as RpcGpuDevice,
    GpuRuntime as RpcGpuRuntime, Heartbeat as RpcHeartbeat, ProbeCapabilities, ReclaimRuntime,
    Register as RpcRegister, RuntimeSlotLoad as RpcRuntimeSlotLoad, TaskEvent, TaskLogBatch,
    TaskProgress, TaskRecordingControl, TaskSnapshot, ZlmCloseStreamParameters, ZlmDebugOperation,
    ZlmDebugRequest, ZlmDebugResponse, ZlmDebugResponseStatus, ZlmHookRequest as RpcZlmHookRequest,
    ZlmHookResponse, ZlmKickSessionParameters, ZlmKickSessionsParameters, ZlmMediaFilter,
    ZlmSnapshotParameters,
    control_plane_server::{ControlPlane, ControlPlaneServer},
};
use reqwest::Url;
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    time::{sleep, timeout},
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::agent_identity::{
    AgentIdentityService, AgentIdentityServiceError, AgentPeerCertificateError,
    AuthenticatedAgentPeer, CompletedAgentCertificateRotation,
    agent_certificate_fingerprint_sha256, parse_authenticated_agent_peer,
};
use crate::agent_management::{
    AgentManagementCertificatePins, AgentManagementError, AgentManagementFuture,
    AgentManagementReadinessProbe, AgentManagementSessionFence, AgentManagementTargetProvider,
    AuthenticatedAgentManagementTarget,
};
use crate::repository::{
    AgentCertificateRotationAcknowledgement, AgentCertificateRotationTakeoverContext,
    AgentControlSessionClaim, AgentControlSessionClaimOutcome,
    AgentManagementRotationActivationOutcome, AgentManagementRotationActivationRequest,
    AgentSessionWriteOutcome, AgentTaskEventRecord, CompleteAgentCertificateRotationOutcome,
    RecordingControlCommand, RepoError, TaskLogBatchRecord, TaskProgressRecord, TaskRepository,
    TaskSnapshotRecord,
};
use crate::source_gateway::SourceGatewayClient;

const CONTROL_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const CONTROL_STREAM_BUFFER: usize = 32;
const CONTROL_MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_ROTATION_CSR_BYTES: usize = 64 * 1024;
const MAX_PENDING_ZLM_PER_SESSION: usize = 4;
const MAX_PENDING_ZLM_GLOBAL: usize = 256;
const MAX_ZLM_JSON_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_ZLM_SNAPSHOT_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_ZLM_HOOK_BODY_BYTES: usize = 256 * 1024;
const MAX_ACTIVE_ZLM_HOOKS_PER_SESSION: usize = 4;
const MAX_ACTIVE_ZLM_HOOKS_GLOBAL: usize = 256;
// A fenced hook holds one pool connection while its business handler may need
// another. The Core pool has ten connections in production and five in the
// database test harness, so four preserves forward progress without weakening
// the outer 256-request admission bound.
const MAX_EXECUTING_ZLM_HOOK_FENCES_GLOBAL: usize = 4;
const MAX_COMPLETED_ZLM_HOOKS_PER_SESSION: usize = 128;
const ZLM_HOOK_HANDLER_TIMEOUT: Duration = Duration::from_secs(3);
const ZLM_DEBUG_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const CERTIFICATE_ROTATION_ACTIVATION_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const CERTIFICATE_ROTATION_MANAGEMENT_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone)]
pub struct ControlPlaneService {
    repository: Arc<TaskRepository>,
    source_gateway: Option<SourceGatewayClient>,
    sessions: Arc<Mutex<HashMap<Uuid, SessionHandle>>>,
    core_instance_id: Uuid,
    agent_identity: Option<AgentIdentityService>,
    management_readiness: Arc<dyn AgentManagementReadinessProbe>,
    pending_rotations: Arc<Mutex<HashMap<(Uuid, Uuid), PendingSessionRotation>>>,
    rotation_activation_retry_interval: Duration,
    pending_zlm: Arc<StdMutex<PendingZlmWaiters>>,
    zlm_request_timeout: Duration,
    zlm_hook_handler: Arc<dyn ZlmHookHandler>,
    zlm_hook_sessions: Arc<Mutex<HashMap<(Uuid, Uuid), ZlmHookSessionState>>>,
    zlm_hook_global_admission: Arc<Semaphore>,
    zlm_hook_durable_fence_admission: Arc<Semaphore>,
}

#[derive(Debug)]
struct SessionHandle {
    session_id: Uuid,
    generation: Arc<SessionGeneration>,
    sender: mpsc::Sender<Result<CoreEnvelope, Status>>,
    registration: AgentRegistration,
    identity: SessionIdentityState,
    capabilities: SessionCapabilities,
    load: SessionLoad,
    reservations: VecDeque<DispatchReservation>,
    management_port: u16,
    management_upload_max_bytes: u64,
}

#[derive(Debug, Default)]
struct SessionGeneration {
    canceled: AtomicBool,
    canceled_notify: Notify,
}

impl SessionGeneration {
    fn cancel(&self) {
        if !self.canceled.swap(true, AtomicOrdering::AcqRel) {
            self.canceled_notify.notify_waiters();
        }
    }

    fn is_canceled(&self) -> bool {
        self.canceled.load(AtomicOrdering::Acquire)
    }

    async fn canceled(&self) {
        loop {
            let notified = self.canceled_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_canceled() {
                return;
            }
            notified.await;
        }
    }
}

#[derive(Debug, Clone)]
struct PendingSessionRotation {
    context: AgentCertificateRotationTakeoverContext,
    generation: Arc<SessionGeneration>,
}

#[derive(Debug)]
struct PendingZlmWaiter {
    operation: ZlmDebugOperation,
    expects_snapshot: bool,
    sender: oneshot::Sender<Result<ZlmDebugResult, ZlmDebugCallError>>,
}

type PendingZlmKey = (Uuid, Uuid, Uuid);
type PendingZlmWaiters = HashMap<PendingZlmKey, PendingZlmWaiter>;

#[derive(Debug)]
struct PendingZlmRegistrationGuard {
    pending: Arc<StdMutex<PendingZlmWaiters>>,
    key: PendingZlmKey,
}

impl PendingZlmRegistrationGuard {
    fn new(pending: Arc<StdMutex<PendingZlmWaiters>>, key: PendingZlmKey) -> Self {
        Self { pending, key }
    }
}

impl Drop for PendingZlmRegistrationGuard {
    fn drop(&mut self) {
        lock_pending_zlm(&self.pending).remove(&self.key);
    }
}

fn lock_pending_zlm(pending: &StdMutex<PendingZlmWaiters>) -> StdMutexGuard<'_, PendingZlmWaiters> {
    pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ZlmDebugCommand {
    ListMedia {
        schema: Option<String>,
        vhost: Option<String>,
        app: Option<String>,
        stream: Option<String>,
    },
    ListSessions,
    ListPlayers,
    GetStatistic,
    GetThreadsLoad,
    GetWorkThreadsLoad,
    KickSession {
        session_id: String,
    },
    KickSessions {
        local_port: Option<u16>,
        peer_ip: Option<String>,
    },
    CloseStream {
        schema: String,
        vhost: String,
        app: String,
        stream: String,
        force: bool,
    },
    Snapshot {
        source_url: String,
        timeout_sec: u32,
        expire_sec: u32,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ZlmDebugResult {
    Json(Value),
    Snapshot { content_type: String, data: Vec<u8> },
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub(crate) enum ZlmDebugCallError {
    #[error("Agent is not connected")]
    Disconnected,
    #[error("too many Agent debug requests are pending")]
    Busy,
    #[error("Agent debug request timed out")]
    DeadlineExceeded,
    #[error("Agent debug request is invalid")]
    InvalidRequest,
    #[error("Agent debug response violated the protocol")]
    ProtocolViolation,
    #[error("Agent debug response exceeded the size limit")]
    ResponseTooLarge,
    #[error("Agent debug operation failed: {code}: {message}")]
    Remote { code: String, message: String },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AuthenticatedZlmHook {
    pub(crate) node_id: Uuid,
    pub(crate) hook_name: String,
    pub(crate) body: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ZlmHookHandlerResponse {
    pub(crate) http_status: u16,
    pub(crate) body: Value,
}

pub(crate) type ZlmHookFuture<'a> =
    Pin<Box<dyn Future<Output = ZlmHookHandlerResponse> + Send + 'a>>;

pub(crate) trait ZlmHookHandler: Send + Sync + std::fmt::Debug {
    fn handle(&self, request: AuthenticatedZlmHook) -> ZlmHookFuture<'_>;
}

#[derive(Debug, Clone, PartialEq)]
struct ParsedZlmHookRequest {
    request_id: Uuid,
    hook_name: String,
    body: Value,
    fingerprint: [u8; 32],
}

#[derive(Debug, Clone)]
enum ZlmHookRequestState {
    Pending {
        fingerprint: [u8; 32],
    },
    Completed {
        fingerprint: [u8; 32],
        response: ZlmHookHandlerResponse,
    },
}

#[derive(Debug, Default)]
struct ZlmHookSessionState {
    active: usize,
    requests: HashMap<Uuid, ZlmHookRequestState>,
    completed_order: VecDeque<Uuid>,
}

#[derive(Debug)]
enum ZlmHookAdmission {
    Start(OwnedSemaphorePermit),
    PendingDuplicate,
    Replay(ZlmHookHandlerResponse),
    Conflict,
    Busy,
    Fenced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ZlmHookRequestError {
    InvalidRequestId,
    InvalidHookName,
    InvalidBody,
    BodyTooLarge,
    ForbiddenBodyField,
}

fn parse_zlm_hook_request(
    request: RpcZlmHookRequest,
) -> Result<ParsedZlmHookRequest, ZlmHookRequestError> {
    let request_id = Uuid::parse_str(&request.request_id)
        .ok()
        .filter(|value| !value.is_nil() && value.to_string() == request.request_id)
        .ok_or(ZlmHookRequestError::InvalidRequestId)?;
    if !matches!(
        request.hook_name.as_str(),
        "on_publish"
            | "on_rtp_server_timeout"
            | "on_record_mp4"
            | "on_record_ts"
            | "on_record_hls"
            | "on_stream_none_reader"
            | "on_stream_not_found"
            | "on_server_keepalive"
            | "on_server_started"
    ) {
        return Err(ZlmHookRequestError::InvalidHookName);
    }
    if request.body_json.len() > MAX_ZLM_HOOK_BODY_BYTES {
        return Err(ZlmHookRequestError::BodyTooLarge);
    }
    let body = serde_json::from_str::<Value>(&request.body_json)
        .map_err(|_| ZlmHookRequestError::InvalidBody)?;
    if !body.is_object() {
        return Err(ZlmHookRequestError::InvalidBody);
    }
    if zlm_hook_body_has_forbidden_field(&body) {
        return Err(ZlmHookRequestError::ForbiddenBodyField);
    }
    if request.hook_name == "on_server_started"
        && body.as_object().is_some_and(|fields| !fields.is_empty())
    {
        return Err(ZlmHookRequestError::ForbiddenBodyField);
    }
    let mut fingerprint = Sha256::new();
    fingerprint.update(request.hook_name.as_bytes());
    fingerprint.update([0]);
    fingerprint.update(request.body_json.as_bytes());
    Ok(ParsedZlmHookRequest {
        request_id,
        hook_name: request.hook_name,
        body,
        fingerprint: fingerprint.finalize().into(),
    })
}

fn zlm_hook_body_has_forbidden_field(value: &Value) -> bool {
    match value {
        Value::Object(fields) => fields.iter().any(|(key, value)| {
            let lower = key.to_ascii_lowercase();
            let compact = lower.replace(['_', '-'], "");
            lower.contains("secret")
                || lower.contains('.')
                || matches!(compact.as_str(), "serverid" | "mediaserverid")
                || zlm_hook_body_has_forbidden_field(value)
        }),
        Value::Array(values) => values.iter().any(zlm_hook_body_has_forbidden_field),
        _ => false,
    }
}

fn safe_zlm_hook_response_request_id(value: &str) -> String {
    Uuid::parse_str(value)
        .ok()
        .filter(|request_id| !request_id.is_nil() && request_id.to_string() == value)
        .map(|request_id| request_id.to_string())
        .unwrap_or_default()
}

fn normalize_zlm_hook_response(response: ZlmHookHandlerResponse) -> ZlmHookHandlerResponse {
    if !(200..=599).contains(&response.http_status)
        || serde_json::to_vec(&response.body)
            .map(|body| body.len() > MAX_ZLM_HOOK_BODY_BYTES)
            .unwrap_or(true)
    {
        zlm_hook_error_response(502, "invalid ZLM hook processor response")
    } else {
        response
    }
}

fn zlm_hook_error_response(http_status: u16, message: &'static str) -> ZlmHookHandlerResponse {
    ZlmHookHandlerResponse {
        http_status,
        body: serde_json::json!({
            "code": -1,
            "msg": message,
        }),
    }
}

#[derive(Debug)]
struct RejectingZlmHookHandler;

impl ZlmHookHandler for RejectingZlmHookHandler {
    fn handle(&self, _request: AuthenticatedZlmHook) -> ZlmHookFuture<'_> {
        Box::pin(async {
            ZlmHookHandlerResponse {
                http_status: 503,
                body: serde_json::json!({
                    "code": -1,
                    "msg": "ZLM hook processing is unavailable",
                }),
            }
        })
    }
}

impl ZlmDebugCommand {
    fn into_rpc(
        self,
        request_id: Uuid,
    ) -> Result<(ZlmDebugRequest, ZlmDebugOperation, bool), ZlmDebugCallError> {
        use media_rpc::control_plane::zlm_debug_request::Parameters;

        let (operation, parameters, expects_snapshot) = match self {
            Self::ListMedia {
                schema,
                vhost,
                app,
                stream,
            } => {
                for value in [&schema, &vhost, &app, &stream].into_iter().flatten() {
                    validate_zlm_parameter(value, 512)?;
                }
                (
                    ZlmDebugOperation::ListMedia,
                    Some(Parameters::MediaFilter(ZlmMediaFilter {
                        schema: schema.unwrap_or_default(),
                        vhost: vhost.unwrap_or_default(),
                        app: app.unwrap_or_default(),
                        stream: stream.unwrap_or_default(),
                    })),
                    false,
                )
            }
            Self::ListSessions => (ZlmDebugOperation::ListSessions, None, false),
            Self::ListPlayers => (ZlmDebugOperation::ListPlayers, None, false),
            Self::GetStatistic => (ZlmDebugOperation::GetStatistic, None, false),
            Self::GetThreadsLoad => (ZlmDebugOperation::GetThreadsLoad, None, false),
            Self::GetWorkThreadsLoad => (ZlmDebugOperation::GetWorkThreadsLoad, None, false),
            Self::KickSession { session_id } => {
                validate_zlm_parameter(&session_id, 512)?;
                (
                    ZlmDebugOperation::KickSession,
                    Some(Parameters::KickSession(ZlmKickSessionParameters {
                        session_id,
                    })),
                    false,
                )
            }
            Self::KickSessions {
                local_port,
                peer_ip,
            } => {
                if let Some(peer_ip) = &peer_ip {
                    validate_zlm_parameter(peer_ip, 128)?;
                    peer_ip
                        .parse::<IpAddr>()
                        .map_err(|_| ZlmDebugCallError::InvalidRequest)?;
                }
                (
                    ZlmDebugOperation::KickSessions,
                    Some(Parameters::KickSessions(ZlmKickSessionsParameters {
                        local_port: local_port.map(u32::from).unwrap_or_default(),
                        peer_ip: peer_ip.unwrap_or_default(),
                    })),
                    false,
                )
            }
            Self::CloseStream {
                schema,
                vhost,
                app,
                stream,
                force,
            } => {
                for value in [&schema, &vhost, &app, &stream] {
                    validate_zlm_parameter(value, 512)?;
                }
                (
                    ZlmDebugOperation::CloseStream,
                    Some(Parameters::CloseStream(ZlmCloseStreamParameters {
                        schema,
                        vhost,
                        app,
                        stream,
                        force,
                    })),
                    false,
                )
            }
            Self::Snapshot {
                source_url,
                timeout_sec,
                expire_sec,
            } => {
                validate_zlm_parameter(&source_url, 8192)?;
                if timeout_sec == 0 || timeout_sec > 300 || expire_sec == 0 || expire_sec > 3600 {
                    return Err(ZlmDebugCallError::InvalidRequest);
                }
                (
                    ZlmDebugOperation::Snapshot,
                    Some(Parameters::Snapshot(ZlmSnapshotParameters {
                        source_url,
                        timeout_sec,
                        expire_sec,
                    })),
                    true,
                )
            }
        };
        Ok((
            ZlmDebugRequest {
                request_id: request_id.to_string(),
                operation: operation as i32,
                parameters,
            },
            operation,
            expects_snapshot,
        ))
    }
}

fn validate_zlm_parameter(value: &str, max_bytes: usize) -> Result<(), ZlmDebugCallError> {
    if value.is_empty()
        || value.len() > max_bytes
        || value.contains(['\0', '\r', '\n'])
        || value != value.trim()
    {
        return Err(ZlmDebugCallError::InvalidRequest);
    }
    Ok(())
}

fn parse_zlm_debug_response(
    response: ZlmDebugResponse,
    expects_snapshot: bool,
) -> Result<ZlmDebugResult, ZlmDebugCallError> {
    use media_rpc::control_plane::zlm_debug_response::Payload;

    if response.truncated {
        return Err(ZlmDebugCallError::ResponseTooLarge);
    }
    let status = ZlmDebugResponseStatus::try_from(response.status)
        .map_err(|_| ZlmDebugCallError::ProtocolViolation)?;
    match status {
        ZlmDebugResponseStatus::Succeeded => match response.payload {
            Some(Payload::JsonPayload(json_payload)) if !expects_snapshot => {
                if json_payload.len() > MAX_ZLM_JSON_RESPONSE_BYTES {
                    return Err(ZlmDebugCallError::ResponseTooLarge);
                }
                serde_json::from_str(&json_payload)
                    .map(ZlmDebugResult::Json)
                    .map_err(|_| ZlmDebugCallError::ProtocolViolation)
            }
            Some(Payload::Snapshot(snapshot)) if expects_snapshot => {
                if snapshot.data.len() > MAX_ZLM_SNAPSHOT_RESPONSE_BYTES {
                    return Err(ZlmDebugCallError::ResponseTooLarge);
                }
                if snapshot.content_type.is_empty()
                    || snapshot.content_type.len() > 128
                    || !snapshot.content_type.starts_with("image/")
                    || snapshot.content_type.contains(['\r', '\n', '\0'])
                {
                    return Err(ZlmDebugCallError::ProtocolViolation);
                }
                Ok(ZlmDebugResult::Snapshot {
                    content_type: snapshot.content_type,
                    data: snapshot.data,
                })
            }
            _ => Err(ZlmDebugCallError::ProtocolViolation),
        },
        ZlmDebugResponseStatus::Failed => match response.payload {
            Some(Payload::Error(error))
                if !error.code.is_empty()
                    && error.code.len() <= 64
                    && error.message.len() <= 1024
                    && !error.code.contains(['\r', '\n', '\0'])
                    && !error.message.contains(['\r', '\n', '\0']) =>
            {
                Err(ZlmDebugCallError::Remote {
                    code: error.code,
                    message: error.message,
                })
            }
            _ => Err(ZlmDebugCallError::ProtocolViolation),
        },
        ZlmDebugResponseStatus::Unspecified => Err(ZlmDebugCallError::ProtocolViolation),
    }
}

impl std::fmt::Debug for ControlPlaneService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ControlPlaneService")
            .field("repository", &self.repository)
            .field("source_gateway", &self.source_gateway)
            .field("core_instance_id", &self.core_instance_id)
            .field("agent_identity", &self.agent_identity)
            .field("management_readiness", &"[DYNAMIC]")
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct RejectingAgentManagementReadinessProbe;

impl AgentManagementReadinessProbe for RejectingAgentManagementReadinessProbe {
    fn probe<'a>(
        &'a self,
        _target: &'a AuthenticatedAgentManagementTarget,
    ) -> AgentManagementFuture<'a, Result<(), AgentManagementError>> {
        Box::pin(async { Err(AgentManagementError::TargetUnavailable) })
    }
}

#[derive(Debug, Clone)]
struct SessionIdentityState {
    certificate_id: Uuid,
    fingerprint_sha256: [u8; 32],
    peer_ip: IpAddr,
    connected_at: DateTime<Utc>,
    last_activity_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
struct SessionTarget {
    node_id: Uuid,
    session_id: Uuid,
    sender: mpsc::Sender<Result<CoreEnvelope, Status>>,
    same_subnet: bool,
    has_gpu_devices: bool,
    using_gpu_path: bool,
    gpu_headroom: Option<f64>,
    slot_usage: f64,
    occupied_tasks: u32,
}

#[derive(Debug, Clone, Default)]
struct SessionLoad {
    running_tasks: u32,
    starting_tasks: u32,
    stopping_tasks: u32,
    orphaned_tasks: u32,
    runtime_slot_loads: Vec<RuntimeSlotLoad>,
    cpu_percent: f64,
    mem_percent: f64,
    disk_percent: f64,
    upload_disk_total_bytes: u64,
    upload_disk_available_bytes: u64,
    upload_disk_used_percent: f64,
    zlm_alive: bool,
    ffmpeg_alive: bool,
    artifact_cleanup_blocked: bool,
    gpu_runtime: Vec<GpuRuntimeStats>,
}

#[derive(Debug, Clone, Default)]
struct SessionCapabilities {
    gpu_devices: Vec<GpuDeviceInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecutionPreference {
    CpuOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DispatchReservation {
    task_id: Uuid,
    source_mode: SourceMode,
}

#[derive(Debug, Clone)]
pub struct NodeLiveLoad {
    pub connected: bool,
    pub running_tasks: u32,
    pub starting_tasks: u32,
    pub stopping_tasks: u32,
    pub orphaned_tasks: u32,
    pub runtime_slot_loads: Vec<RuntimeSlotLoad>,
    pub cpu_percent: f64,
    pub mem_percent: f64,
    pub disk_percent: f64,
    pub upload_disk_total_bytes: u64,
    pub upload_disk_available_bytes: u64,
    pub upload_disk_used_percent: f64,
    pub zlm_alive: bool,
    pub ffmpeg_alive: bool,
    pub artifact_cleanup_blocked: bool,
    pub gpu_runtime: Vec<GpuRuntimeStats>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct DispatchScore {
    same_subnet: bool,
    gpu_headroom: Option<f64>,
    slot_usage: f64,
    occupied_tasks: u32,
    node_id: Uuid,
}

#[derive(Debug)]
enum ClaimResult {
    Selected(SessionTarget),
    NoConnectedNode,
    MissingRequiredLabels,
}

impl ControlPlaneService {
    #[cfg(test)]
    pub fn new(repository: Arc<TaskRepository>) -> Self {
        Self::new_with_core_instance_id(repository, Uuid::now_v7())
    }

    pub(crate) fn new_with_core_instance_id(
        repository: Arc<TaskRepository>,
        core_instance_id: Uuid,
    ) -> Self {
        debug_assert!(!core_instance_id.is_nil());
        Self {
            repository,
            source_gateway: None,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            core_instance_id,
            agent_identity: None,
            management_readiness: Arc::new(RejectingAgentManagementReadinessProbe),
            pending_rotations: Arc::new(Mutex::new(HashMap::new())),
            rotation_activation_retry_interval: CERTIFICATE_ROTATION_ACTIVATION_RETRY_INTERVAL,
            pending_zlm: Arc::new(StdMutex::new(HashMap::new())),
            zlm_request_timeout: ZLM_DEBUG_REQUEST_TIMEOUT,
            zlm_hook_handler: Arc::new(RejectingZlmHookHandler),
            zlm_hook_sessions: Arc::new(Mutex::new(HashMap::new())),
            zlm_hook_global_admission: Arc::new(Semaphore::new(MAX_ACTIVE_ZLM_HOOKS_GLOBAL)),
            zlm_hook_durable_fence_admission: Arc::new(Semaphore::new(
                MAX_EXECUTING_ZLM_HOOK_FENCES_GLOBAL,
            )),
        }
    }

    #[cfg(test)]
    pub fn with_source_gateway(
        repository: Arc<TaskRepository>,
        source_gateway: SourceGatewayClient,
    ) -> Self {
        Self::with_source_gateway_and_core_instance_id(repository, source_gateway, Uuid::now_v7())
    }

    pub(crate) fn with_source_gateway_and_core_instance_id(
        repository: Arc<TaskRepository>,
        source_gateway: SourceGatewayClient,
        core_instance_id: Uuid,
    ) -> Self {
        debug_assert!(!core_instance_id.is_nil());
        Self {
            repository,
            source_gateway: Some(source_gateway),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            core_instance_id,
            agent_identity: None,
            management_readiness: Arc::new(RejectingAgentManagementReadinessProbe),
            pending_rotations: Arc::new(Mutex::new(HashMap::new())),
            rotation_activation_retry_interval: CERTIFICATE_ROTATION_ACTIVATION_RETRY_INTERVAL,
            pending_zlm: Arc::new(StdMutex::new(HashMap::new())),
            zlm_request_timeout: ZLM_DEBUG_REQUEST_TIMEOUT,
            zlm_hook_handler: Arc::new(RejectingZlmHookHandler),
            zlm_hook_sessions: Arc::new(Mutex::new(HashMap::new())),
            zlm_hook_global_admission: Arc::new(Semaphore::new(MAX_ACTIVE_ZLM_HOOKS_GLOBAL)),
            zlm_hook_durable_fence_admission: Arc::new(Semaphore::new(
                MAX_EXECUTING_ZLM_HOOK_FENCES_GLOBAL,
            )),
        }
    }

    pub(crate) fn with_agent_identity_and_readiness(
        mut self,
        agent_identity: AgentIdentityService,
        management_readiness: Arc<dyn AgentManagementReadinessProbe>,
    ) -> Self {
        self.agent_identity = Some(agent_identity);
        self.management_readiness = management_readiness;
        self
    }

    pub(crate) fn with_zlm_hook_handler(mut self, handler: Arc<dyn ZlmHookHandler>) -> Self {
        self.zlm_hook_handler = handler;
        self
    }

    #[cfg(test)]
    fn with_zlm_request_timeout_for_test(mut self, timeout: Duration) -> Self {
        self.zlm_request_timeout = timeout;
        self
    }

    #[cfg(test)]
    fn with_rotation_activation_retry_interval_for_test(mut self, interval: Duration) -> Self {
        assert!(!interval.is_zero());
        self.rotation_activation_retry_interval = interval;
        self
    }

    pub fn into_server(self) -> ControlPlaneServer<Self> {
        ControlPlaneServer::new(self)
            .max_decoding_message_size(CONTROL_MAX_MESSAGE_BYTES)
            .max_encoding_message_size(CONTROL_MAX_MESSAGE_BYTES)
    }

    async fn authenticated_management_target(
        &self,
        node_id: Uuid,
    ) -> Result<AuthenticatedAgentManagementTarget, AgentManagementError> {
        let (session_id, peer_ip, management_port, management_upload_max_bytes) = {
            let sessions = self.sessions.lock().await;
            let session = sessions
                .get(&node_id)
                .ok_or(AgentManagementError::TargetUnavailable)?;
            (
                session.session_id,
                session.identity.peer_ip,
                session.management_port,
                session.management_upload_max_bytes,
            )
        };
        let fingerprints = self
            .repository
            .agent_management_certificate_fingerprints_for_session(node_id, session_id, Utc::now())
            .await
            .map_err(|_| AgentManagementError::TargetUnavailable)?
            .ok_or(AgentManagementError::TargetUnavailable)?;
        {
            let sessions = self.sessions.lock().await;
            let current = sessions
                .get(&node_id)
                .ok_or(AgentManagementError::TargetUnavailable)?;
            if current.session_id != session_id
                || current.identity.peer_ip != peer_ip
                || current.management_port != management_port
                || current.management_upload_max_bytes != management_upload_max_bytes
            {
                return Err(AgentManagementError::TargetUnavailable);
            }
        }
        let current = lowercase_hex(&fingerprints.current_fingerprint_sha256);
        let rotating = fingerprints
            .rotating_fingerprint_sha256
            .map(|fingerprint| lowercase_hex(&fingerprint));
        let pins = AgentManagementCertificatePins::new(&current, rotating.as_deref())?;
        AuthenticatedAgentManagementTarget::new(
            node_id,
            session_id,
            peer_ip,
            management_port,
            management_upload_max_bytes,
            pins,
        )
    }

    async fn handle_certificate_rotation_request(
        &self,
        node_id: Uuid,
        session_id: Uuid,
        request: RpcCertificateRotationRequest,
    ) -> Result<(), Status> {
        let rotation_id = parse_canonical_uuid("rotation_id", &request.rotation_id)?;
        if request.control_csr_pem.is_empty()
            || request.management_csr_pem.is_empty()
            || request.control_csr_pem.len() > MAX_ROTATION_CSR_BYTES
            || request.management_csr_pem.len() > MAX_ROTATION_CSR_BYTES
        {
            return Err(Status::invalid_argument("rotation CSR size is invalid"));
        }
        let identity = self.agent_identity.as_ref().ok_or_else(|| {
            Status::failed_precondition("Agent certificate rotation is not configured on Core")
        })?;
        let rotation = identity
            .rotate_agent_certificates(
                rotation_id,
                node_id,
                session_id,
                &request.control_csr_pem,
                &request.management_csr_pem,
                self.session_peer_ip(node_id, session_id).await?,
                Utc::now(),
            )
            .await;
        let target = self
            .session_for_node(node_id)
            .await
            .filter(|target| target.session_id == session_id)
            .ok_or_else(|| {
                Status::permission_denied("Agent control session is no longer current")
            })?;
        let rotated = match rotation {
            Ok(rotated) => rotated,
            Err(AgentIdentityServiceError::RotationExpired) => {
                return self
                    .send_to_current_session(
                        &target,
                        CoreEnvelope {
                            payload: Some(
                                media_rpc::control_plane::core_envelope::Payload::CertificateRotationReset(
                                    CertificateRotationReset {
                                        rotation_id: rotation_id.to_string(),
                                        reason: CertificateRotationResetReason::Expired as i32,
                                    },
                                ),
                            ),
                        },
                    )
                    .await;
            }
            Err(error) => return Err(agent_rotation_status(error)),
        };
        self.send_to_current_session(
            &target,
            CoreEnvelope {
                payload: Some(
                    media_rpc::control_plane::core_envelope::Payload::CertificateRotationBundle(
                        rotation_bundle_to_rpc(rotated),
                    ),
                ),
            },
        )
        .await
    }

    async fn handle_certificate_rotation_activated(
        &self,
        node_id: Uuid,
        session_id: Uuid,
        acknowledgement: media_rpc::control_plane::CertificateRotationActivated,
    ) -> Result<(), Status> {
        let rotation_id = parse_canonical_uuid("rotation_id", &acknowledgement.rotation_id)?;
        DateTime::<Utc>::from_timestamp_millis(acknowledgement.activated_at_ms)
            .ok_or_else(|| Status::invalid_argument("rotation activated_at_ms is invalid"))?;
        let control_fingerprint_sha256 = parse_lowercase_sha256(
            "control_fingerprint_sha256",
            &acknowledgement.control_fingerprint_sha256,
        )?;
        let management_fingerprint_sha256 = parse_lowercase_sha256(
            "management_fingerprint_sha256",
            &acknowledgement.management_fingerprint_sha256,
        )?;
        let outcome = self
            .repository
            .complete_agent_certificate_rotation(AgentCertificateRotationAcknowledgement {
                rotation_id,
                node_id,
                session_id,
                control_fingerprint_sha256,
                management_fingerprint_sha256,
                acknowledged_at: Utc::now(),
            })
            .await
            .map_err(repo_status)?;
        match outcome {
            CompleteAgentCertificateRotationOutcome::Completed
            | CompleteAgentCertificateRotationOutcome::Recovered => {
                self.pending_rotations
                    .lock()
                    .await
                    .remove(&(node_id, session_id));
                Ok(())
            }
            CompleteAgentCertificateRotationOutcome::Rejected => Err(Status::permission_denied(
                "certificate rotation activation acknowledgement was rejected",
            )),
        }
    }

    async fn session_peer_ip(&self, node_id: Uuid, session_id: Uuid) -> Result<IpAddr, Status> {
        let sessions = self.sessions.lock().await;
        let session = sessions
            .get(&node_id)
            .filter(|session| session.session_id == session_id)
            .ok_or_else(|| {
                Status::permission_denied("Agent control session is no longer current")
            })?;
        Ok(session.identity.peer_ip)
    }

    fn spawn_pending_management_activation_worker(
        &self,
        node_id: Uuid,
        session_id: Uuid,
        rotation_id: Uuid,
        generation: Arc<SessionGeneration>,
    ) {
        let service = self.clone();
        tokio::spawn(async move {
            service
                .run_pending_management_activation_worker(
                    node_id,
                    session_id,
                    rotation_id,
                    generation,
                )
                .await;
        });
    }

    async fn run_pending_management_activation_worker(
        self,
        node_id: Uuid,
        session_id: Uuid,
        rotation_id: Uuid,
        generation: Arc<SessionGeneration>,
    ) {
        loop {
            let Some(context) = self
                .matching_pending_rotation(node_id, session_id, rotation_id, &generation)
                .await
            else {
                return;
            };
            let target = tokio::select! {
                biased;
                _ = generation.canceled() => return,
                target = self.authenticated_management_target(node_id) => target,
            };
            match target {
                Ok(target) if target.session_id() == session_id => {
                    let probe = tokio::select! {
                        biased;
                        _ = generation.canceled() => return,
                        probe = timeout(
                            CERTIFICATE_ROTATION_MANAGEMENT_PROBE_TIMEOUT,
                            self.management_readiness.probe(&target),
                        ) => probe,
                    };
                    match probe {
                        Ok(Ok(())) => {
                            if !self
                                .activate_pending_management_rotation(
                                    node_id,
                                    session_id,
                                    rotation_id,
                                    context,
                                    &generation,
                                )
                                .await
                            {
                                return;
                            }
                        }
                        Ok(Err(error)) => {
                            warn!(
                                node_id = %node_id,
                                session_id = %session_id,
                                rotation_id = %rotation_id,
                                error_code = error.safe_code(),
                                "rotating Agent management endpoint is not ready"
                            );
                        }
                        Err(_) => {
                            warn!(
                                node_id = %node_id,
                                session_id = %session_id,
                                rotation_id = %rotation_id,
                                timeout_seconds = CERTIFICATE_ROTATION_MANAGEMENT_PROBE_TIMEOUT.as_secs(),
                                "rotating Agent management readiness probe timed out"
                            );
                        }
                    }
                }
                Ok(_) => return,
                Err(error) => {
                    warn!(
                        node_id = %node_id,
                        session_id = %session_id,
                        rotation_id = %rotation_id,
                        error_code = error.safe_code(),
                        "Agent management rotation target is not available yet"
                    );
                }
            }

            if self
                .matching_pending_rotation(node_id, session_id, rotation_id, &generation)
                .await
                .is_none()
            {
                return;
            }
            tokio::select! {
                biased;
                _ = generation.canceled() => return,
                _ = sleep(self.rotation_activation_retry_interval) => {}
            }
        }
    }

    async fn matching_pending_rotation(
        &self,
        node_id: Uuid,
        session_id: Uuid,
        rotation_id: Uuid,
        generation: &Arc<SessionGeneration>,
    ) -> Option<AgentCertificateRotationTakeoverContext> {
        if generation.is_canceled() {
            return None;
        }
        let current_generation = self.current_session_generation(node_id, session_id).await?;
        if !Arc::ptr_eq(&current_generation, generation) || current_generation.is_canceled() {
            return None;
        }
        self.pending_rotations
            .lock()
            .await
            .get(&(node_id, session_id))
            .filter(|pending| {
                pending.context.rotation_id == rotation_id
                    && Arc::ptr_eq(&pending.generation, generation)
            })
            .map(|pending| pending.context)
    }

    async fn activate_pending_management_rotation(
        &self,
        node_id: Uuid,
        session_id: Uuid,
        rotation_id: Uuid,
        context: AgentCertificateRotationTakeoverContext,
        generation: &Arc<SessionGeneration>,
    ) -> bool {
        if self
            .matching_pending_rotation(node_id, session_id, rotation_id, generation)
            .await
            .is_none()
        {
            return false;
        }
        let outcome = tokio::select! {
            biased;
            _ = generation.canceled() => return false,
            outcome = self.repository.activate_agent_management_rotation(
                AgentManagementRotationActivationRequest {
                    rotation_id,
                    node_id,
                    session_id,
                    control_fingerprint_sha256: context.new_control_fingerprint_sha256,
                    management_fingerprint_sha256: context.new_management_fingerprint_sha256,
                    activated_at: Utc::now(),
                },
            ) => outcome,
        };
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                warn!(
                    node_id = %node_id,
                    session_id = %session_id,
                    rotation_id = %rotation_id,
                    error = %error,
                    "Agent management certificate activation failed and will be retried"
                );
                return true;
            }
        };
        let activation = match outcome {
            AgentManagementRotationActivationOutcome::Activated(context)
            | AgentManagementRotationActivationOutcome::Recovered(context) => context,
            AgentManagementRotationActivationOutcome::Rejected => {
                let mut rotations = self.pending_rotations.lock().await;
                if rotations
                    .get(&(node_id, session_id))
                    .is_some_and(|pending| {
                        pending.context.rotation_id == rotation_id
                            && Arc::ptr_eq(&pending.generation, generation)
                    })
                {
                    rotations.remove(&(node_id, session_id));
                }
                warn!(
                    node_id = %node_id,
                    session_id = %session_id,
                    rotation_id = %rotation_id,
                    "Agent management certificate activation was rejected"
                );
                return false;
            }
        };
        if self
            .matching_pending_rotation(node_id, session_id, rotation_id, generation)
            .await
            .is_none()
        {
            return false;
        }
        let Some(session) = self
            .session_for_node(node_id)
            .await
            .filter(|session| session.session_id == session_id)
        else {
            return false;
        };
        let send = self.send_to_current_session(
            &session,
            CoreEnvelope {
                payload: Some(
                    media_rpc::control_plane::core_envelope::Payload::ActivateCertificateRotation(
                        ActivateCertificateRotation {
                            rotation_id: activation.rotation_id.to_string(),
                            previous_identity_expires_at_ms: activation
                                .previous_identity_expires_at
                                .timestamp_millis(),
                        },
                    ),
                ),
            },
        );
        let send_result = tokio::select! {
            biased;
            _ = generation.canceled() => return false,
            result = send => result,
        };
        if let Err(error) = send_result {
            warn!(
                node_id = %node_id,
                session_id = %session_id,
                rotation_id = %rotation_id,
                error = %error,
                "Agent certificate rotation activation command was not delivered and will be retried"
            );
        }
        true
    }

    pub(crate) async fn zlm_debug(
        &self,
        node_id: Uuid,
        command: ZlmDebugCommand,
    ) -> Result<ZlmDebugResult, ZlmDebugCallError> {
        let target = self
            .session_for_node(node_id)
            .await
            .ok_or(ZlmDebugCallError::Disconnected)?;
        let request_id = Uuid::now_v7();
        let (request, operation, expects_snapshot) = command.into_rpc(request_id)?;
        let (sender, receiver) = oneshot::channel();
        let pending_key = (node_id, target.session_id, request_id);
        {
            let mut pending = lock_pending_zlm(&self.pending_zlm);
            let session_pending = pending
                .keys()
                .filter(|(pending_node, pending_session, _)| {
                    *pending_node == node_id && *pending_session == target.session_id
                })
                .count();
            if pending.len() >= MAX_PENDING_ZLM_GLOBAL
                || session_pending >= MAX_PENDING_ZLM_PER_SESSION
            {
                return Err(ZlmDebugCallError::Busy);
            }
            pending.insert(
                pending_key,
                PendingZlmWaiter {
                    operation,
                    expects_snapshot,
                    sender,
                },
            );
        }
        let _registration = PendingZlmRegistrationGuard::new(self.pending_zlm.clone(), pending_key);
        let envelope = CoreEnvelope {
            payload: Some(
                media_rpc::control_plane::core_envelope::Payload::ZlmDebugRequest(request),
            ),
        };
        if self
            .send_to_current_session(&target, envelope)
            .await
            .is_err()
        {
            return Err(ZlmDebugCallError::Disconnected);
        }
        match timeout(self.zlm_request_timeout, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(ZlmDebugCallError::Disconnected),
            Err(_) => Err(ZlmDebugCallError::DeadlineExceeded),
        }
    }

    async fn handle_zlm_debug_response(
        &self,
        node_id: Uuid,
        session_id: Uuid,
        response: ZlmDebugResponse,
    ) -> Result<(), Status> {
        let request_id = parse_canonical_uuid("request_id", &response.request_id)?;
        let waiter = {
            let mut pending = lock_pending_zlm(&self.pending_zlm);
            if pending.keys().any(|(_, pending_session, pending_request)| {
                *pending_request == request_id && *pending_session != session_id
            }) {
                return Err(Status::permission_denied(
                    "ZLM debug response belongs to a different Agent session",
                ));
            }
            pending.remove(&(node_id, session_id, request_id))
        };
        let Some(waiter) = waiter else {
            debug!(
                node_id = %node_id,
                session_id = %session_id,
                request_id = %request_id,
                "late or unknown ZLM debug response ignored"
            );
            return Ok(());
        };
        if response.operation != waiter.operation as i32 {
            let _ = waiter
                .sender
                .send(Err(ZlmDebugCallError::ProtocolViolation));
            return Err(Status::failed_precondition(
                "ZLM debug response operation does not match its request",
            ));
        }
        let result = parse_zlm_debug_response(response, waiter.expects_snapshot);
        let protocol_violation = matches!(result, Err(ZlmDebugCallError::ProtocolViolation));
        let _ = waiter.sender.send(result);
        if protocol_violation {
            Err(Status::failed_precondition(
                "ZLM debug response payload violated the protocol",
            ))
        } else {
            Ok(())
        }
    }

    async fn handle_zlm_hook_request(
        &self,
        node_id: Uuid,
        session_id: Uuid,
        request: RpcZlmHookRequest,
    ) -> Result<(), Status> {
        let response_request_id = safe_zlm_hook_response_request_id(&request.request_id);
        let request = match parse_zlm_hook_request(request) {
            Ok(request) => request,
            Err(error) => {
                let http_status = if error == ZlmHookRequestError::BodyTooLarge {
                    413
                } else {
                    400
                };
                return self
                    .send_zlm_hook_response(
                        node_id,
                        session_id,
                        response_request_id,
                        ZlmHookHandlerResponse {
                            http_status,
                            body: serde_json::json!({
                                "code": -1,
                                "msg": "invalid ZLM hook request",
                            }),
                        },
                    )
                    .await;
            }
        };
        let generation = self
            .current_session_generation(node_id, session_id)
            .await
            .filter(|generation| !generation.is_canceled())
            .ok_or_else(|| {
                Status::permission_denied("Agent control session is no longer current")
            })?;
        match self
            .admit_zlm_hook_request(node_id, session_id, &request, &generation)
            .await
        {
            ZlmHookAdmission::Start(global_permit) => {
                let service = self.clone();
                tokio::spawn(async move {
                    service
                        .run_zlm_hook_worker(
                            node_id,
                            session_id,
                            request,
                            generation,
                            global_permit,
                        )
                        .await;
                });
                Ok(())
            }
            ZlmHookAdmission::PendingDuplicate => Ok(()),
            ZlmHookAdmission::Replay(response) => {
                self.send_zlm_hook_response(
                    node_id,
                    session_id,
                    request.request_id.to_string(),
                    response,
                )
                .await
            }
            ZlmHookAdmission::Conflict => {
                self.send_zlm_hook_response(
                    node_id,
                    session_id,
                    request.request_id.to_string(),
                    zlm_hook_error_response(
                        409,
                        "ZLM hook request_id was reused with different content",
                    ),
                )
                .await
            }
            ZlmHookAdmission::Busy => {
                self.send_zlm_hook_response(
                    node_id,
                    session_id,
                    request.request_id.to_string(),
                    zlm_hook_error_response(429, "too many ZLM hook requests are active"),
                )
                .await
            }
            ZlmHookAdmission::Fenced => Err(Status::permission_denied(
                "Agent control session is no longer current",
            )),
        }
    }

    async fn admit_zlm_hook_request(
        &self,
        node_id: Uuid,
        session_id: Uuid,
        request: &ParsedZlmHookRequest,
        generation: &SessionGeneration,
    ) -> ZlmHookAdmission {
        let mut sessions = self.zlm_hook_sessions.lock().await;
        if generation.is_canceled() {
            return ZlmHookAdmission::Fenced;
        }
        let state = sessions.entry((node_id, session_id)).or_default();
        if let Some(existing) = state.requests.get(&request.request_id) {
            return match existing {
                ZlmHookRequestState::Pending { fingerprint }
                    if *fingerprint == request.fingerprint =>
                {
                    ZlmHookAdmission::PendingDuplicate
                }
                ZlmHookRequestState::Completed {
                    fingerprint,
                    response,
                } if *fingerprint == request.fingerprint => {
                    ZlmHookAdmission::Replay(response.clone())
                }
                _ => ZlmHookAdmission::Conflict,
            };
        }
        if state.active >= MAX_ACTIVE_ZLM_HOOKS_PER_SESSION {
            return ZlmHookAdmission::Busy;
        }
        let Ok(global_permit) = self.zlm_hook_global_admission.clone().try_acquire_owned() else {
            return ZlmHookAdmission::Busy;
        };
        state.active += 1;
        state.requests.insert(
            request.request_id,
            ZlmHookRequestState::Pending {
                fingerprint: request.fingerprint,
            },
        );
        ZlmHookAdmission::Start(global_permit)
    }

    async fn run_zlm_hook_worker(
        self,
        node_id: Uuid,
        session_id: Uuid,
        request: ParsedZlmHookRequest,
        generation: Arc<SessionGeneration>,
        _global_permit: OwnedSemaphorePermit,
    ) {
        let durable_fence_admission = self.zlm_hook_durable_fence_admission.clone();
        let _durable_fence_permit = tokio::select! {
            biased;
            _ = generation.canceled() => {
                self.discard_zlm_hook_request(
                    node_id,
                    session_id,
                    request.request_id,
                    request.fingerprint,
                ).await;
                return;
            }
            permit = durable_fence_admission.acquire_owned() => {
                permit.expect("ZLM hook durable-fence semaphore remains open")
            }
        };
        let durable_fence = tokio::select! {
            biased;
            _ = generation.canceled() => {
                self.discard_zlm_hook_request(
                    node_id,
                    session_id,
                    request.request_id,
                    request.fingerprint,
                ).await;
                return;
            }
            fence = self.repository.begin_agent_control_session_fence(
                node_id,
                session_id,
                Utc::now(),
            ) => fence,
        };
        let durable_fence = match durable_fence {
            Ok(Some(fence)) if !generation.is_canceled() => fence,
            Ok(_) => {
                self.discard_zlm_hook_request(
                    node_id,
                    session_id,
                    request.request_id,
                    request.fingerprint,
                )
                .await;
                return;
            }
            Err(error) => {
                warn!(
                    node_id = %node_id,
                    session_id = %session_id,
                    request_id = %request.request_id,
                    error = %error,
                    "failed to acquire durable ZLM hook session fence"
                );
                self.discard_zlm_hook_request(
                    node_id,
                    session_id,
                    request.request_id,
                    request.fingerprint,
                )
                .await;
                return;
            }
        };

        let handler = self.zlm_hook_handler.clone();
        let hook = AuthenticatedZlmHook {
            node_id,
            hook_name: request.hook_name,
            body: request.body,
        };
        let mut worker = tokio::spawn(async move { handler.handle(hook).await });
        let response = tokio::select! {
            biased;
            _ = generation.canceled() => {
                worker.abort();
                let _ = worker.await;
                self.discard_zlm_hook_request(
                    node_id,
                    session_id,
                    request.request_id,
                    request.fingerprint,
                ).await;
                return;
            }
            result = timeout(ZLM_HOOK_HANDLER_TIMEOUT, &mut worker) => match result {
                Ok(Ok(response)) => normalize_zlm_hook_response(response),
                Ok(Err(error)) => {
                    warn!(
                        node_id = %node_id,
                        session_id = %session_id,
                        request_id = %request.request_id,
                        error = %error,
                        "ZLM hook processor task failed"
                    );
                    zlm_hook_error_response(500, "ZLM hook processing failed")
                }
                Err(_) => {
                    worker.abort();
                    let _ = worker.await;
                    zlm_hook_error_response(504, "ZLM hook processing timed out")
                }
            }
        };
        let completed = !generation.is_canceled()
            && self
                .complete_zlm_hook_request(
                    node_id,
                    session_id,
                    request.request_id,
                    request.fingerprint,
                    response.clone(),
                )
                .await;
        if let Err(error) = durable_fence.commit().await {
            warn!(
                node_id = %node_id,
                session_id = %session_id,
                request_id = %request.request_id,
                error = %error,
                "durable ZLM hook session fence release failed"
            );
        }
        if !completed {
            return;
        }
        if let Err(error) = self
            .send_zlm_hook_response(
                node_id,
                session_id,
                request.request_id.to_string(),
                response,
            )
            .await
        {
            debug!(
                node_id = %node_id,
                session_id = %session_id,
                request_id = %request.request_id,
                error = %error,
                "completed ZLM hook response was not delivered to its original session"
            );
        }
    }

    async fn discard_zlm_hook_request(
        &self,
        node_id: Uuid,
        session_id: Uuid,
        request_id: Uuid,
        fingerprint: [u8; 32],
    ) {
        let key = (node_id, session_id);
        let mut sessions = self.zlm_hook_sessions.lock().await;
        let remove_session = if let Some(state) = sessions.get_mut(&key) {
            if matches!(
                state.requests.get(&request_id),
                Some(ZlmHookRequestState::Pending {
                    fingerprint: pending,
                }) if *pending == fingerprint
            ) {
                state.active = state.active.saturating_sub(1);
                state.requests.remove(&request_id);
            }
            state.active == 0 && state.requests.is_empty()
        } else {
            false
        };
        if remove_session {
            sessions.remove(&key);
        }
    }

    async fn complete_zlm_hook_request(
        &self,
        node_id: Uuid,
        session_id: Uuid,
        request_id: Uuid,
        fingerprint: [u8; 32],
        response: ZlmHookHandlerResponse,
    ) -> bool {
        let mut sessions = self.zlm_hook_sessions.lock().await;
        let Some(state) = sessions.get_mut(&(node_id, session_id)) else {
            return false;
        };
        if !matches!(
            state.requests.get(&request_id),
            Some(ZlmHookRequestState::Pending {
                fingerprint: pending,
            }) if *pending == fingerprint
        ) {
            return false;
        }
        state.active = state.active.saturating_sub(1);
        state.requests.insert(
            request_id,
            ZlmHookRequestState::Completed {
                fingerprint,
                response,
            },
        );
        state.completed_order.push_back(request_id);
        while state.completed_order.len() > MAX_COMPLETED_ZLM_HOOKS_PER_SESSION {
            if let Some(expired) = state.completed_order.pop_front() {
                if matches!(
                    state.requests.get(&expired),
                    Some(ZlmHookRequestState::Completed { .. })
                ) {
                    state.requests.remove(&expired);
                }
            }
        }
        true
    }

    async fn send_zlm_hook_response(
        &self,
        node_id: Uuid,
        session_id: Uuid,
        request_id: String,
        response: ZlmHookHandlerResponse,
    ) -> Result<(), Status> {
        let body_json = serde_json::to_string(&response.body)
            .map_err(|_| Status::internal("serialize ZLM hook response"))?;
        let target = self
            .session_for_node(node_id)
            .await
            .filter(|target| target.session_id == session_id)
            .ok_or_else(|| {
                Status::permission_denied("Agent control session is no longer current")
            })?;
        self.send_to_current_session(
            &target,
            CoreEnvelope {
                payload: Some(
                    media_rpc::control_plane::core_envelope::Payload::ZlmHookResponse(
                        ZlmHookResponse {
                            request_id,
                            http_status: u32::from(response.http_status),
                            body_json,
                        },
                    ),
                ),
            },
        )
        .await
        .map_err(|_| Status::unavailable("Agent control session is not writable"))
    }

    async fn cancel_zlm_waiters(&self, node_id: Uuid, session_id: Uuid) {
        let senders = {
            let mut pending = lock_pending_zlm(&self.pending_zlm);
            let keys = pending
                .keys()
                .filter(|(pending_node, pending_session, _)| {
                    *pending_node == node_id && *pending_session == session_id
                })
                .copied()
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| pending.remove(&key).map(|waiter| waiter.sender))
                .collect::<Vec<_>>()
        };
        for sender in senders {
            let _ = sender.send(Err(ZlmDebugCallError::Disconnected));
        }
    }

    #[cfg(test)]
    async fn pending_zlm_waiter_count(&self) -> usize {
        lock_pending_zlm(&self.pending_zlm).len()
    }

    pub async fn dispatch_task(&self, task_id: Uuid) -> Result<(), ControlPlaneError> {
        self.repository.ensure_task_queued(task_id).await?;
        let mut resolved_spec =
            serde_json::from_value::<TaskSpec>(self.repository.get_resolved_spec(task_id).await?)?;
        if let Some(source_gateway) = &self.source_gateway {
            match source_gateway
                .prepare_task_spec(task_id, &resolved_spec)
                .await
            {
                Ok(Some(rewritten_spec)) => {
                    self.repository
                        .update_queued_resolved_spec(task_id, &rewritten_spec)
                        .await?;
                    resolved_spec = rewritten_spec;
                }
                Ok(None) => {}
                Err(error) => {
                    let reason = error.to_string();
                    self.repository
                        .fail_queued_task(task_id, "source_gateway_failed", &reason)
                        .await?;
                    warn!(
                        task_id = %task_id,
                        error = %reason,
                        "task failed before dispatch because source gateway preparation failed"
                    );
                    return Ok(());
                }
            }
        }
        let source_affinity_ip = task_source_affinity_ip(&resolved_spec);
        let uploaded_file_affinity_node = task_uploaded_file_affinity_node(&resolved_spec);
        let execution_preference = task_execution_preference(&resolved_spec);
        let retry_affinity_node = if task_keeps_retry_node_affinity(&resolved_spec) {
            self.repository
                .preferred_retry_node_after_disconnect(task_id)
                .await?
        } else {
            None
        };
        let forced_node = uploaded_file_affinity_node.or(retry_affinity_node);
        let claim = if let Some(node_id) = forced_node {
            self.claim_session_by_node(
                node_id,
                source_affinity_ip,
                task_id,
                &resolved_spec,
                execution_preference,
            )
            .await
        } else {
            self.claim_best_session(
                source_affinity_ip,
                task_id,
                &resolved_spec,
                execution_preference,
            )
            .await
        };
        let target = match claim {
            ClaimResult::Selected(target) => target,
            ClaimResult::NoConnectedNode => return Err(ControlPlaneError::NoConnectedNode),
            ClaimResult::MissingRequiredLabels => {
                let required_labels: Vec<String> = resolved_spec
                    .resource
                    .required_labels
                    .iter()
                    .map(|label| label.trim())
                    .filter(|label| !label.is_empty())
                    .map(str::to_string)
                    .collect();
                let failure_reason = format!(
                    "no online node satisfies required_labels: {}",
                    required_labels.join(", ")
                );
                self.repository
                    .fail_queued_task(task_id, "required_labels_unmatched", &failure_reason)
                    .await?;
                warn!(
                    task_id = %task_id,
                    required_labels = ?required_labels,
                    "task failed because no online node satisfies required_labels"
                );
                return Ok(());
            }
        };
        let command = match self
            .repository
            .prepare_task_dispatch(task_id, target.node_id, &format!("node:{}", target.node_id))
            .await
        {
            Ok(command) => command,
            Err(error) => {
                self.release_dispatch_reservation(target.node_id, Some(target.session_id), task_id)
                    .await;
                return Err(error.into());
            }
        };

        let envelope = CoreEnvelope {
            payload: Some(media_rpc::control_plane::core_envelope::Payload::StartTask(
                media_rpc::control_plane::StartTask {
                    task_id: command.task_id.to_string(),
                    attempt_no: command.attempt_no,
                    task_type: command.task_type.as_str().to_string(),
                    resolved_spec_json: serde_json::to_string(&command.resolved_spec)?,
                    execution_mode: "managed".to_string(),
                    lease_token: command.lease_token,
                    trace_context: String::new(),
                },
            )),
        };

        if self
            .send_to_current_session(&target, envelope)
            .await
            .is_err()
        {
            self.release_dispatch_reservation(target.node_id, Some(target.session_id), task_id)
                .await;
            self.repository
                .rollback_task_dispatch(
                    task_id,
                    command.attempt_no,
                    target.node_id,
                    "failed to send start_task to agent",
                )
                .await?;
            self.close_session(target.node_id, target.session_id).await;
            return Err(ControlPlaneError::NodeDisconnected(target.node_id));
        }

        info!(
            task_id = %task_id,
            node_id = %command.node_id,
            attempt_no = command.attempt_no,
            source_affinity_ip = ?source_affinity_ip,
            execution_preference = ?execution_preference,
            gpu_dispatched = target.using_gpu_path,
            gpu_node_used_as_cpu_node = !target.using_gpu_path && target.has_gpu_devices,
            same_subnet = target.same_subnet,
            gpu_headroom = target.gpu_headroom,
            slot_usage = target.slot_usage,
            occupied_tasks = target.occupied_tasks,
            "start_task dispatched to agent"
        );

        Ok(())
    }

    pub async fn request_stop(
        &self,
        task_id: Uuid,
        reason: impl Into<String>,
        grace_period_sec: u32,
        force_after_sec: u32,
    ) -> Result<(), ControlPlaneError> {
        if let Some(source_gateway) = &self.source_gateway {
            if let Err(error) = source_gateway.delete_relay(task_id).await {
                warn!(
                    task_id = %task_id,
                    error = %error,
                    "failed to delete source gateway relay during stop"
                );
            }
        }
        let Some(command) = self
            .repository
            .build_stop_command(task_id, reason, grace_period_sec, force_after_sec)
            .await?
        else {
            return Ok(());
        };

        let Some(target) = self.session_for_node(command.node_id).await else {
            info!(
                task_id = %task_id,
                node_id = %command.node_id,
                attempt_no = command.attempt_no,
                "stop intent persisted while node session is disconnected"
            );
            return Ok(());
        };

        let envelope = CoreEnvelope {
            payload: Some(media_rpc::control_plane::core_envelope::Payload::StopTask(
                media_rpc::control_plane::StopTask {
                    task_id: command.task_id.to_string(),
                    attempt_no: command.attempt_no,
                    lease_token: command.lease_token,
                    reason: command.reason,
                    grace_period_sec: command.grace_period_sec,
                    force_after_sec: command.force_after_sec,
                },
            )),
        };

        if self
            .send_to_current_session(&target, envelope)
            .await
            .is_err()
        {
            self.close_session(target.node_id, target.session_id).await;
            info!(
                task_id = %task_id,
                node_id = %target.node_id,
                attempt_no = command.attempt_no,
                "stop intent persisted but stop_task delivery raced with session close"
            );
            return Ok(());
        }

        info!(
            task_id = %task_id,
            node_id = %command.node_id,
            attempt_no = command.attempt_no,
            "stop_task sent to agent"
        );

        Ok(())
    }

    pub async fn request_recording_control(
        &self,
        task_id: Uuid,
        action: &'static str,
        record_config: Option<Value>,
        reason: impl Into<String>,
        command_id: String,
    ) -> Result<RecordingControlCommand, ControlPlaneError> {
        let command = self
            .repository
            .build_recording_control_command(task_id)
            .await?;
        let Some(target) = self.session_for_node(command.node_id).await else {
            return Err(ControlPlaneError::NodeDisconnected(command.node_id));
        };
        let envelope = CoreEnvelope {
            payload: Some(
                media_rpc::control_plane::core_envelope::Payload::TaskRecordingControl(
                    TaskRecordingControl {
                        task_id: command.task_id.to_string(),
                        attempt_no: command.attempt_no,
                        lease_token: command.lease_token.clone(),
                        action: action.to_string(),
                        record_config_json: record_config
                            .map(|value| serde_json::to_string(&value))
                            .transpose()?
                            .unwrap_or_default(),
                        reason: reason.into(),
                        command_id,
                    },
                ),
            ),
        };

        if self
            .send_to_current_session(&target, envelope)
            .await
            .is_err()
        {
            self.close_session(target.node_id, target.session_id).await;
            return Err(ControlPlaneError::NodeDisconnected(target.node_id));
        }

        info!(
            task_id = %task_id,
            node_id = %command.node_id,
            attempt_no = command.attempt_no,
            action,
            "task recording control sent to agent"
        );

        Ok(command)
    }

    #[cfg(test)]
    async fn bootstrap_session(
        &self,
        registration: &AgentRegistration,
        peer: &AuthenticatedAgentPeer,
        sender: mpsc::Sender<Result<CoreEnvelope, Status>>,
    ) -> Result<Uuid, Status> {
        self.bootstrap_session_with_management(registration, peer, 9443, 64 * 1024 * 1024, sender)
            .await
    }

    async fn bootstrap_session_with_management(
        &self,
        registration: &AgentRegistration,
        peer: &AuthenticatedAgentPeer,
        management_port: u16,
        management_upload_max_bytes: u64,
        sender: mpsc::Sender<Result<CoreEnvelope, Status>>,
    ) -> Result<Uuid, Status> {
        if registration.node_id != peer.node_id {
            return Err(Status::permission_denied(
                "Agent certificate identity does not match registration",
            ));
        }
        let session_id = Uuid::now_v7();

        // Prepare every fallible bootstrap artifact before claiming the durable
        // session. A failed probe/reclaim preparation can therefore never evict
        // a healthy existing stream.
        let reclaim_runtimes = self
            .repository
            .list_reclaim_runtimes(registration.node_id)
            .await
            .map_err(repo_status)?;
        let mut bootstrap_envelopes = vec![CoreEnvelope {
            payload: Some(
                media_rpc::control_plane::core_envelope::Payload::ProbeCapabilities(
                    ProbeCapabilities {},
                ),
            ),
        }];
        if !reclaim_runtimes.is_empty() {
            bootstrap_envelopes.push(CoreEnvelope {
                payload: Some(
                    media_rpc::control_plane::core_envelope::Payload::AdoptOrphans(AdoptOrphans {
                        runtimes: reclaim_runtimes
                            .into_iter()
                            .map(|runtime| ReclaimRuntime {
                                task_id: runtime.task_id.to_string(),
                                attempt_no: runtime.attempt_no,
                                lease_token: runtime.lease_token,
                                worker_kind: runtime.worker_kind.as_str().to_string(),
                            })
                            .collect(),
                    }),
                ),
            });
        }

        // Start the lease only after every fallible preparation has completed,
        // so preparation time cannot consume its 30-second lifetime before the
        // stream is registered.
        let connected_at = Utc::now();
        let claim = self
            .repository
            .claim_agent_control_session(AgentControlSessionClaim {
                registration: registration.clone(),
                session_id,
                core_instance_id: self.core_instance_id,
                certificate_fingerprint_sha256: peer.fingerprint_sha256,
                peer_ip: peer.peer_ip,
                connected_at,
                lease_expires_at: connected_at + chrono::Duration::seconds(30),
            })
            .await
            .map_err(repo_status)?;
        let (certificate_id, durable_replaced_session_id, takeover_reason, rotation_context) =
            match claim {
                AgentControlSessionClaimOutcome::Claimed {
                    certificate_id,
                    replaced_session_id,
                    takeover_reason,
                    rotation_context,
                } => (
                    certificate_id,
                    replaced_session_id,
                    takeover_reason,
                    rotation_context,
                ),
                AgentControlSessionClaimOutcome::DuplicateHealthy { .. } => {
                    return Err(Status::already_exists(
                        "a healthy Agent control session already exists",
                    ));
                }
                AgentControlSessionClaimOutcome::UnauthorizedCertificate(_failure) => {
                    return Err(Status::unauthenticated(
                        "Agent certificate is not authorized",
                    ));
                }
            };

        let session_generation = Arc::new(SessionGeneration::default());
        let replaced = {
            let mut sessions = self.sessions.lock().await;
            let replaced = sessions.insert(
                registration.node_id,
                SessionHandle {
                    session_id,
                    generation: session_generation.clone(),
                    sender: sender.clone(),
                    registration: registration.clone(),
                    identity: SessionIdentityState {
                        certificate_id,
                        fingerprint_sha256: peer.fingerprint_sha256,
                        peer_ip: peer.peer_ip,
                        connected_at,
                        last_activity_at: connected_at,
                    },
                    capabilities: SessionCapabilities::default(),
                    load: SessionLoad::default(),
                    reservations: VecDeque::new(),
                    management_port,
                    management_upload_max_bytes,
                },
            );
            if let Some(replaced) = &replaced {
                // Cancel while the session map is still locked. An old hook
                // admission can therefore observe either the old live
                // generation or its canceled state, never a replacement with
                // an uncanceled old generation.
                replaced.generation.cancel();
            }
            replaced
        };
        if let Some(replaced) = replaced {
            self.pending_rotations
                .lock()
                .await
                .remove(&(registration.node_id, replaced.session_id));
            self.cancel_zlm_waiters(registration.node_id, replaced.session_id)
                .await;
            self.zlm_hook_sessions
                .lock()
                .await
                .remove(&(registration.node_id, replaced.session_id));
            let _ = replaced.sender.try_send(Err(Status::aborted(
                "Agent control session was fenced by an authorized takeover",
            )));
        }
        // Nothing is exposed to the response stream until authorization and
        // durable claim both succeed. The fresh bounded channel has capacity
        // for all bootstrap envelopes, so this does not await an untrusted
        // receiver or leak reclaim leases to a rejected duplicate session.
        for envelope in bootstrap_envelopes {
            if sender.try_send(Ok(envelope)).is_err() {
                self.close_session(registration.node_id, session_id).await;
                return Err(Status::unavailable(
                    "failed to initialize Agent control response stream",
                ));
            }
        }
        if let Some(context) = rotation_context {
            let rotation_id = context.rotation_id;
            self.pending_rotations.lock().await.insert(
                (registration.node_id, session_id),
                PendingSessionRotation {
                    context,
                    generation: session_generation.clone(),
                },
            );
            self.spawn_pending_management_activation_worker(
                registration.node_id,
                session_id,
                rotation_id,
                session_generation,
            );
        }

        info!(
            node_id = %registration.node_id,
            node_name = %registration.node_name,
            session_id = %session_id,
            certificate_id = %certificate_id,
            durable_replaced_session_id = ?durable_replaced_session_id,
            takeover_reason = ?takeover_reason,
            "control-plane session registered"
        );

        Ok(session_id)
    }

    async fn send_to_current_session(
        &self,
        target: &SessionTarget,
        envelope: CoreEnvelope,
    ) -> Result<(), Status> {
        let permit = timeout(CONTROL_STREAM_IDLE_TIMEOUT, target.sender.reserve())
            .await
            .map_err(|_| Status::deadline_exceeded("Agent response channel is not draining"))?
            .map_err(|_| Status::unavailable("Agent response stream is closed"))?;
        let Some(fence) = self
            .repository
            .begin_agent_control_session_fence(target.node_id, target.session_id, Utc::now())
            .await
            .map_err(repo_status)?
        else {
            drop(permit);
            self.close_session(target.node_id, target.session_id).await;
            return Err(Status::permission_denied(
                "Agent control session is no longer current",
            ));
        };

        // The reserved channel slot makes enqueue synchronous. Keep the
        // shared database row fence until after enqueue, so takeover either
        // linearizes before this send (and rejects it) or waits until this send
        // has been enqueued.
        permit.send(Ok(envelope));
        if let Err(error) = fence.commit().await {
            // The command was already linearized while the shared row lock was
            // held. A read-only fence commit failure must not make the caller
            // roll back a task that the Agent may already have received.
            warn!(
                node_id = %target.node_id,
                session_id = %target.session_id,
                error = %error,
                "Agent command was enqueued but session fence release reported an error"
            );
        }
        Ok(())
    }

    async fn process_stream(
        &self,
        node_id: Uuid,
        session_id: Uuid,
        mut inbound: Streaming<AgentEnvelope>,
    ) {
        loop {
            let message = match timeout(CONTROL_STREAM_IDLE_TIMEOUT, inbound.message()).await {
                Ok(Ok(Some(message))) => message,
                Ok(Ok(None)) => break,
                Ok(Err(error)) => {
                    warn!(node_id = %node_id, error = %error, "control-plane stream failed");
                    break;
                }
                Err(_) => {
                    warn!(node_id = %node_id, "control-plane stream timed out");
                    break;
                }
            };

            if !self.is_current_session(node_id, session_id).await {
                debug!(
                    node_id = %node_id,
                    session_id = %session_id,
                    "stale control-plane session observed after replacement"
                );
                break;
            }

            let Some(payload) = message.payload else {
                warn!(
                    node_id = %node_id,
                    session_id = %session_id,
                    "empty Agent control envelope rejected"
                );
                break;
            };

            if let Err(error) = self.handle_payload(node_id, session_id, payload).await {
                warn!(node_id = %node_id, error = %error, "failed to process control-plane payload");
                if matches!(
                    error.code(),
                    tonic::Code::Unauthenticated
                        | tonic::Code::PermissionDenied
                        | tonic::Code::FailedPrecondition
                ) {
                    break;
                }
            }
        }

        self.close_session(node_id, session_id).await;
    }

    async fn handle_payload(
        &self,
        node_id: Uuid,
        session_id: Uuid,
        payload: media_rpc::control_plane::agent_envelope::Payload,
    ) -> Result<(), Status> {
        if !self.is_current_session(node_id, session_id).await {
            return Err(Status::permission_denied(
                "Agent control session is no longer current",
            ));
        }
        self.note_session_activity(node_id, session_id, Utc::now())
            .await?;

        match payload {
            media_rpc::control_plane::agent_envelope::Payload::Register(register) => {
                let registration = authenticated_registration_from_rpc(register)?.registration;
                if registration.node_id != node_id {
                    return Err(Status::failed_precondition(format!(
                        "node_id cannot change on an active control-plane stream: expected {node_id}, got {}",
                        registration.node_id
                    )));
                }
                return Err(Status::failed_precondition(
                    "register is only accepted as the first control-plane message",
                ));
            }
            media_rpc::control_plane::agent_envelope::Payload::Heartbeat(heartbeat) => {
                let snapshot = heartbeat_from_rpc(heartbeat)?;
                if !session_write_applied(
                    self.repository
                        .record_node_heartbeat_for_session(node_id, session_id, &snapshot)
                        .await
                        .map_err(repo_status)?,
                )? {
                    return Ok(());
                }
                self.update_session_load(node_id, session_id, &snapshot)
                    .await?;
                debug!(
                    node_id = %node_id,
                    running_tasks = snapshot.running_tasks,
                    starting_tasks = snapshot.starting_tasks,
                    stopping_tasks = snapshot.stopping_tasks,
                    orphaned_tasks = snapshot.orphaned_tasks,
                    runtime_slot_loads = ?snapshot.runtime_slot_loads,
                    zlm_alive = snapshot.zlm_alive,
                    ffmpeg_alive = snapshot.ffmpeg_alive,
                    "heartbeat updated"
                );
            }
            media_rpc::control_plane::agent_envelope::Payload::CapabilitySnapshot(snapshot) => {
                let snapshot = capability_from_rpc(snapshot);
                if !session_write_applied(
                    self.repository
                        .upsert_node_capabilities_for_session(node_id, session_id, &snapshot)
                        .await
                        .map_err(repo_status)?,
                )? {
                    return Ok(());
                }
                self.update_session_capabilities(node_id, session_id, &snapshot)
                    .await?;
                info!(
                    node_id = %node_id,
                    protocols = snapshot.ffmpeg_protocols.len(),
                    encoders = snapshot.ffmpeg_encoders.len(),
                    zlm_api = snapshot.zlm_api_list.len(),
                    "capability snapshot updated"
                );
            }
            media_rpc::control_plane::agent_envelope::Payload::TaskEvent(event) => {
                let event = parse_task_event(event)?;
                if !session_write_applied(
                    self.repository
                        .record_agent_task_event_for_session(node_id, session_id, event.clone())
                        .await
                        .map_err(repo_status)?,
                )? {
                    return Ok(());
                }
                if event.event_type == "adopted"
                    && self
                        .repository
                        .attempt_has_stop_intent(event.task_id, event.attempt_no)
                        .await
                        .map_err(repo_status)?
                {
                    // Agent 重连后可能收养了 Core 已经请求停止的进程，需要补发 stop 保持意图一致。
                    if let Err(error) = self
                        .request_stop(event.task_id, "reclaim_stop", 30, 5)
                        .await
                    {
                        warn!(
                            node_id = %node_id,
                            task_id = %event.task_id,
                            attempt_no = event.attempt_no,
                            error = %error,
                            "failed to resend stop after runtime adoption"
                        );
                    }
                }
                if event_releases_dispatch_reservation(&event.event_type) {
                    self.release_dispatch_reservation(node_id, Some(session_id), event.task_id)
                        .await;
                }
                info!(
                    node_id = %node_id,
                    task_id = %event.task_id,
                    attempt_no = event.attempt_no,
                    event_type = %event.event_type,
                    event_level = %event.event_level,
                    message = %event.message,
                    "task event received"
                );
            }
            media_rpc::control_plane::agent_envelope::Payload::TaskLogBatch(batch) => {
                let batch = parse_task_log_batch(batch)?;
                if !session_write_applied(
                    self.repository
                        .record_agent_log_batch_for_session(node_id, session_id, batch.clone())
                        .await
                        .map_err(repo_status)?,
                )? {
                    return Ok(());
                }
                debug!(
                    node_id = %node_id,
                    task_id = %batch.task_id,
                    attempt_no = batch.attempt_no,
                    stream = %batch.stream,
                    line_count = batch.lines.len(),
                    "task log batch received"
                );
            }
            media_rpc::control_plane::agent_envelope::Payload::TaskProgress(progress) => {
                let progress = parse_task_progress(progress)?;
                if !session_write_applied(
                    self.repository
                        .record_agent_progress_for_session(node_id, session_id, progress.clone())
                        .await
                        .map_err(repo_status)?,
                )? {
                    return Ok(());
                }
                debug!(
                    node_id = %node_id,
                    task_id = %progress.task_id,
                    attempt_no = progress.attempt_no,
                    frame = progress.frame,
                    fps = progress.fps,
                    speed = progress.speed,
                    out_time_ms = progress.out_time_ms,
                    "task progress received"
                );
            }
            media_rpc::control_plane::agent_envelope::Payload::TaskSnapshot(snapshot) => {
                let snapshot = parse_task_snapshot(snapshot)?;
                if !session_write_applied(
                    self.repository
                        .record_agent_snapshot_for_session(node_id, session_id, snapshot.clone())
                        .await
                        .map_err(repo_status)?,
                )? {
                    return Ok(());
                }
                if snapshot.state.eq_ignore_ascii_case("exited") {
                    self.release_dispatch_reservation(node_id, Some(session_id), snapshot.task_id)
                        .await;
                }
                info!(
                    node_id = %node_id,
                    task_id = %snapshot.task_id,
                    attempt_no = snapshot.attempt_no,
                    worker_kind = %snapshot.worker_kind,
                    pid = ?snapshot.pid,
                    state = %snapshot.state,
                    "task snapshot received"
                );
            }
            media_rpc::control_plane::agent_envelope::Payload::CertificateRotationRequest(
                request,
            ) => {
                self.handle_certificate_rotation_request(node_id, session_id, request)
                    .await?;
            }
            media_rpc::control_plane::agent_envelope::Payload::CertificateRotationActivated(
                acknowledgement,
            ) => {
                self.handle_certificate_rotation_activated(node_id, session_id, acknowledgement)
                    .await?;
            }
            media_rpc::control_plane::agent_envelope::Payload::ZlmDebugResponse(response) => {
                self.handle_zlm_debug_response(node_id, session_id, response)
                    .await?;
            }
            media_rpc::control_plane::agent_envelope::Payload::ZlmHookRequest(request) => {
                self.handle_zlm_hook_request(node_id, session_id, request)
                    .await?;
            }
        }

        Ok(())
    }

    async fn close_session(&self, node_id: Uuid, session_id: Uuid) {
        {
            let sessions = self.sessions.lock().await;
            if let Some(current) = sessions
                .get(&node_id)
                .filter(|current| current.session_id == session_id)
            {
                current.generation.cancel();
            }
        }
        self.cancel_zlm_waiters(node_id, session_id).await;
        self.zlm_hook_sessions
            .lock()
            .await
            .remove(&(node_id, session_id));
        let removed = {
            let mut sessions = self.sessions.lock().await;
            match sessions.get(&node_id) {
                Some(current) if current.session_id == session_id => sessions.remove(&node_id),
                _ => None,
            }
        };

        let Some(removed) = removed else {
            return;
        };
        self.pending_rotations
            .lock()
            .await
            .remove(&(node_id, session_id));

        let identity = &removed.identity;
        match self
            .repository
            .close_agent_control_session_and_reclaim(node_id, session_id, Utc::now())
            .await
        {
            Ok(true) => {
                debug!(
                    node_id = %node_id,
                    session_id = %session_id,
                    certificate_id = %identity.certificate_id,
                    certificate_fingerprint_sha256 = ?identity.fingerprint_sha256,
                    peer_ip = %identity.peer_ip,
                    connected_at = %identity.connected_at,
                    last_activity_at = %identity.last_activity_at,
                    "durable Agent control session closed"
                );
            }
            Ok(false) => {
                debug!(
                    node_id = %node_id,
                    session_id = %session_id,
                    "stale Agent control session close was fenced"
                );
            }
            Err(error) => {
                warn!(
                    node_id = %node_id,
                    session_id = %session_id,
                    error = %error,
                    "failed to close durable Agent control session"
                );
            }
        }

        // 控制流断开时任务不立即判失败，先进入 reclaiming，等待 Agent 重连或快照回补。
    }

    async fn note_session_activity(
        &self,
        node_id: Uuid,
        session_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<(), Status> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&node_id)
            .ok_or_else(|| Status::unavailable("control-plane session no longer exists"))?;
        if session.session_id != session_id {
            return Err(Status::permission_denied(
                "Agent control session is no longer current",
            ));
        }
        session.identity.last_activity_at = now;
        Ok(())
    }

    async fn update_session_load(
        &self,
        node_id: Uuid,
        session_id: Uuid,
        snapshot: &HeartbeatSnapshot,
    ) -> Result<(), Status> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&node_id)
            .ok_or_else(|| Status::unavailable("control-plane session no longer exists"))?;
        if session.session_id != session_id {
            return Err(Status::permission_denied(
                "Agent control session is no longer current",
            ));
        }
        // dispatch reservation 是乐观占位；心跳里真实活跃任务数增加后释放对应占位。
        for source_mode in [SourceMode::Live, SourceMode::Vod] {
            let previous_active = session_slot_load(&session.load, source_mode)
                .map(slot_load_occupied)
                .unwrap_or(0);
            let current_active = runtime_slot_load(&snapshot.runtime_slot_loads, source_mode)
                .map(slot_load_occupied)
                .unwrap_or(0);
            release_dispatch_reservations_for_source_mode(
                &mut session.reservations,
                source_mode,
                current_active.saturating_sub(previous_active),
            );
        }
        session.load = SessionLoad {
            running_tasks: snapshot.running_tasks,
            starting_tasks: snapshot.starting_tasks,
            stopping_tasks: snapshot.stopping_tasks,
            orphaned_tasks: snapshot.orphaned_tasks,
            runtime_slot_loads: snapshot.runtime_slot_loads.clone(),
            cpu_percent: snapshot.cpu_percent,
            mem_percent: snapshot.mem_percent,
            disk_percent: snapshot.disk_percent,
            upload_disk_total_bytes: snapshot.upload_disk_total_bytes,
            upload_disk_available_bytes: snapshot.upload_disk_available_bytes,
            upload_disk_used_percent: snapshot.upload_disk_used_percent,
            zlm_alive: snapshot.zlm_alive,
            ffmpeg_alive: snapshot.ffmpeg_alive,
            artifact_cleanup_blocked: snapshot.artifact_cleanup_blocked,
            gpu_runtime: snapshot.gpu_runtime.clone(),
        };
        Ok(())
    }

    async fn update_session_capabilities(
        &self,
        node_id: Uuid,
        session_id: Uuid,
        snapshot: &CapabilitySnapshot,
    ) -> Result<(), Status> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&node_id)
            .ok_or_else(|| Status::unavailable("control-plane session no longer exists"))?;
        if session.session_id != session_id {
            return Err(Status::permission_denied(
                "Agent control session is no longer current",
            ));
        }
        session.capabilities = SessionCapabilities {
            gpu_devices: snapshot.gpu_devices.clone(),
        };
        Ok(())
    }

    async fn current_session_generation(
        &self,
        node_id: Uuid,
        session_id: Uuid,
    ) -> Option<Arc<SessionGeneration>> {
        self.sessions
            .lock()
            .await
            .get(&node_id)
            .filter(|current| current.session_id == session_id)
            .map(|current| current.generation.clone())
    }

    async fn is_current_session(&self, node_id: Uuid, session_id: Uuid) -> bool {
        {
            let sessions = self.sessions.lock().await;
            match sessions.get(&node_id) {
                Some(current) if current.session_id == session_id => {}
                _ => return false,
            }
        }
        match self
            .repository
            .agent_control_session_is_current(node_id, session_id, Utc::now())
            .await
        {
            Ok(current) => current,
            Err(error) => {
                warn!(
                    node_id = %node_id,
                    session_id = %session_id,
                    error = %error,
                    "failed to validate durable Agent control session"
                );
                false
            }
        }
    }
    async fn claim_best_session(
        &self,
        source_affinity_ip: Option<IpAddr>,
        task_id: Uuid,
        spec: &TaskSpec,
        preference: ExecutionPreference,
    ) -> ClaimResult {
        let mut sessions = self.sessions.lock().await;
        // required_labels 要区分“没有匹配标签节点”和“有匹配节点但当前不可调度”两类错误。
        let has_required_label_match = task_has_required_labels(spec)
            && sessions
                .values()
                .any(|handle| node_matches_required_labels(spec, &handle.registration));
        let Some(target) =
            pick_best_session_target(&sessions, source_affinity_ip, spec, preference)
        else {
            return if task_has_required_labels(spec) && !has_required_label_match {
                ClaimResult::MissingRequiredLabels
            } else {
                ClaimResult::NoConnectedNode
            };
        };
        let Some(source_mode) = task_source_mode(spec) else {
            return ClaimResult::NoConnectedNode;
        };
        let Some(handle) = sessions.get_mut(&target.node_id) else {
            return ClaimResult::NoConnectedNode;
        };
        // 选中节点后立即写入 reservation，降低并发派发时多个任务挤到同一节点的概率。
        handle.reservations.push_back(DispatchReservation {
            task_id,
            source_mode,
        });
        let score = dispatch_score(
            target.node_id,
            &handle.registration,
            &handle.capabilities,
            &handle.load,
            source_affinity_ip,
            source_mode,
            reservation_count(handle, source_mode),
            target.using_gpu_path,
        );
        ClaimResult::Selected(SessionTarget {
            node_id: target.node_id,
            session_id: handle.session_id,
            sender: handle.sender.clone(),
            same_subnet: score.same_subnet,
            has_gpu_devices: !handle.capabilities.gpu_devices.is_empty(),
            using_gpu_path: target.using_gpu_path,
            gpu_headroom: score.gpu_headroom,
            slot_usage: score.slot_usage,
            occupied_tasks: score.occupied_tasks,
        })
    }

    async fn claim_session_by_node(
        &self,
        node_id: Uuid,
        source_affinity_ip: Option<IpAddr>,
        task_id: Uuid,
        spec: &TaskSpec,
        preference: ExecutionPreference,
    ) -> ClaimResult {
        let mut sessions = self.sessions.lock().await;
        let Some(handle) = sessions.get_mut(&node_id) else {
            return ClaimResult::NoConnectedNode;
        };
        if !node_matches_required_labels(spec, &handle.registration) {
            return ClaimResult::MissingRequiredLabels;
        }
        let Some(source_mode) = task_source_mode(spec) else {
            return ClaimResult::NoConnectedNode;
        };
        let reservations = reservation_count(handle, source_mode);
        if !session_execution_eligible(
            spec,
            preference,
            &handle.capabilities,
            &handle.load,
            reservations,
        ) {
            return ClaimResult::NoConnectedNode;
        }
        handle.reservations.push_back(DispatchReservation {
            task_id,
            source_mode,
        });
        let score = dispatch_score(
            node_id,
            &handle.registration,
            &handle.capabilities,
            &handle.load,
            source_affinity_ip,
            source_mode,
            reservation_count(handle, source_mode),
            false,
        );
        ClaimResult::Selected(SessionTarget {
            node_id,
            session_id: handle.session_id,
            sender: handle.sender.clone(),
            same_subnet: score.same_subnet,
            has_gpu_devices: !handle.capabilities.gpu_devices.is_empty(),
            using_gpu_path: false,
            gpu_headroom: score.gpu_headroom,
            slot_usage: score.slot_usage,
            occupied_tasks: score.occupied_tasks,
        })
    }

    async fn release_dispatch_reservation(
        &self,
        node_id: Uuid,
        session_id: Option<Uuid>,
        task_id: Uuid,
    ) {
        let mut sessions = self.sessions.lock().await;
        let Some(session) = sessions.get_mut(&node_id) else {
            return;
        };
        if session_id.is_some_and(|expected| expected != session.session_id) {
            return;
        }
        if let Some(index) = session
            .reservations
            .iter()
            .position(|reservation| reservation.task_id == task_id)
        {
            session.reservations.remove(index);
        }
    }

    async fn session_for_node(&self, node_id: Uuid) -> Option<SessionTarget> {
        let sessions = self.sessions.lock().await;
        sessions.get(&node_id).map(|handle| SessionTarget {
            node_id,
            session_id: handle.session_id,
            sender: handle.sender.clone(),
            same_subnet: false,
            has_gpu_devices: !handle.capabilities.gpu_devices.is_empty(),
            using_gpu_path: false,
            gpu_headroom: None,
            slot_usage: max_runtime_slot_usage(&handle.load),
            occupied_tasks: occupied_tasks(&handle.load),
        })
    }

    pub async fn current_node_loads(&self) -> HashMap<Uuid, NodeLiveLoad> {
        let sessions = self.sessions.lock().await;
        sessions
            .iter()
            .map(|(node_id, handle)| {
                (
                    *node_id,
                    NodeLiveLoad {
                        connected: true,
                        running_tasks: handle.load.running_tasks,
                        starting_tasks: handle.load.starting_tasks,
                        stopping_tasks: handle.load.stopping_tasks,
                        orphaned_tasks: handle.load.orphaned_tasks,
                        runtime_slot_loads: handle.load.runtime_slot_loads.clone(),
                        cpu_percent: handle.load.cpu_percent,
                        mem_percent: handle.load.mem_percent,
                        disk_percent: handle.load.disk_percent,
                        upload_disk_total_bytes: handle.load.upload_disk_total_bytes,
                        upload_disk_available_bytes: handle.load.upload_disk_available_bytes,
                        upload_disk_used_percent: handle.load.upload_disk_used_percent,
                        zlm_alive: handle.load.zlm_alive,
                        ffmpeg_alive: handle.load.ffmpeg_alive,
                        artifact_cleanup_blocked: handle.load.artifact_cleanup_blocked,
                        gpu_runtime: handle.load.gpu_runtime.clone(),
                    },
                )
            })
            .collect()
    }
}

impl AgentManagementTargetProvider for ControlPlaneService {
    fn target(
        &self,
        node_id: Uuid,
    ) -> AgentManagementFuture<'_, Result<AuthenticatedAgentManagementTarget, AgentManagementError>>
    {
        Box::pin(async move { self.authenticated_management_target(node_id).await })
    }

    fn begin_request_fence<'a>(
        &'a self,
        target: &'a AuthenticatedAgentManagementTarget,
    ) -> AgentManagementFuture<'a, Result<Box<dyn AgentManagementSessionFence>, AgentManagementError>>
    {
        Box::pin(async move {
            let fence = self
                .repository
                .begin_agent_control_session_fence(
                    target.node_id(),
                    target.session_id(),
                    Utc::now(),
                )
                .await
                .map_err(|_| AgentManagementError::TargetUnavailable)?
                .ok_or(AgentManagementError::SessionFenced)?;
            Ok(Box::new(fence) as Box<dyn AgentManagementSessionFence>)
        })
    }
}

impl AgentManagementSessionFence for sqlx::Transaction<'static, sqlx::Postgres> {
    fn release(
        self: Box<Self>,
    ) -> AgentManagementFuture<'static, Result<(), AgentManagementError>> {
        Box::pin(async move {
            (*self)
                .commit()
                .await
                .map_err(|_| AgentManagementError::TargetUnavailable)
        })
    }
}

fn lowercase_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[tonic::async_trait]
impl ControlPlane for ControlPlaneService {
    type StreamConnectStream = ReceiverStream<Result<CoreEnvelope, Status>>;

    async fn stream_connect(
        &self,
        request: Request<Streaming<AgentEnvelope>>,
    ) -> Result<Response<Self::StreamConnectStream>, Status> {
        let peer_ip = request
            .remote_addr()
            .map(|address| address.ip())
            .ok_or_else(|| Status::unauthenticated("Agent peer address is unavailable"))?;
        let peer_certificates = request
            .peer_certs()
            .filter(|certificates| !certificates.is_empty())
            .ok_or_else(|| Status::unauthenticated("Agent client certificate is required"))?;
        let attempted_at = Utc::now();
        if peer_certificates.len() != 1 {
            let leaf_certificate = peer_certificates[0].as_ref();
            let certificate_node_id =
                parse_authenticated_agent_peer(leaf_certificate, peer_ip, attempted_at)
                    .ok()
                    .map(|peer| peer.node_id);
            if let Err(audit_error) = self
                .repository
                .record_agent_peer_rejection(
                    certificate_node_id,
                    None,
                    agent_certificate_fingerprint_sha256(leaf_certificate),
                    peer_ip,
                    "unexpected_certificate_chain",
                    attempted_at,
                )
                .await
            {
                warn!(
                    peer_ip = %peer_ip,
                    certificate_count = peer_certificates.len(),
                    error = %audit_error,
                    "failed to audit rejected Agent certificate chain"
                );
            }
            return Err(Status::unauthenticated(
                "Agent must present exactly one directly issued client certificate",
            ));
        }
        let leaf_certificate = peer_certificates[0].as_ref();
        let peer = match parse_authenticated_agent_peer(leaf_certificate, peer_ip, attempted_at) {
            Ok(peer) => peer,
            Err(error) => {
                if let Err(audit_error) = self
                    .repository
                    .record_agent_peer_rejection(
                        None,
                        None,
                        agent_certificate_fingerprint_sha256(leaf_certificate),
                        peer_ip,
                        agent_peer_rejection_reason(error),
                        attempted_at,
                    )
                    .await
                {
                    warn!(
                        peer_ip = %peer_ip,
                        error = %audit_error,
                        "failed to audit rejected Agent peer certificate"
                    );
                }
                return Err(Status::unauthenticated(
                    "Agent client certificate is invalid",
                ));
            }
        };
        let mut inbound = request.into_inner();
        // 双向流的第一包必须是注册信息，Core 依赖它建立 node_id 到 sender 的会话映射。
        let first = match timeout(CONTROL_STREAM_IDLE_TIMEOUT, inbound.message()).await {
            Ok(Ok(Some(message))) => message,
            Ok(Ok(None)) => return Err(Status::invalid_argument("missing register message")),
            Ok(Err(error)) => return Err(Status::unavailable(error.to_string())),
            Err(_) => return Err(Status::deadline_exceeded("timed out waiting for register")),
        };

        let Some(media_rpc::control_plane::agent_envelope::Payload::Register(register)) =
            first.payload
        else {
            return Err(Status::invalid_argument(
                "the first AgentEnvelope payload must be register",
            ));
        };

        let AuthenticatedAgentRegistration {
            registration,
            management_port,
            management_upload_max_bytes,
        } = authenticated_registration_from_rpc(register)?;
        if registration.node_id != peer.node_id {
            if let Err(error) = self
                .repository
                .record_agent_peer_rejection(
                    Some(peer.node_id),
                    Some(registration.node_id),
                    peer.fingerprint_sha256,
                    peer.peer_ip,
                    "registration_node_mismatch",
                    Utc::now(),
                )
                .await
            {
                warn!(
                    certificate_node_id = %peer.node_id,
                    claimed_node_id = %registration.node_id,
                    peer_ip = %peer.peer_ip,
                    error = %error,
                    "failed to audit Agent registration identity mismatch"
                );
            }
            return Err(Status::permission_denied(
                "Agent certificate identity does not match registration",
            ));
        }
        let (sender, receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
        let session_id = self
            .bootstrap_session_with_management(
                &registration,
                &peer,
                management_port,
                management_upload_max_bytes,
                sender.clone(),
            )
            .await?;

        let service = self.clone();
        tokio::spawn(async move {
            service
                .process_stream(peer.node_id, session_id, inbound)
                .await;
        });

        Ok(Response::new(ReceiverStream::new(receiver)))
    }
}

fn repo_status(error: RepoError) -> Status {
    Status::internal(error.to_string())
}

fn agent_rotation_status(error: AgentIdentityServiceError) -> Status {
    match error {
        AgentIdentityServiceError::InvalidCsr => Status::invalid_argument("Agent CSR is invalid"),
        AgentIdentityServiceError::InvalidRotation
        | AgentIdentityServiceError::RotationExpired
        | AgentIdentityServiceError::InvalidEnrollment
        | AgentIdentityServiceError::IdentityAlreadyActive
        | AgentIdentityServiceError::IdentityRevoked => {
            Status::failed_precondition("Agent certificate rotation is not authorized")
        }
        AgentIdentityServiceError::CertificateSigning => {
            Status::internal("Agent certificate signing failed")
        }
        AgentIdentityServiceError::Repository(error) => repo_status(error),
    }
}

fn rotation_bundle_to_rpc(bundle: CompletedAgentCertificateRotation) -> CertificateRotationBundle {
    CertificateRotationBundle {
        rotation_id: bundle.rotation_id.to_string(),
        expires_at_ms: bundle.expires_at.timestamp_millis(),
        control_certificate_pem: bundle.control_certificate_pem,
        control_fingerprint_sha256: bundle.control_fingerprint_sha256,
        control_serial_number: bundle.control_serial_number,
        control_not_before_ms: bundle.control_not_before.timestamp_millis(),
        control_not_after_ms: bundle.control_not_after.timestamp_millis(),
        management_certificate_pem: bundle.management_certificate_pem,
        management_fingerprint_sha256: bundle.management_fingerprint_sha256,
        management_serial_number: bundle.management_serial_number,
        management_not_before_ms: bundle.management_not_before.timestamp_millis(),
        management_not_after_ms: bundle.management_not_after.timestamp_millis(),
        agent_client_issuer_ca_pem: bundle.agent_client_issuer_ca_pem,
        control_plane_server_ca_pem: bundle.control_plane_server_ca_pem,
        management_client_ca_pem: bundle.management_client_ca_pem,
        capability_jwt_public_key_pem: bundle.capability_jwt_public_key_pem,
        capability_jwt_kid: bundle.capability_jwt_kid,
    }
}

fn parse_canonical_uuid(field: &'static str, value: &str) -> Result<Uuid, Status> {
    let parsed = Uuid::parse_str(value)
        .map_err(|_| Status::invalid_argument(format!("{field} must be a canonical UUID")))?;
    if parsed.is_nil() || parsed.to_string() != value {
        return Err(Status::invalid_argument(format!(
            "{field} must be a canonical non-nil UUID"
        )));
    }
    Ok(parsed)
}

fn parse_lowercase_sha256(field: &'static str, value: &str) -> Result<[u8; 32], Status> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(Status::invalid_argument(format!(
            "{field} must be lowercase SHA-256 hex"
        )));
    }
    let mut output = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_nibble(pair[0]).ok_or_else(|| {
            Status::invalid_argument(format!("{field} must be lowercase SHA-256 hex"))
        })?;
        let low = hex_nibble(pair[1]).ok_or_else(|| {
            Status::invalid_argument(format!("{field} must be lowercase SHA-256 hex"))
        })?;
        output[index] = (high << 4) | low;
    }
    Ok(output)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn agent_peer_rejection_reason(error: AgentPeerCertificateError) -> &'static str {
    match error {
        AgentPeerCertificateError::Invalid => "malformed_leaf_certificate",
        AgentPeerCertificateError::NotCurrentlyValid => "certificate_not_currently_valid",
        AgentPeerCertificateError::InvalidIdentity => "invalid_spiffe_identity",
        AgentPeerCertificateError::InvalidUsage => "invalid_certificate_usage",
    }
}

#[allow(clippy::result_large_err)]
fn session_write_applied(outcome: AgentSessionWriteOutcome) -> Result<bool, Status> {
    match outcome {
        AgentSessionWriteOutcome::Applied => Ok(true),
        AgentSessionWriteOutcome::IgnoredStaleAttempt => Ok(false),
        AgentSessionWriteOutcome::FencedSession => Err(Status::permission_denied(
            "Agent control session is no longer current",
        )),
    }
}

#[derive(Debug, Clone)]
struct AuthenticatedAgentRegistration {
    registration: AgentRegistration,
    management_port: u16,
    management_upload_max_bytes: u64,
}

fn authenticated_registration_from_rpc(
    register: RpcRegister,
) -> Result<AuthenticatedAgentRegistration, Status> {
    let node_id = Uuid::parse_str(register.node_id.trim())
        .map_err(|error| Status::invalid_argument(format!("invalid node_id: {error}")))?;
    let network_mode = register
        .network_mode
        .parse::<NetworkMode>()
        .map_err(|error| Status::invalid_argument(error.to_string()))?;
    let zlm_rtmp_port = u16::try_from(register.zlm_rtmp_port)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| Status::invalid_argument("invalid zlm_rtmp_port"))?;
    let zlm_rtsp_port = u16::try_from(register.zlm_rtsp_port)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| Status::invalid_argument("invalid zlm_rtsp_port"))?;
    let management_port = u16::try_from(register.management_port)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| Status::invalid_argument("invalid management_port"))?;
    if register.management_upload_max_bytes == 0 {
        return Err(Status::invalid_argument(
            "invalid management_upload_max_bytes",
        ));
    }

    let registration = AgentRegistration {
        node_id,
        node_name: require_field("node_name", register.node_name)?,
        agent_version: require_field("agent_version", register.agent_version)?,
        hostname: require_field("hostname", register.hostname)?,
        labels: normalize_strings(register.labels),
        interfaces: normalize_strings(register.interfaces),
        // Legacy wire fields 7/8 are attacker-controlled and no longer form
        // any Core-side management target or credential.
        zlm_api_base: String::new(),
        zlm_api_secret: String::new(),
        agent_stream_addr: require_field("agent_stream_addr", register.agent_stream_addr)?,
        // Legacy wire field 18 is replaced by the certificate-bound
        // management endpoint and must never be persisted or consumed.
        agent_http_base_url: String::new(),
        zlm_rtmp_port,
        zlm_rtsp_port,
        network_mode,
        ffmpeg_bin: require_field("ffmpeg_bin", register.ffmpeg_bin)?,
        ffprobe_bin: require_field("ffprobe_bin", register.ffprobe_bin)?,
        zlm_server_id: require_field("zlm_server_id", register.zlm_server_id)?,
        output_mount_relative_prefix_mp4: normalize_output_mount_relative_prefix(
            &register.output_mount_relative_prefix_mp4,
        )
        .map_err(|error| {
            Status::invalid_argument(format!("invalid output_mount_relative_prefix_mp4: {error}"))
        })?,
        output_mount_relative_prefix_hls: normalize_output_mount_relative_prefix(
            &register.output_mount_relative_prefix_hls,
        )
        .map_err(|error| {
            Status::invalid_argument(format!("invalid output_mount_relative_prefix_hls: {error}"))
        })?,
    };
    Ok(AuthenticatedAgentRegistration {
        registration,
        management_port,
        management_upload_max_bytes: register.management_upload_max_bytes,
    })
}

fn heartbeat_from_rpc(heartbeat: RpcHeartbeat) -> Result<HeartbeatSnapshot, Status> {
    let node_time = DateTime::<Utc>::from_timestamp_millis(heartbeat.node_time_ms)
        .ok_or_else(|| Status::invalid_argument("invalid node_time_ms"))?;

    Ok(HeartbeatSnapshot {
        node_time,
        cpu_percent: heartbeat.cpu_percent,
        mem_percent: heartbeat.mem_percent,
        disk_percent: heartbeat.disk_percent,
        upload_disk_total_bytes: heartbeat.upload_disk_total_bytes,
        upload_disk_available_bytes: heartbeat.upload_disk_available_bytes,
        upload_disk_used_percent: heartbeat.upload_disk_used_percent,
        running_tasks: heartbeat.running_tasks,
        starting_tasks: heartbeat.starting_tasks,
        stopping_tasks: heartbeat.stopping_tasks,
        orphaned_tasks: heartbeat.orphaned_tasks,
        runtime_slot_loads: heartbeat
            .runtime_slot_loads
            .into_iter()
            .map(runtime_slot_load_from_rpc)
            .collect::<Result<Vec<_>, _>>()?,
        zlm_alive: heartbeat.zlm_alive,
        ffmpeg_alive: heartbeat.ffmpeg_alive,
        artifact_cleanup_blocked: heartbeat.artifact_cleanup_blocked,
        artifact_cleanup_block_reason: (!heartbeat.artifact_cleanup_block_reason.trim().is_empty())
            .then_some(heartbeat.artifact_cleanup_block_reason),
        gpu_runtime: heartbeat
            .gpu_runtime
            .into_iter()
            .map(gpu_runtime_from_rpc)
            .collect(),
    })
}

fn capability_from_rpc(snapshot: RpcCapabilitySnapshot) -> CapabilitySnapshot {
    CapabilitySnapshot {
        ffmpeg_protocols: normalize_strings(snapshot.ffmpeg_protocols),
        ffmpeg_formats: normalize_strings(snapshot.ffmpeg_formats),
        ffmpeg_encoders: normalize_strings(snapshot.ffmpeg_encoders),
        ffmpeg_decoders: normalize_strings(snapshot.ffmpeg_decoders),
        zlm_version: option_string(snapshot.zlm_version),
        zlm_api_list: normalize_strings(snapshot.zlm_api_list),
        gpu: normalize_strings(snapshot.gpu),
        gpu_devices: snapshot
            .gpu_devices
            .into_iter()
            .map(gpu_device_from_rpc)
            .collect(),
        captured_at: Utc::now(),
    }
}

fn gpu_device_from_rpc(device: RpcGpuDevice) -> GpuDeviceInfo {
    GpuDeviceInfo {
        index: device.index,
        uuid: device.uuid.trim().to_string(),
        name: device.name.trim().to_string(),
        memory_total_mb: device.memory_total_mb,
    }
}

fn gpu_runtime_from_rpc(runtime: RpcGpuRuntime) -> GpuRuntimeStats {
    GpuRuntimeStats {
        index: runtime.index,
        gpu_util_percent: runtime.gpu_util_percent,
        memory_used_mb: runtime.memory_used_mb,
        memory_total_mb: runtime.memory_total_mb,
        encoder_util_percent: runtime.encoder_util_percent,
        decoder_util_percent: runtime.decoder_util_percent,
    }
}

fn runtime_slot_load_from_rpc(load: RpcRuntimeSlotLoad) -> Result<RuntimeSlotLoad, Status> {
    let source_mode = match load.source_mode.trim() {
        "live" => SourceMode::Live,
        "vod" => SourceMode::Vod,
        value => {
            return Err(Status::invalid_argument(format!(
                "invalid runtime_slot_loads.source_mode: {value}"
            )));
        }
    };
    Ok(RuntimeSlotLoad {
        source_mode,
        max_runtime_slots: load.max_runtime_slots,
        running_tasks: load.running_tasks,
        starting_tasks: load.starting_tasks,
        stopping_tasks: load.stopping_tasks,
        orphaned_tasks: load.orphaned_tasks,
        slot_usage: normalized_slot_usage(load.slot_usage),
    })
}

fn parse_task_event(event: TaskEvent) -> Result<AgentTaskEventRecord, Status> {
    Ok(AgentTaskEventRecord {
        task_id: parse_uuid("task_id", &event.task_id)?,
        attempt_no: event.attempt_no,
        lease_token: require_string("lease_token", event.lease_token)?,
        event_type: require_string("event_type", event.event_type)?,
        event_level: require_string("event_level", event.event_level)?,
        message: event.message,
        payload: parse_json("payload_json", &event.payload_json)?,
    })
}

fn parse_task_log_batch(batch: TaskLogBatch) -> Result<TaskLogBatchRecord, Status> {
    Ok(TaskLogBatchRecord {
        task_id: parse_uuid("task_id", &batch.task_id)?,
        attempt_no: batch.attempt_no,
        lease_token: require_string("lease_token", batch.lease_token)?,
        stream: require_string("stream", batch.stream)?,
        lines: batch.lines,
    })
}

fn parse_task_progress(progress: TaskProgress) -> Result<TaskProgressRecord, Status> {
    Ok(TaskProgressRecord {
        task_id: parse_uuid("task_id", &progress.task_id)?,
        attempt_no: progress.attempt_no,
        lease_token: require_string("lease_token", progress.lease_token)?,
        frame: progress.frame,
        fps: progress.fps,
        bitrate_kbps: progress.bitrate_kbps,
        speed: progress.speed,
        out_time_ms: progress.out_time_ms,
        dup_frames: progress.dup_frames,
        drop_frames: progress.drop_frames,
    })
}

fn parse_task_snapshot(snapshot: TaskSnapshot) -> Result<TaskSnapshotRecord, Status> {
    Ok(TaskSnapshotRecord {
        runtime_id: parse_uuid("runtime_id", &snapshot.runtime_id)?,
        task_id: parse_uuid("task_id", &snapshot.task_id)?,
        attempt_no: snapshot.attempt_no,
        lease_token: require_string("lease_token", snapshot.lease_token)?,
        worker_kind: require_string("worker_kind", snapshot.worker_kind)?,
        pid: (snapshot.pid > 0).then_some(snapshot.pid),
        state: require_string("state", snapshot.state)?,
        command_line: option_string(snapshot.command_line),
        outputs: snapshot.outputs,
        metadata: parse_json("metadata_json", &snapshot.metadata_json)?,
    })
}

fn require_field(name: &'static str, value: String) -> Result<String, Status> {
    let value = value.trim().to_string();
    if value.is_empty() {
        Err(Status::invalid_argument(format!(
            "{name} must not be empty"
        )))
    } else {
        Ok(value)
    }
}

fn require_string(name: &'static str, value: String) -> Result<String, Status> {
    let value = value.trim().to_string();
    if value.is_empty() {
        Err(Status::invalid_argument(format!(
            "{name} must not be empty"
        )))
    } else {
        Ok(value)
    }
}

fn option_string(value: String) -> Option<String> {
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn parse_uuid(name: &'static str, value: &str) -> Result<Uuid, Status> {
    Uuid::parse_str(value.trim())
        .map_err(|error| Status::invalid_argument(format!("invalid {name}: {error}")))
}

fn parse_json(name: &'static str, value: &str) -> Result<Value, Status> {
    if value.trim().is_empty() {
        Ok(Value::Null)
    } else {
        serde_json::from_str(value)
            .map_err(|error| Status::invalid_argument(format!("invalid {name}: {error}")))
    }
}

fn normalize_strings(values: Vec<String>) -> Vec<String> {
    let mut values = values
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
}

fn task_source_affinity_ip(spec: &TaskSpec) -> Option<IpAddr> {
    match spec.input.kind {
        Some(
            InputKind::Rtsp
            | InputKind::Rtmp
            | InputKind::Hls
            | InputKind::HttpFlv
            | InputKind::HttpTs,
        ) => spec
            .input
            .url
            .as_deref()
            .and_then(parse_url_host_ip_literal),
        _ => None,
    }
}

fn task_uploaded_file_affinity_node(spec: &TaskSpec) -> Option<Uuid> {
    if spec.input.kind != Some(InputKind::File) {
        return None;
    }
    crate::upload::uploaded_file_node_id(spec.input.url.as_deref()?)
}

fn parse_ip_literal(value: &str) -> Option<IpAddr> {
    value.trim().parse().ok()
}

fn parse_url_host_ip_literal(value: &str) -> Option<IpAddr> {
    Url::parse(value.trim())
        .ok()?
        .host_str()
        .and_then(parse_ip_literal)
}

fn reservation_count(handle: &SessionHandle, source_mode: SourceMode) -> u32 {
    u32::try_from(
        handle
            .reservations
            .iter()
            .filter(|reservation| reservation.source_mode == source_mode)
            .count(),
    )
    .unwrap_or(u32::MAX)
}

fn task_execution_preference(spec: &TaskSpec) -> ExecutionPreference {
    let _ = spec;
    ExecutionPreference::CpuOnly
}

fn task_source_mode(spec: &TaskSpec) -> Option<SourceMode> {
    spec.input.source_mode
}

fn slot_load_occupied(load: &RuntimeSlotLoad) -> u32 {
    load.running_tasks
        .saturating_add(load.starting_tasks)
        .saturating_add(load.stopping_tasks)
        .saturating_add(load.orphaned_tasks)
}

fn runtime_slot_load(
    loads: &[RuntimeSlotLoad],
    source_mode: SourceMode,
) -> Option<&RuntimeSlotLoad> {
    loads.iter().find(|load| load.source_mode == source_mode)
}

fn session_slot_load(load: &SessionLoad, source_mode: SourceMode) -> Option<&RuntimeSlotLoad> {
    runtime_slot_load(&load.runtime_slot_loads, source_mode)
}

fn effective_occupied_tasks(
    load: &SessionLoad,
    source_mode: SourceMode,
    reserved_dispatches: u32,
) -> u32 {
    session_slot_load(load, source_mode)
        .map(slot_load_occupied)
        .unwrap_or(0)
        .saturating_add(reserved_dispatches)
}

fn effective_slot_usage(
    load: &SessionLoad,
    source_mode: SourceMode,
    reserved_dispatches: u32,
) -> f64 {
    let Some(slot_load) = session_slot_load(load, source_mode) else {
        return 1.0;
    };
    if slot_load.max_runtime_slots == 0 {
        return 0.0;
    }
    (effective_occupied_tasks(load, source_mode, reserved_dispatches) as f64
        / slot_load.max_runtime_slots as f64)
        .clamp(0.0, 1.0)
}

fn session_is_saturated(
    load: &SessionLoad,
    source_mode: SourceMode,
    reserved_dispatches: u32,
) -> bool {
    let slot_usage = effective_slot_usage(load, source_mode, reserved_dispatches);
    slot_usage.is_finite() && slot_usage >= 1.0
}

fn occupied_tasks(load: &SessionLoad) -> u32 {
    load.running_tasks
        .saturating_add(load.starting_tasks)
        .saturating_add(load.stopping_tasks)
        .saturating_add(load.orphaned_tasks)
}

fn max_runtime_slot_usage(load: &SessionLoad) -> f64 {
    load.runtime_slot_loads
        .iter()
        .map(|slot_load| normalized_slot_usage(slot_load.slot_usage))
        .fold(0.0, f64::max)
}

fn release_dispatch_reservations_for_source_mode(
    reservations: &mut VecDeque<DispatchReservation>,
    source_mode: SourceMode,
    mut count: u32,
) {
    while count > 0 {
        let Some(index) = reservations
            .iter()
            .position(|reservation| reservation.source_mode == source_mode)
        else {
            return;
        };
        reservations.remove(index);
        count -= 1;
    }
}

fn task_requires_zlm(spec: &TaskSpec) -> bool {
    match spec.task_type {
        TaskType::StreamIngest => true,
        TaskType::StreamBridge | TaskType::FileTranscode => false,
    }
}

fn task_keeps_retry_node_affinity(spec: &TaskSpec) -> bool {
    matches!(
        spec.task_type,
        TaskType::StreamIngest | TaskType::StreamBridge
    )
}

fn base_execution_eligible(spec: &TaskSpec, load: &SessionLoad, reserved_dispatches: u32) -> bool {
    if load.artifact_cleanup_blocked {
        return false;
    }
    let Some(source_mode) = task_source_mode(spec) else {
        return false;
    };
    if session_is_saturated(load, source_mode, reserved_dispatches) || !load.ffmpeg_alive {
        return false;
    }
    !task_requires_zlm(spec) || load.zlm_alive
}

fn session_execution_eligible(
    spec: &TaskSpec,
    preference: ExecutionPreference,
    _capabilities: &SessionCapabilities,
    load: &SessionLoad,
    reserved_dispatches: u32,
) -> bool {
    match preference {
        ExecutionPreference::CpuOnly => base_execution_eligible(spec, load, reserved_dispatches),
    }
}

fn node_matches_required_labels(spec: &TaskSpec, registration: &AgentRegistration) -> bool {
    spec.resource
        .required_labels
        .iter()
        .map(|label| label.trim())
        .filter(|label| !label.is_empty())
        .all(|required| registration.labels.iter().any(|label| label == required))
}

fn task_has_required_labels(spec: &TaskSpec) -> bool {
    spec.resource
        .required_labels
        .iter()
        .any(|label| !label.trim().is_empty())
}

fn best_gpu_headroom(runtime: &[GpuRuntimeStats]) -> Option<f64> {
    runtime
        .iter()
        .filter_map(gpu_runtime_headroom)
        .max_by(|left, right| left.partial_cmp(right).unwrap_or(CmpOrdering::Equal))
}

fn gpu_runtime_headroom(runtime: &GpuRuntimeStats) -> Option<f64> {
    let memory_util = if runtime.memory_total_mb == 0 {
        100.0
    } else {
        (runtime.memory_used_mb as f64 / runtime.memory_total_mb as f64) * 100.0
    };
    let hottest = runtime
        .gpu_util_percent
        .max(memory_util)
        .max(runtime.encoder_util_percent)
        .max(runtime.decoder_util_percent);
    if hottest >= 95.0 {
        return None;
    }
    Some((100.0 - hottest).max(0.0))
}

fn event_releases_dispatch_reservation(event_type: &str) -> bool {
    matches!(
        event_type,
        "start_rejected" | "succeeded" | "failed" | "canceled"
    )
}

fn pick_best_session_target(
    sessions: &HashMap<Uuid, SessionHandle>,
    source_affinity_ip: Option<IpAddr>,
    spec: &TaskSpec,
    preference: ExecutionPreference,
) -> Option<SessionTarget> {
    let source_mode = task_source_mode(spec)?;
    let select = |gpu_only: bool| {
        sessions
            .iter()
            .filter(|(_, handle)| {
                if !node_matches_required_labels(spec, &handle.registration) {
                    return false;
                }
                let reservations = reservation_count(handle, source_mode);
                let _ = gpu_only;
                session_execution_eligible(
                    spec,
                    preference,
                    &handle.capabilities,
                    &handle.load,
                    reservations,
                )
            })
            .min_by(|(left_id, left_handle), (right_id, right_handle)| {
                compare_dispatch_score(
                    dispatch_score(
                        **left_id,
                        &left_handle.registration,
                        &left_handle.capabilities,
                        &left_handle.load,
                        source_affinity_ip,
                        source_mode,
                        reservation_count(left_handle, source_mode),
                        gpu_only,
                    ),
                    dispatch_score(
                        **right_id,
                        &right_handle.registration,
                        &right_handle.capabilities,
                        &right_handle.load,
                        source_affinity_ip,
                        source_mode,
                        reservation_count(right_handle, source_mode),
                        gpu_only,
                    ),
                )
            })
            .map(|(node_id, handle)| {
                let score = dispatch_score(
                    *node_id,
                    &handle.registration,
                    &handle.capabilities,
                    &handle.load,
                    source_affinity_ip,
                    source_mode,
                    reservation_count(handle, source_mode),
                    gpu_only,
                );
                SessionTarget {
                    node_id: *node_id,
                    session_id: handle.session_id,
                    sender: handle.sender.clone(),
                    same_subnet: score.same_subnet,
                    has_gpu_devices: !handle.capabilities.gpu_devices.is_empty(),
                    using_gpu_path: gpu_only,
                    gpu_headroom: score.gpu_headroom,
                    slot_usage: score.slot_usage,
                    occupied_tasks: score.occupied_tasks,
                }
            })
    };

    match preference {
        ExecutionPreference::CpuOnly => select(false),
    }
}

#[allow(clippy::too_many_arguments)]
fn dispatch_score(
    node_id: Uuid,
    registration: &AgentRegistration,
    capabilities: &SessionCapabilities,
    load: &SessionLoad,
    source_affinity_ip: Option<IpAddr>,
    source_mode: SourceMode,
    reserved_dispatches: u32,
    prefer_gpu_headroom: bool,
) -> DispatchScore {
    DispatchScore {
        same_subnet: source_affinity_ip
            .is_some_and(|source_ip| node_has_same_subnet(registration, source_ip)),
        gpu_headroom: (prefer_gpu_headroom && !capabilities.gpu_devices.is_empty())
            .then(|| best_gpu_headroom(&load.gpu_runtime))
            .flatten(),
        slot_usage: effective_slot_usage(load, source_mode, reserved_dispatches),
        occupied_tasks: effective_occupied_tasks(load, source_mode, reserved_dispatches),
        node_id,
    }
}

fn compare_dispatch_score(left: DispatchScore, right: DispatchScore) -> CmpOrdering {
    right
        .same_subnet
        .cmp(&left.same_subnet)
        .then_with(|| compare_gpu_headroom(left.gpu_headroom, right.gpu_headroom))
        .then_with(|| compare_slot_usage(left.slot_usage, right.slot_usage))
        .then_with(|| left.occupied_tasks.cmp(&right.occupied_tasks))
        .then_with(|| left.node_id.cmp(&right.node_id))
}

fn compare_gpu_headroom(left: Option<f64>, right: Option<f64>) -> CmpOrdering {
    match (left, right) {
        (Some(left), Some(right)) => right.partial_cmp(&left).unwrap_or(CmpOrdering::Equal),
        (Some(_), None) => CmpOrdering::Less,
        (None, Some(_)) => CmpOrdering::Greater,
        (None, None) => CmpOrdering::Equal,
    }
}

fn compare_slot_usage(left: f64, right: f64) -> CmpOrdering {
    normalized_slot_usage(left)
        .partial_cmp(&normalized_slot_usage(right))
        .unwrap_or(CmpOrdering::Equal)
}

fn normalized_slot_usage(value: f64) -> f64 {
    if value.is_finite() && value >= 0.0 {
        value
    } else {
        f64::INFINITY
    }
}

fn node_has_same_subnet(registration: &AgentRegistration, source_ip: IpAddr) -> bool {
    registration
        .interfaces
        .iter()
        .filter_map(|interface| parse_interface_network(interface))
        .any(|network| same_subnet(network.ip, source_ip, network.prefix))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InterfaceNetwork {
    ip: IpAddr,
    prefix: u8,
}

fn parse_interface_network(value: &str) -> Option<InterfaceNetwork> {
    let cidr = value
        .trim()
        .rsplit_once('|')
        .map(|(_, cidr)| cidr)
        .unwrap_or(value.trim());
    let (ip, prefix) = cidr.split_once('/')?;
    let ip = parse_ip_literal(ip)?;
    let prefix = prefix.trim().parse::<u8>().ok()?;

    match ip {
        IpAddr::V4(_) if prefix <= 32 => Some(InterfaceNetwork { ip, prefix }),
        IpAddr::V6(_) if prefix <= 128 => Some(InterfaceNetwork { ip, prefix }),
        _ => None,
    }
}

fn same_subnet(left: IpAddr, right: IpAddr, prefix: u8) -> bool {
    match (left, right) {
        (IpAddr::V4(left), IpAddr::V4(right)) if prefix <= 32 => {
            let mask = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - u32::from(prefix))
            };
            (u32::from(left) & mask) == (u32::from(right) & mask)
        }
        (IpAddr::V6(left), IpAddr::V6(right)) if prefix <= 128 => {
            let mask = if prefix == 0 {
                0
            } else {
                u128::MAX << (128 - u32::from(prefix))
            };
            (u128::from_be_bytes(left.octets()) & mask)
                == (u128::from_be_bytes(right.octets()) & mask)
        }
        _ => false,
    }
}

#[derive(Debug, Error)]
pub enum ControlPlaneError {
    #[error("no connected media-agent is available")]
    NoConnectedNode,
    #[error("media-agent {0} is not connected")]
    NodeDisconnected(Uuid),
    #[error(transparent)]
    Repository(#[from] RepoError),
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
}
