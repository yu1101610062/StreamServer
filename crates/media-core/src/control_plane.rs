use std::{
    cmp::Ordering as CmpOrdering,
    collections::HashMap,
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use chrono::{DateTime, Utc};
use media_domain::{
    AgentRegistration, CapabilitySnapshot, HeartbeatSnapshot, InputKind, NetworkMode, TaskSpec,
};
use media_rpc::control_plane::{
    AdoptOrphans, AgentEnvelope, CapabilitySnapshot as RpcCapabilitySnapshot, CoreEnvelope,
    Heartbeat as RpcHeartbeat, ProbeCapabilities, Register as RpcRegister, TaskEvent, TaskLogBatch,
    TaskProgress, TaskSnapshot,
    control_plane_server::{ControlPlane, ControlPlaneServer},
};
use reqwest::Url;
use serde_json::Value;
use thiserror::Error;
use tokio::{
    sync::{Mutex, mpsc},
    time::timeout,
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::repository::{
    AgentTaskEventRecord, RepoError, TaskLogBatchRecord, TaskProgressRecord, TaskRepository,
    TaskSnapshotRecord,
};

const CONTROL_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const CONTROL_STREAM_BUFFER: usize = 32;

#[derive(Debug, Clone)]
pub struct ControlPlaneService {
    repository: Arc<TaskRepository>,
    sessions: Arc<Mutex<HashMap<Uuid, SessionHandle>>>,
    session_seq: Arc<AtomicU64>,
}

#[derive(Debug)]
struct SessionHandle {
    session_id: u64,
    sender: mpsc::Sender<Result<CoreEnvelope, Status>>,
    registration: AgentRegistration,
    load: SessionLoad,
}

#[derive(Debug, Clone)]
struct SessionTarget {
    node_id: Uuid,
    session_id: u64,
    sender: mpsc::Sender<Result<CoreEnvelope, Status>>,
    same_subnet: bool,
    slot_usage: f64,
    running_tasks: u32,
}

#[derive(Debug, Clone, Copy, Default)]
struct SessionLoad {
    slot_usage: f64,
    running_tasks: u32,
    cpu_percent: f64,
    mem_percent: f64,
    disk_percent: f64,
    zlm_alive: bool,
    ffmpeg_alive: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct NodeLiveLoad {
    pub connected: bool,
    pub slot_usage: f64,
    pub running_tasks: u32,
    pub cpu_percent: f64,
    pub mem_percent: f64,
    pub disk_percent: f64,
    pub zlm_alive: bool,
    pub ffmpeg_alive: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct DispatchScore {
    same_subnet: bool,
    slot_usage: f64,
    running_tasks: u32,
    node_id: Uuid,
}

impl ControlPlaneService {
    pub fn new(repository: Arc<TaskRepository>) -> Self {
        Self {
            repository,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            session_seq: Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn into_server(self) -> ControlPlaneServer<Self> {
        ControlPlaneServer::new(self)
    }

    pub async fn dispatch_task(&self, task_id: Uuid) -> Result<(), ControlPlaneError> {
        self.repository.ensure_task_queued(task_id).await?;
        let resolved_spec =
            serde_json::from_value::<TaskSpec>(self.repository.get_resolved_spec(task_id).await?)?;
        let source_affinity_ip = task_source_affinity_ip(&resolved_spec);
        let target = self
            .pick_best_session(source_affinity_ip)
            .await
            .ok_or(ControlPlaneError::NoConnectedNode)?;
        let command = self
            .repository
            .prepare_task_dispatch(task_id, target.node_id, &format!("node:{}", target.node_id))
            .await?;

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

        if send_core_message(&target.sender, envelope).await.is_err() {
            self.close_session(target.node_id, target.session_id).await;
            return Err(ControlPlaneError::NodeDisconnected(target.node_id));
        }

        info!(
            task_id = %task_id,
            node_id = %command.node_id,
            attempt_no = command.attempt_no,
            source_affinity_ip = ?source_affinity_ip,
            same_subnet = target.same_subnet,
            slot_usage = target.slot_usage,
            running_tasks = target.running_tasks,
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
        let Some(command) = self
            .repository
            .build_stop_command(task_id, reason, grace_period_sec, force_after_sec)
            .await?
        else {
            return Ok(());
        };

        let Some(target) = self.session_for_node(command.node_id).await else {
            return Err(ControlPlaneError::NodeDisconnected(command.node_id));
        };

        let envelope = CoreEnvelope {
            payload: Some(media_rpc::control_plane::core_envelope::Payload::StopTask(
                media_rpc::control_plane::StopTask {
                    task_id: command.task_id.to_string(),
                    attempt_no: command.attempt_no,
                    reason: command.reason,
                    grace_period_sec: command.grace_period_sec,
                    force_after_sec: command.force_after_sec,
                },
            )),
        };

        if send_core_message(&target.sender, envelope).await.is_err() {
            self.close_session(target.node_id, target.session_id).await;
            return Err(ControlPlaneError::NodeDisconnected(target.node_id));
        }

        info!(
            task_id = %task_id,
            node_id = %command.node_id,
            attempt_no = command.attempt_no,
            "stop_task sent to agent"
        );

        Ok(())
    }

    async fn bootstrap_session(
        &self,
        registration: &AgentRegistration,
        sender: mpsc::Sender<Result<CoreEnvelope, Status>>,
    ) -> Result<u64, Status> {
        let session_id = self.session_seq.fetch_add(1, Ordering::Relaxed);
        let inserted = {
            let mut sessions = self.sessions.lock().await;
            if sessions.contains_key(&registration.node_id) {
                return Err(Status::already_exists(format!(
                    "node_id {} is already connected",
                    registration.node_id
                )));
            }
            sessions.insert(
                registration.node_id,
                SessionHandle {
                    session_id,
                    sender: sender.clone(),
                    registration: registration.clone(),
                    load: SessionLoad::default(),
                },
            );
            true
        };

        if inserted {
            if let Err(error) = self
                .repository
                .upsert_node_registration(registration, Utc::now())
                .await
            {
                self.forget_session(registration.node_id, session_id).await;
                return Err(repo_status(error));
            }
        }

        if let Err(error) = send_core_message(
            &sender,
            CoreEnvelope {
                payload: Some(
                    media_rpc::control_plane::core_envelope::Payload::ProbeCapabilities(
                        ProbeCapabilities {},
                    ),
                ),
            },
        )
        .await
        {
            self.close_session(registration.node_id, session_id).await;
            return Err(error);
        }

        if let Err(error) = send_core_message(
            &sender,
            CoreEnvelope {
                payload: Some(
                    media_rpc::control_plane::core_envelope::Payload::AdoptOrphans(AdoptOrphans {
                        task_ids: Vec::new(),
                        worker_kind: Vec::new(),
                    }),
                ),
            },
        )
        .await
        {
            self.close_session(registration.node_id, session_id).await;
            return Err(error);
        }

        info!(
            node_id = %registration.node_id,
            node_name = %registration.node_name,
            "control-plane session registered"
        );

        Ok(session_id)
    }

    async fn process_stream(
        &self,
        node_id: Uuid,
        session_id: u64,
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

            let Some(payload) = message.payload else {
                continue;
            };

            if let Err(error) = self.handle_payload(node_id, payload).await {
                warn!(node_id = %node_id, error = %error, "failed to process control-plane payload");
                break;
            }
        }

        self.close_session(node_id, session_id).await;
    }

    async fn handle_payload(
        &self,
        node_id: Uuid,
        payload: media_rpc::control_plane::agent_envelope::Payload,
    ) -> Result<(), Status> {
        match payload {
            media_rpc::control_plane::agent_envelope::Payload::Register(register) => {
                let registration = registration_from_rpc(register)?;
                if registration.node_id != node_id {
                    return Err(Status::failed_precondition(format!(
                        "node_id cannot change on an active control-plane stream: expected {node_id}, got {}",
                        registration.node_id
                    )));
                }
                self.update_session_registration(node_id, &registration)
                    .await?;
                self.repository
                    .upsert_node_registration(&registration, Utc::now())
                    .await
                    .map_err(repo_status)?;
            }
            media_rpc::control_plane::agent_envelope::Payload::Heartbeat(heartbeat) => {
                let snapshot = heartbeat_from_rpc(heartbeat)?;
                self.update_session_load(node_id, &snapshot).await?;
                self.repository
                    .record_node_heartbeat(node_id, &snapshot)
                    .await
                    .map_err(repo_status)?;
                debug!(
                    node_id = %node_id,
                    running_tasks = snapshot.running_tasks,
                    slot_usage = snapshot.slot_usage,
                    zlm_alive = snapshot.zlm_alive,
                    ffmpeg_alive = snapshot.ffmpeg_alive,
                    "heartbeat updated"
                );
            }
            media_rpc::control_plane::agent_envelope::Payload::CapabilitySnapshot(snapshot) => {
                let snapshot = capability_from_rpc(snapshot);
                self.repository
                    .upsert_node_capabilities(node_id, &snapshot)
                    .await
                    .map_err(repo_status)?;
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
                self.repository
                    .record_agent_task_event(node_id, event.clone())
                    .await
                    .map_err(repo_status)?;
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
                self.repository
                    .record_agent_log_batch(node_id, batch.clone())
                    .await
                    .map_err(repo_status)?;
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
                self.repository
                    .record_agent_progress(node_id, progress.clone())
                    .await
                    .map_err(repo_status)?;
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
                self.repository
                    .record_agent_snapshot(node_id, snapshot.clone())
                    .await
                    .map_err(repo_status)?;
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
        }

        Ok(())
    }

    async fn close_session(&self, node_id: Uuid, session_id: u64) {
        let removed = {
            let mut sessions = self.sessions.lock().await;
            match sessions.get(&node_id) {
                Some(current) if current.session_id == session_id => sessions.remove(&node_id),
                _ => None,
            }
        };

        if removed.is_none() {
            return;
        }

        if let Err(error) = self
            .repository
            .update_node_health(node_id, false, None)
            .await
        {
            warn!(node_id = %node_id, error = %error, "failed to mark node unhealthy");
        } else {
            info!(node_id = %node_id, "control-plane session closed");
        }
    }

    async fn forget_session(&self, node_id: Uuid, session_id: u64) {
        let mut sessions = self.sessions.lock().await;
        if matches!(
            sessions.get(&node_id),
            Some(current) if current.session_id == session_id
        ) {
            sessions.remove(&node_id);
        }
    }

    async fn update_session_registration(
        &self,
        node_id: Uuid,
        registration: &AgentRegistration,
    ) -> Result<(), Status> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&node_id)
            .ok_or_else(|| Status::unavailable("control-plane session no longer exists"))?;
        session.registration = registration.clone();
        Ok(())
    }

    async fn update_session_load(
        &self,
        node_id: Uuid,
        snapshot: &HeartbeatSnapshot,
    ) -> Result<(), Status> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&node_id)
            .ok_or_else(|| Status::unavailable("control-plane session no longer exists"))?;
        session.load = SessionLoad {
            slot_usage: normalized_slot_usage(snapshot.slot_usage),
            running_tasks: snapshot.running_tasks,
            cpu_percent: snapshot.cpu_percent,
            mem_percent: snapshot.mem_percent,
            disk_percent: snapshot.disk_percent,
            zlm_alive: snapshot.zlm_alive,
            ffmpeg_alive: snapshot.ffmpeg_alive,
        };
        Ok(())
    }

    async fn pick_best_session(&self, source_affinity_ip: Option<IpAddr>) -> Option<SessionTarget> {
        let sessions = self.sessions.lock().await;
        sessions
            .iter()
            .min_by(|(left_id, left_handle), (right_id, right_handle)| {
                compare_dispatch_score(
                    dispatch_score(
                        **left_id,
                        &left_handle.registration,
                        &left_handle.load,
                        source_affinity_ip,
                    ),
                    dispatch_score(
                        **right_id,
                        &right_handle.registration,
                        &right_handle.load,
                        source_affinity_ip,
                    ),
                )
            })
            .map(|(node_id, handle)| {
                let score = dispatch_score(
                    *node_id,
                    &handle.registration,
                    &handle.load,
                    source_affinity_ip,
                );
                SessionTarget {
                    node_id: *node_id,
                    session_id: handle.session_id,
                    sender: handle.sender.clone(),
                    same_subnet: score.same_subnet,
                    slot_usage: score.slot_usage,
                    running_tasks: score.running_tasks,
                }
            })
    }

    async fn session_for_node(&self, node_id: Uuid) -> Option<SessionTarget> {
        let sessions = self.sessions.lock().await;
        sessions.get(&node_id).map(|handle| SessionTarget {
            node_id,
            session_id: handle.session_id,
            sender: handle.sender.clone(),
            same_subnet: false,
            slot_usage: handle.load.slot_usage,
            running_tasks: handle.load.running_tasks,
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
                        slot_usage: handle.load.slot_usage,
                        running_tasks: handle.load.running_tasks,
                        cpu_percent: handle.load.cpu_percent,
                        mem_percent: handle.load.mem_percent,
                        disk_percent: handle.load.disk_percent,
                        zlm_alive: handle.load.zlm_alive,
                        ffmpeg_alive: handle.load.ffmpeg_alive,
                    },
                )
            })
            .collect()
    }
}

#[tonic::async_trait]
impl ControlPlane for ControlPlaneService {
    type StreamConnectStream = ReceiverStream<Result<CoreEnvelope, Status>>;

    async fn stream_connect(
        &self,
        request: Request<Streaming<AgentEnvelope>>,
    ) -> Result<Response<Self::StreamConnectStream>, Status> {
        let mut inbound = request.into_inner();
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

        let registration = registration_from_rpc(register)?;
        let (sender, receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
        let session_id = self
            .bootstrap_session(&registration, sender.clone())
            .await?;

        let service = self.clone();
        tokio::spawn(async move {
            service
                .process_stream(registration.node_id, session_id, inbound)
                .await;
        });

        Ok(Response::new(ReceiverStream::new(receiver)))
    }
}

fn repo_status(error: RepoError) -> Status {
    Status::internal(error.to_string())
}

fn registration_from_rpc(register: RpcRegister) -> Result<AgentRegistration, Status> {
    let node_id = Uuid::parse_str(register.node_id.trim())
        .map_err(|error| Status::invalid_argument(format!("invalid node_id: {error}")))?;
    let network_mode = register
        .network_mode
        .parse::<NetworkMode>()
        .map_err(|error| Status::invalid_argument(error.to_string()))?;

    Ok(AgentRegistration {
        node_id,
        node_name: require_field("node_name", register.node_name)?,
        agent_version: require_field("agent_version", register.agent_version)?,
        hostname: require_field("hostname", register.hostname)?,
        labels: normalize_strings(register.labels),
        interfaces: normalize_strings(register.interfaces),
        zlm_api_base: register.zlm_api_base.trim().to_string(),
        zlm_api_secret: register.zlm_api_secret.trim().to_string(),
        agent_stream_addr: require_field("agent_stream_addr", register.agent_stream_addr)?,
        network_mode,
        ffmpeg_bin: require_field("ffmpeg_bin", register.ffmpeg_bin)?,
        ffprobe_bin: require_field("ffprobe_bin", register.ffprobe_bin)?,
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
        running_tasks: heartbeat.running_tasks,
        slot_usage: heartbeat.slot_usage,
        zlm_alive: heartbeat.zlm_alive,
        ffmpeg_alive: heartbeat.ffmpeg_alive,
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
        captured_at: Utc::now(),
    }
}

fn parse_task_event(event: TaskEvent) -> Result<AgentTaskEventRecord, Status> {
    Ok(AgentTaskEventRecord {
        task_id: parse_uuid("task_id", &event.task_id)?,
        attempt_no: event.attempt_no,
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
        stream: require_string("stream", batch.stream)?,
        lines: batch.lines,
    })
}

fn parse_task_progress(progress: TaskProgress) -> Result<TaskProgressRecord, Status> {
    Ok(TaskProgressRecord {
        task_id: parse_uuid("task_id", &progress.task_id)?,
        attempt_no: progress.attempt_no,
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
        worker_kind: require_string("worker_kind", snapshot.worker_kind)?,
        pid: (snapshot.pid > 0).then_some(snapshot.pid),
        state: require_string("state", snapshot.state)?,
        command_line: option_string(snapshot.command_line),
        outputs: snapshot.outputs,
        metadata: parse_json("metadata_json", &snapshot.metadata_json)?,
    })
}

async fn send_core_message(
    sender: &mpsc::Sender<Result<CoreEnvelope, Status>>,
    envelope: CoreEnvelope,
) -> Result<(), Status> {
    sender
        .send(Ok(envelope))
        .await
        .map_err(|_| Status::unavailable("agent stream closed"))
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

fn parse_ip_literal(value: &str) -> Option<IpAddr> {
    value.trim().parse().ok()
}

fn parse_url_host_ip_literal(value: &str) -> Option<IpAddr> {
    Url::parse(value.trim())
        .ok()?
        .host_str()
        .and_then(parse_ip_literal)
}

fn dispatch_score(
    node_id: Uuid,
    registration: &AgentRegistration,
    load: &SessionLoad,
    source_affinity_ip: Option<IpAddr>,
) -> DispatchScore {
    DispatchScore {
        same_subnet: source_affinity_ip
            .is_some_and(|source_ip| node_has_same_subnet(registration, source_ip)),
        slot_usage: normalized_slot_usage(load.slot_usage),
        running_tasks: load.running_tasks,
        node_id,
    }
}

fn compare_dispatch_score(left: DispatchScore, right: DispatchScore) -> CmpOrdering {
    right
        .same_subnet
        .cmp(&left.same_subnet)
        .then_with(|| compare_slot_usage(left.slot_usage, right.slot_usage))
        .then_with(|| left.running_tasks.cmp(&right.running_tasks))
        .then_with(|| left.node_id.cmp(&right.node_id))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    use media_domain::{
        CommonSpec, InputSpec, PublishSpec, RecordSpec, RecoverySpec, ResourceSpec, ScheduleSpec,
        TaskType,
    };

    fn sample_spec(kind: InputKind, url: Option<&str>, interface_ip: Option<&str>) -> TaskSpec {
        TaskSpec {
            task_type: TaskType::LiveRelay,
            template: None,
            name: "camera".to_string(),
            profile: None,
            priority: 50,
            common: CommonSpec {
                tenant_id: Some("default".to_string()),
                created_by: Some("test".to_string()),
                callback_url: None,
                labels: Vec::new(),
            },
            input: InputSpec {
                kind: Some(kind),
                url: url.map(str::to_string),
                group: None,
                port: None,
                interface_ip: interface_ip.map(str::to_string),
                ttl: None,
                reuse: None,
                pkt_size: None,
                dscp: None,
                buffer_size: None,
                fifo_size: None,
                probe_timeout_ms: None,
                tcp_mode: None,
                ssrc: None,
            },
            process: Default::default(),
            publish: PublishSpec::default(),
            record: RecordSpec::default(),
            recovery: RecoverySpec::default(),
            schedule: ScheduleSpec::default(),
            resource: ResourceSpec::default(),
        }
    }

    #[test]
    fn task_source_affinity_uses_source_url_instead_of_local_interface_ip() {
        let spec = sample_spec(
            InputKind::Rtsp,
            Some("rtsp://10.10.10.20/live"),
            Some("192.168.10.8"),
        );

        assert_eq!(
            task_source_affinity_ip(&spec),
            Some(IpAddr::V4(Ipv4Addr::new(10, 10, 10, 20)))
        );
    }

    #[test]
    fn task_source_affinity_uses_literal_url_host() {
        let spec = sample_spec(InputKind::Rtsp, Some("rtsp://192.168.20.15/live"), None);

        assert_eq!(
            task_source_affinity_ip(&spec),
            Some(IpAddr::V4(Ipv4Addr::new(192, 168, 20, 15)))
        );
    }

    #[test]
    fn task_source_affinity_ignores_domain_hosts() {
        let spec = sample_spec(InputKind::Rtsp, Some("rtsp://camera.example/live"), None);

        assert_eq!(task_source_affinity_ip(&spec), None);
    }

    #[test]
    fn parse_interface_network_accepts_named_cidr() {
        let network = parse_interface_network("eth0|192.168.10.7/24").expect("cidr should parse");

        assert_eq!(
            network,
            InterfaceNetwork {
                ip: IpAddr::V4(Ipv4Addr::new(192, 168, 10, 7)),
                prefix: 24,
            }
        );
    }

    #[test]
    fn compare_dispatch_score_prefers_same_subnet_then_lower_load() {
        let better = DispatchScore {
            same_subnet: true,
            slot_usage: 0.9,
            running_tasks: 8,
            node_id: Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap(),
        };
        let worse = DispatchScore {
            same_subnet: false,
            slot_usage: 0.1,
            running_tasks: 1,
            node_id: Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
        };

        assert_eq!(compare_dispatch_score(better, worse), CmpOrdering::Less);

        let lighter = DispatchScore {
            same_subnet: true,
            slot_usage: 0.2,
            running_tasks: 5,
            node_id: Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap(),
        };

        assert_eq!(compare_dispatch_score(lighter, better), CmpOrdering::Less);
    }

    #[test]
    fn compare_dispatch_score_falls_back_to_load_and_running_tasks() {
        let lighter = DispatchScore {
            same_subnet: false,
            slot_usage: 0.2,
            running_tasks: 3,
            node_id: Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap(),
        };
        let heavier = DispatchScore {
            same_subnet: false,
            slot_usage: 0.8,
            running_tasks: 1,
            node_id: Uuid::parse_str("00000000-0000-0000-0000-000000000004").unwrap(),
        };
        let same_load_more_tasks = DispatchScore {
            same_subnet: false,
            slot_usage: 0.2,
            running_tasks: 6,
            node_id: Uuid::parse_str("00000000-0000-0000-0000-000000000005").unwrap(),
        };

        assert_eq!(compare_dispatch_score(lighter, heavier), CmpOrdering::Less);
        assert_eq!(
            compare_dispatch_score(lighter, same_load_more_tasks),
            CmpOrdering::Less
        );
    }

    #[test]
    fn same_subnet_matches_ipv4_prefix() {
        assert!(same_subnet(
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 200)),
            24,
        ));
        assert!(!same_subnet(
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)),
            IpAddr::V4(Ipv4Addr::new(192, 168, 2, 10)),
            24,
        ));
    }
}
