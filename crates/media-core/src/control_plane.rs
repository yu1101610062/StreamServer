use std::{
    cmp::Ordering as CmpOrdering,
    collections::{HashMap, VecDeque},
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use chrono::{DateTime, Utc};
use media_domain::{
    AgentRegistration, CapabilitySnapshot, GpuDeviceInfo, GpuRuntimeStats, HeartbeatSnapshot,
    InputKind, NetworkMode, TaskSpec, TaskType, normalize_output_mount_relative_prefix,
};
use media_rpc::control_plane::{
    AdoptOrphans, AgentEnvelope, CapabilitySnapshot as RpcCapabilitySnapshot, CoreEnvelope,
    GpuDevice as RpcGpuDevice, GpuRuntime as RpcGpuRuntime, Heartbeat as RpcHeartbeat,
    ProbeCapabilities, ReclaimRuntime, Register as RpcRegister, TaskEvent, TaskLogBatch,
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
    capabilities: SessionCapabilities,
    load: SessionLoad,
    reservations: VecDeque<DispatchReservation>,
}

#[derive(Debug, Clone)]
struct SessionTarget {
    node_id: Uuid,
    session_id: u64,
    sender: mpsc::Sender<Result<CoreEnvelope, Status>>,
    same_subnet: bool,
    has_gpu_devices: bool,
    using_gpu_path: bool,
    gpu_headroom: Option<f64>,
    slot_usage: f64,
    running_tasks: u32,
}

#[derive(Debug, Clone, Default)]
struct SessionLoad {
    slot_usage: f64,
    running_tasks: u32,
    starting_tasks: u32,
    stopping_tasks: u32,
    orphaned_tasks: u32,
    cpu_percent: f64,
    mem_percent: f64,
    disk_percent: f64,
    zlm_alive: bool,
    ffmpeg_alive: bool,
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
}

#[derive(Debug, Clone)]
pub struct NodeLiveLoad {
    pub connected: bool,
    pub slot_usage: f64,
    pub running_tasks: u32,
    pub starting_tasks: u32,
    pub stopping_tasks: u32,
    pub orphaned_tasks: u32,
    pub cpu_percent: f64,
    pub mem_percent: f64,
    pub disk_percent: f64,
    pub zlm_alive: bool,
    pub ffmpeg_alive: bool,
    pub gpu_runtime: Vec<GpuRuntimeStats>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct DispatchScore {
    same_subnet: bool,
    gpu_headroom: Option<f64>,
    slot_usage: f64,
    running_tasks: u32,
    node_id: Uuid,
}

#[derive(Debug)]
enum ClaimResult {
    Selected(SessionTarget),
    NoConnectedNode,
    MissingRequiredLabels,
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
        let execution_preference = task_execution_preference(&resolved_spec);
        let retry_affinity_node = if task_keeps_retry_node_affinity(&resolved_spec) {
            self.repository
                .preferred_retry_node_after_disconnect(task_id)
                .await?
        } else {
            None
        };
        let claim = if let Some(node_id) = retry_affinity_node {
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

        if send_core_message(&target.sender, envelope).await.is_err() {
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
                    lease_token: command.lease_token,
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
        let replaced = {
            let mut sessions = self.sessions.lock().await;
            sessions.insert(
                registration.node_id,
                SessionHandle {
                    session_id,
                    sender: sender.clone(),
                    registration: registration.clone(),
                    capabilities: SessionCapabilities::default(),
                    load: SessionLoad::default(),
                    reservations: VecDeque::new(),
                },
            )
        };

        if let Err(error) = self
            .repository
            .upsert_node_registration(registration, Utc::now())
            .await
        {
            self.forget_session(registration.node_id, session_id).await;
            return Err(repo_status(error));
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

        let reclaim_runtimes = self
            .repository
            .list_reclaim_runtimes(registration.node_id)
            .await
            .map_err(repo_status)?;
        if !reclaim_runtimes.is_empty() {
            let envelope = CoreEnvelope {
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
            };
            if let Err(error) = send_core_message(&sender, envelope).await {
                self.close_session(registration.node_id, session_id).await;
                return Err(error);
            }
        }

        info!(
            node_id = %registration.node_id,
            node_name = %registration.node_name,
            replaced_existing_session = replaced.is_some(),
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

            if !self.is_current_session(node_id, session_id).await {
                debug!(
                    node_id = %node_id,
                    session_id,
                    "stale control-plane session observed after replacement"
                );
                break;
            }

            if let Err(error) = self.handle_payload(node_id, session_id, payload).await {
                warn!(node_id = %node_id, error = %error, "failed to process control-plane payload");
                continue;
            }
        }

        self.close_session(node_id, session_id).await;
    }

    async fn handle_payload(
        &self,
        node_id: Uuid,
        session_id: u64,
        payload: media_rpc::control_plane::agent_envelope::Payload,
    ) -> Result<(), Status> {
        if !self.is_current_session(node_id, session_id).await {
            return Ok(());
        }

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
                self.update_session_capabilities(node_id, &snapshot).await?;
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
                if event.event_type == "adopted"
                    && self
                        .repository
                        .attempt_has_stop_intent(event.task_id, event.attempt_no)
                        .await
                        .map_err(repo_status)?
                {
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
                    self.release_dispatch_reservation(node_id, None, event.task_id)
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
                if snapshot.state.eq_ignore_ascii_case("exited") {
                    self.release_dispatch_reservation(node_id, None, snapshot.task_id)
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
            .recover_tasks_for_disconnected_node(node_id)
            .await
        {
            warn!(node_id = %node_id, error = %error, "failed to recover tasks after session close");
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
        let previous_active = session.load.running_tasks
            + session.load.starting_tasks
            + session.load.stopping_tasks
            + session.load.orphaned_tasks;
        let current_active = snapshot.running_tasks
            + snapshot.starting_tasks
            + snapshot.stopping_tasks
            + snapshot.orphaned_tasks;
        for _ in 0..current_active.saturating_sub(previous_active) {
            if session.reservations.pop_front().is_none() {
                break;
            }
        }
        session.load = SessionLoad {
            slot_usage: normalized_slot_usage(snapshot.slot_usage),
            running_tasks: snapshot.running_tasks,
            starting_tasks: snapshot.starting_tasks,
            stopping_tasks: snapshot.stopping_tasks,
            orphaned_tasks: snapshot.orphaned_tasks,
            cpu_percent: snapshot.cpu_percent,
            mem_percent: snapshot.mem_percent,
            disk_percent: snapshot.disk_percent,
            zlm_alive: snapshot.zlm_alive,
            ffmpeg_alive: snapshot.ffmpeg_alive,
            gpu_runtime: snapshot.gpu_runtime.clone(),
        };
        Ok(())
    }

    async fn update_session_capabilities(
        &self,
        node_id: Uuid,
        snapshot: &CapabilitySnapshot,
    ) -> Result<(), Status> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&node_id)
            .ok_or_else(|| Status::unavailable("control-plane session no longer exists"))?;
        session.capabilities = SessionCapabilities {
            gpu_devices: snapshot.gpu_devices.clone(),
        };
        Ok(())
    }

    async fn is_current_session(&self, node_id: Uuid, session_id: u64) -> bool {
        let sessions = self.sessions.lock().await;
        matches!(
            sessions.get(&node_id),
            Some(current) if current.session_id == session_id
        )
    }

    #[cfg(test)]
    async fn pick_best_session(
        &self,
        source_affinity_ip: Option<IpAddr>,
        spec: &TaskSpec,
        preference: ExecutionPreference,
    ) -> Option<SessionTarget> {
        let sessions = self.sessions.lock().await;
        pick_best_session_target(&sessions, source_affinity_ip, spec, preference)
    }

    async fn claim_best_session(
        &self,
        source_affinity_ip: Option<IpAddr>,
        task_id: Uuid,
        spec: &TaskSpec,
        preference: ExecutionPreference,
    ) -> ClaimResult {
        let mut sessions = self.sessions.lock().await;
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
        let Some(handle) = sessions.get_mut(&target.node_id) else {
            return ClaimResult::NoConnectedNode;
        };
        handle
            .reservations
            .push_back(DispatchReservation { task_id });
        let score = dispatch_score(
            target.node_id,
            &handle.registration,
            &handle.capabilities,
            &handle.load,
            source_affinity_ip,
            reservation_count(handle),
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
            running_tasks: score.running_tasks,
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
        let reservations = reservation_count(handle);
        if !session_execution_eligible(
            spec,
            preference,
            &handle.capabilities,
            &handle.load,
            reservations,
        ) {
            return ClaimResult::NoConnectedNode;
        }
        handle
            .reservations
            .push_back(DispatchReservation { task_id });
        let score = dispatch_score(
            node_id,
            &handle.registration,
            &handle.capabilities,
            &handle.load,
            source_affinity_ip,
            reservation_count(handle),
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
            running_tasks: score.running_tasks,
        })
    }

    async fn release_dispatch_reservation(
        &self,
        node_id: Uuid,
        session_id: Option<u64>,
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
                        starting_tasks: handle.load.starting_tasks,
                        stopping_tasks: handle.load.stopping_tasks,
                        orphaned_tasks: handle.load.orphaned_tasks,
                        cpu_percent: handle.load.cpu_percent,
                        mem_percent: handle.load.mem_percent,
                        disk_percent: handle.load.disk_percent,
                        zlm_alive: handle.load.zlm_alive,
                        ffmpeg_alive: handle.load.ffmpeg_alive,
                        gpu_runtime: handle.load.gpu_runtime.clone(),
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
        starting_tasks: heartbeat.starting_tasks,
        stopping_tasks: heartbeat.stopping_tasks,
        orphaned_tasks: heartbeat.orphaned_tasks,
        slot_usage: heartbeat.slot_usage,
        zlm_alive: heartbeat.zlm_alive,
        ffmpeg_alive: heartbeat.ffmpeg_alive,
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

fn reservation_count(handle: &SessionHandle) -> u32 {
    u32::try_from(handle.reservations.len()).unwrap_or(u32::MAX)
}

fn task_execution_preference(spec: &TaskSpec) -> ExecutionPreference {
    let _ = spec;
    ExecutionPreference::CpuOnly
}

fn effective_running_tasks(load: &SessionLoad, reserved_dispatches: u32) -> u32 {
    load.running_tasks.saturating_add(reserved_dispatches)
}

fn estimated_max_slots(load: &SessionLoad) -> Option<u32> {
    let slot_usage = normalized_slot_usage(load.slot_usage);
    if !slot_usage.is_finite() || slot_usage <= 0.0 || load.running_tasks == 0 {
        return None;
    }

    let estimate = (load.running_tasks as f64 / slot_usage).ceil();
    if !estimate.is_finite() || estimate <= 0.0 {
        return None;
    }

    Some((estimate as u32).max(load.running_tasks))
}

fn effective_slot_usage(load: &SessionLoad, reserved_dispatches: u32) -> f64 {
    let base_usage = normalized_slot_usage(load.slot_usage);
    if reserved_dispatches == 0 || !base_usage.is_finite() || base_usage >= 1.0 {
        return base_usage;
    }

    match estimated_max_slots(load) {
        Some(max_slots) if max_slots > 0 => {
            (effective_running_tasks(load, reserved_dispatches) as f64 / max_slots as f64)
                .clamp(0.0, 1.0)
        }
        _ => base_usage,
    }
}

fn session_is_saturated(load: &SessionLoad, reserved_dispatches: u32) -> bool {
    let slot_usage = effective_slot_usage(load, reserved_dispatches);
    slot_usage.is_finite() && slot_usage >= 1.0
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
    if session_is_saturated(load, reserved_dispatches) || !load.ffmpeg_alive {
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
        "accepted"
            | "starting"
            | "recovering"
            | "running"
            | "start_rejected"
            | "succeeded"
            | "failed"
            | "canceled"
    )
}

fn pick_best_session_target(
    sessions: &HashMap<Uuid, SessionHandle>,
    source_affinity_ip: Option<IpAddr>,
    spec: &TaskSpec,
    preference: ExecutionPreference,
) -> Option<SessionTarget> {
    let select = |gpu_only: bool| {
        sessions
            .iter()
            .filter(|(_, handle)| {
                if !node_matches_required_labels(spec, &handle.registration) {
                    return false;
                }
                let reservations = reservation_count(handle);
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
                        reservation_count(left_handle),
                        gpu_only,
                    ),
                    dispatch_score(
                        **right_id,
                        &right_handle.registration,
                        &right_handle.capabilities,
                        &right_handle.load,
                        source_affinity_ip,
                        reservation_count(right_handle),
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
                    reservation_count(handle),
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
                    running_tasks: score.running_tasks,
                }
            })
    };

    match preference {
        ExecutionPreference::CpuOnly => select(false),
    }
}

fn dispatch_score(
    node_id: Uuid,
    registration: &AgentRegistration,
    capabilities: &SessionCapabilities,
    load: &SessionLoad,
    source_affinity_ip: Option<IpAddr>,
    reserved_dispatches: u32,
    prefer_gpu_headroom: bool,
) -> DispatchScore {
    DispatchScore {
        same_subnet: source_affinity_ip
            .is_some_and(|source_ip| node_has_same_subnet(registration, source_ip)),
        gpu_headroom: (prefer_gpu_headroom && !capabilities.gpu_devices.is_empty())
            .then(|| best_gpu_headroom(&load.gpu_runtime))
            .flatten(),
        slot_usage: effective_slot_usage(load, reserved_dispatches),
        running_tasks: effective_running_tasks(load, reserved_dispatches),
        node_id,
    }
}

fn compare_dispatch_score(left: DispatchScore, right: DispatchScore) -> CmpOrdering {
    right
        .same_subnet
        .cmp(&left.same_subnet)
        .then_with(|| compare_gpu_headroom(left.gpu_headroom, right.gpu_headroom))
        .then_with(|| compare_slot_usage(left.slot_usage, right.slot_usage))
        .then_with(|| left.running_tasks.cmp(&right.running_tasks))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    use media_domain::{
        CommonSpec, ExposeSpec, InputSpec, PublishSpec, RecordSpec, RecoverySpec, ResourceSpec,
        ScheduleSpec, StreamSpec, TaskStatus, TaskType,
    };
    use sqlx::{PgPool, Row, postgres::PgPoolOptions};
    use tokio::{net::TcpStream, sync::mpsc, time::timeout};

    fn sample_spec(kind: InputKind, url: Option<&str>, interface_ip: Option<&str>) -> TaskSpec {
        TaskSpec {
            task_type: TaskType::StreamIngest,
            name: "camera".to_string(),
            priority: 50,
            common: CommonSpec {
                created_by: Some("test".to_string()),
                callback_url: None,
                labels: Vec::new(),
            },
            input: InputSpec {
                kind: Some(kind),
                source_mode: kind.default_source_mode(),
                loop_enabled: None,
                url: url.map(str::to_string),
                group: None,
                port: None,
                interface_name: None,
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
            stream: StreamSpec::default(),
            expose: ExposeSpec::default(),
            process: Default::default(),
            publish: PublishSpec::default(),
            record: RecordSpec::default(),
            recovery: RecoverySpec::default(),
            schedule: ScheduleSpec::default(),
            resource: ResourceSpec::default(),
        }
    }

    struct TestDatabase {
        admin_pool: PgPool,
        pool: PgPool,
        database_name: String,
    }

    impl TestDatabase {
        async fn new(run_migrations: bool) -> anyhow::Result<Self> {
            let admin_url = test_admin_database_url();
            let admin_pool = PgPoolOptions::new()
                .max_connections(1)
                .connect(&admin_url)
                .await?;
            let database_name = format!("streamserver_test_{}", Uuid::now_v7().simple());
            sqlx::query(&format!("create database {database_name}"))
                .execute(&admin_pool)
                .await?;

            let database_url = test_database_url(&admin_url, &database_name)?;
            let pool = PgPoolOptions::new()
                .max_connections(5)
                .connect(&database_url)
                .await?;
            if run_migrations {
                sqlx::migrate!("../../migrations").run(&pool).await?;
            }

            Ok(Self {
                admin_pool,
                pool,
                database_name,
            })
        }

        async fn maybe_new(run_migrations: bool) -> anyhow::Result<Option<Self>> {
            if !database_is_reachable(&test_admin_database_url()).await {
                eprintln!("skipping database-backed test: database is unreachable");
                return Ok(None);
            }
            match Self::new(run_migrations).await {
                Ok(database) => Ok(Some(database)),
                Err(error) => {
                    eprintln!("skipping database-backed test: {error}");
                    Ok(None)
                }
            }
        }

        async fn cleanup(self) -> anyhow::Result<()> {
            self.pool.close().await;
            sqlx::query(
                r#"
                select pg_terminate_backend(pid)
                  from pg_stat_activity
                 where datname = $1
                   and pid <> pg_backend_pid()
                "#,
            )
            .bind(&self.database_name)
            .execute(&self.admin_pool)
            .await?;
            sqlx::query(&format!("drop database if exists {}", self.database_name))
                .execute(&self.admin_pool)
                .await?;
            self.admin_pool.close().await;
            Ok(())
        }
    }

    fn test_admin_database_url() -> String {
        std::env::var("TEST_DATABASE_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgresql://postgres:test@127.0.0.1/postgres".to_string())
    }

    fn test_database_url(admin_url: &str, database_name: &str) -> anyhow::Result<String> {
        let mut url = reqwest::Url::parse(admin_url)?;
        url.set_path(&format!("/{database_name}"));
        url.set_query(None);
        Ok(url.to_string())
    }

    async fn database_is_reachable(database_url: &str) -> bool {
        let Ok(url) = reqwest::Url::parse(database_url) else {
            return false;
        };
        let Some(host) = url.host_str() else {
            return false;
        };
        let port = url.port().unwrap_or(5432);
        timeout(
            std::time::Duration::from_secs(1),
            TcpStream::connect((host, port)),
        )
        .await
        .is_ok_and(|result| result.is_ok())
    }

    async fn require_test_database(run_migrations: bool) -> anyhow::Result<Option<TestDatabase>> {
        TestDatabase::maybe_new(run_migrations).await
    }

    fn sample_immediate_task_spec() -> TaskSpec {
        let mut spec = sample_spec(InputKind::Rtsp, Some("rtsp://192.168.20.15/live"), None);
        spec.schedule.start_mode = Some(media_domain::StartMode::Immediate);
        spec
    }

    fn sample_registration(node_id: Uuid) -> AgentRegistration {
        AgentRegistration {
            node_id,
            node_name: format!("node-{node_id}"),
            agent_version: "test".to_string(),
            hostname: "worker-a".to_string(),
            labels: vec!["edge".to_string()],
            interfaces: vec!["eth0|192.168.20.2/24".to_string()],
            zlm_api_base: "http://127.0.0.1:65535".to_string(),
            zlm_api_secret: "secret".to_string(),
            agent_stream_addr: "http://stream.example".to_string(),
            network_mode: NetworkMode::Bridge,
            ffmpeg_bin: "ffmpeg".to_string(),
            ffprobe_bin: "ffprobe".to_string(),
            zlm_server_id: format!("zlm-{node_id}"),
            output_mount_relative_prefix_mp4: String::new(),
            output_mount_relative_prefix_hls: String::new(),
        }
    }

    fn sample_heartbeat(running_tasks: u32, slot_usage: f64) -> HeartbeatSnapshot {
        HeartbeatSnapshot {
            node_time: Utc::now(),
            cpu_percent: 0.0,
            mem_percent: 0.0,
            disk_percent: 0.0,
            running_tasks,
            starting_tasks: 0,
            stopping_tasks: 0,
            orphaned_tasks: 0,
            slot_usage,
            zlm_alive: true,
            ffmpeg_alive: true,
            gpu_runtime: Vec::new(),
        }
    }

    fn sample_gpu_runtime(
        gpu_util: f64,
        encoder_util: f64,
        decoder_util: f64,
    ) -> Vec<GpuRuntimeStats> {
        vec![GpuRuntimeStats {
            index: 0,
            gpu_util_percent: gpu_util,
            memory_used_mb: 1024,
            memory_total_mb: 8192,
            encoder_util_percent: encoder_util,
            decoder_util_percent: decoder_util,
        }]
    }

    fn sample_gpu_capabilities() -> SessionCapabilities {
        SessionCapabilities {
            gpu_devices: vec![GpuDeviceInfo {
                index: 0,
                uuid: "GPU-00000000".to_string(),
                name: "NVIDIA Test GPU".to_string(),
                memory_total_mb: 8192,
            }],
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
            gpu_headroom: None,
            slot_usage: 0.9,
            running_tasks: 8,
            node_id: Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap(),
        };
        let worse = DispatchScore {
            same_subnet: false,
            gpu_headroom: None,
            slot_usage: 0.1,
            running_tasks: 1,
            node_id: Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
        };

        assert_eq!(compare_dispatch_score(better, worse), CmpOrdering::Less);

        let lighter = DispatchScore {
            same_subnet: true,
            gpu_headroom: None,
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
            gpu_headroom: None,
            slot_usage: 0.2,
            running_tasks: 3,
            node_id: Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap(),
        };
        let heavier = DispatchScore {
            same_subnet: false,
            gpu_headroom: None,
            slot_usage: 0.8,
            running_tasks: 1,
            node_id: Uuid::parse_str("00000000-0000-0000-0000-000000000004").unwrap(),
        };
        let same_load_more_tasks = DispatchScore {
            same_subnet: false,
            gpu_headroom: None,
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

    #[tokio::test]
    async fn pick_best_session_skips_saturated_node_without_database() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://postgres:test@127.0.0.1/postgres")
            .expect("lazy test pool should parse");
        let service = ControlPlaneService::new(Arc::new(TaskRepository::new(pool)));
        let node_id = Uuid::parse_str("00000000-0000-0000-0000-000000000009").unwrap();
        let (sender, _receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);

        service.sessions.lock().await.insert(
            node_id,
            SessionHandle {
                session_id: 1,
                sender,
                registration: sample_registration(node_id),
                capabilities: SessionCapabilities::default(),
                load: SessionLoad {
                    slot_usage: 1.0,
                    running_tasks: 1,
                    starting_tasks: 0,
                    stopping_tasks: 0,
                    orphaned_tasks: 0,
                    cpu_percent: 0.0,
                    mem_percent: 0.0,
                    disk_percent: 0.0,
                    zlm_alive: true,
                    ffmpeg_alive: true,
                    gpu_runtime: Vec::new(),
                },
                reservations: VecDeque::new(),
            },
        );

        assert!(
            service
                .pick_best_session(
                    None,
                    &sample_immediate_task_spec(),
                    ExecutionPreference::CpuOnly,
                )
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn claim_best_session_uses_reservations_to_spread_burst_dispatches() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://postgres:test@127.0.0.1/postgres")
            .expect("lazy test pool should parse");
        let service = ControlPlaneService::new(Arc::new(TaskRepository::new(pool)));
        let first_node = Uuid::parse_str("00000000-0000-0000-0000-000000000007").unwrap();
        let second_node = Uuid::parse_str("00000000-0000-0000-0000-000000000008").unwrap();
        let (first_sender, _first_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
        let (second_sender, _second_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);

        {
            let mut sessions = service.sessions.lock().await;
            for (session_id, node_id, sender) in [
                (1, first_node, first_sender),
                (2, second_node, second_sender),
            ] {
                sessions.insert(
                    node_id,
                    SessionHandle {
                        session_id,
                        sender,
                        registration: sample_registration(node_id),
                        capabilities: SessionCapabilities::default(),
                        load: SessionLoad {
                            zlm_alive: true,
                            ffmpeg_alive: true,
                            ..SessionLoad::default()
                        },
                        reservations: VecDeque::new(),
                    },
                );
            }
        }

        let ClaimResult::Selected(first) = service
            .claim_best_session(
                None,
                Uuid::parse_str("00000000-0000-0000-0000-000000000101").unwrap(),
                &sample_immediate_task_spec(),
                ExecutionPreference::CpuOnly,
            )
            .await
        else {
            panic!("first dispatch should find a node");
        };
        let ClaimResult::Selected(second) = service
            .claim_best_session(
                None,
                Uuid::parse_str("00000000-0000-0000-0000-000000000102").unwrap(),
                &sample_immediate_task_spec(),
                ExecutionPreference::CpuOnly,
            )
            .await
        else {
            panic!("second dispatch should find a node");
        };

        assert_eq!(first.node_id, first_node);
        assert_eq!(second.node_id, second_node);
    }

    #[tokio::test]
    async fn required_labels_filter_candidates_before_scoring() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://postgres:test@127.0.0.1/postgres")
            .expect("lazy test pool should parse");
        let service = ControlPlaneService::new(Arc::new(TaskRepository::new(pool)));
        let matching_node = Uuid::parse_str("00000000-0000-0000-0000-000000000025").unwrap();
        let other_node = Uuid::parse_str("00000000-0000-0000-0000-000000000026").unwrap();
        let (matching_sender, _matching_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
        let (other_sender, _other_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);

        let mut matching_registration = sample_registration(matching_node);
        matching_registration.labels = vec!["archive".to_string(), "beijing-idc".to_string()];
        let mut other_registration = sample_registration(other_node);
        other_registration.labels = vec!["archive".to_string(), "shanghai".to_string()];

        let mut spec = sample_immediate_task_spec();
        spec.resource.required_labels = vec!["archive".to_string(), "beijing-idc".to_string()];

        let mut sessions = service.sessions.lock().await;
        sessions.insert(
            matching_node,
            SessionHandle {
                session_id: 1,
                sender: matching_sender,
                registration: matching_registration,
                capabilities: SessionCapabilities::default(),
                load: SessionLoad {
                    slot_usage: 0.9,
                    running_tasks: 9,
                    zlm_alive: true,
                    ffmpeg_alive: true,
                    ..SessionLoad::default()
                },
                reservations: VecDeque::new(),
            },
        );
        sessions.insert(
            other_node,
            SessionHandle {
                session_id: 2,
                sender: other_sender,
                registration: other_registration,
                capabilities: SessionCapabilities::default(),
                load: SessionLoad {
                    slot_usage: 0.1,
                    running_tasks: 1,
                    zlm_alive: true,
                    ffmpeg_alive: true,
                    ..SessionLoad::default()
                },
                reservations: VecDeque::new(),
            },
        );
        drop(sessions);

        let target = service
            .pick_best_session(None, &spec, ExecutionPreference::CpuOnly)
            .await
            .expect("required label match should still find a node");

        assert_eq!(target.node_id, matching_node);
    }

    #[tokio::test]
    async fn required_labels_return_none_when_no_online_node_matches() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://postgres:test@127.0.0.1/postgres")
            .expect("lazy test pool should parse");
        let service = ControlPlaneService::new(Arc::new(TaskRepository::new(pool)));
        let node_id = Uuid::parse_str("00000000-0000-0000-0000-000000000027").unwrap();
        let (sender, _receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);

        let mut registration = sample_registration(node_id);
        registration.labels = vec!["archive".to_string()];

        let mut spec = sample_immediate_task_spec();
        spec.resource.required_labels = vec!["gpu".to_string()];

        let mut sessions = service.sessions.lock().await;
        sessions.insert(
            node_id,
            SessionHandle {
                session_id: 1,
                sender,
                registration,
                capabilities: SessionCapabilities::default(),
                load: SessionLoad {
                    zlm_alive: true,
                    ffmpeg_alive: true,
                    ..SessionLoad::default()
                },
                reservations: VecDeque::new(),
            },
        );
        drop(sessions);

        let target = service
            .pick_best_session(None, &spec, ExecutionPreference::CpuOnly)
            .await;

        assert!(target.is_none());
    }

    #[tokio::test]
    async fn required_labels_still_queue_when_matching_node_is_saturated() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://postgres:test@127.0.0.1/postgres")
            .expect("lazy test pool should parse");
        let service = ControlPlaneService::new(Arc::new(TaskRepository::new(pool)));
        let node_id = Uuid::parse_str("00000000-0000-0000-0000-000000000029").unwrap();
        let (sender, _receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);

        let mut registration = sample_registration(node_id);
        registration.labels = vec!["archive".to_string()];

        let mut spec = sample_immediate_task_spec();
        spec.resource.required_labels = vec!["archive".to_string()];

        let mut sessions = service.sessions.lock().await;
        sessions.insert(
            node_id,
            SessionHandle {
                session_id: 1,
                sender,
                registration,
                capabilities: SessionCapabilities::default(),
                load: SessionLoad {
                    slot_usage: 1.0,
                    running_tasks: 1,
                    zlm_alive: true,
                    ffmpeg_alive: true,
                    ..SessionLoad::default()
                },
                reservations: VecDeque::new(),
            },
        );
        drop(sessions);

        let target = service
            .pick_best_session(None, &spec, ExecutionPreference::CpuOnly)
            .await;

        assert!(target.is_none());
    }

    #[tokio::test]
    async fn cpu_only_dispatch_still_prefers_lower_load_gpu_node_as_cpu_candidate() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://postgres:test@127.0.0.1/postgres")
            .expect("lazy test pool should parse");
        let service = ControlPlaneService::new(Arc::new(TaskRepository::new(pool)));
        let gpu_node = Uuid::parse_str("00000000-0000-0000-0000-000000000021").unwrap();
        let cpu_node = Uuid::parse_str("00000000-0000-0000-0000-000000000022").unwrap();
        let (gpu_sender, _gpu_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
        let (cpu_sender, _cpu_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);

        let mut gpu_load = SessionLoad {
            zlm_alive: true,
            ffmpeg_alive: true,
            ..SessionLoad::default()
        };
        gpu_load.gpu_runtime = sample_gpu_runtime(22.0, 18.0, 5.0);

        let mut sessions = service.sessions.lock().await;
        sessions.insert(
            gpu_node,
            SessionHandle {
                session_id: 1,
                sender: gpu_sender,
                registration: sample_registration(gpu_node),
                capabilities: sample_gpu_capabilities(),
                load: gpu_load,
                reservations: VecDeque::new(),
            },
        );
        sessions.insert(
            cpu_node,
            SessionHandle {
                session_id: 2,
                sender: cpu_sender,
                registration: sample_registration(cpu_node),
                capabilities: SessionCapabilities::default(),
                load: SessionLoad {
                    zlm_alive: true,
                    ffmpeg_alive: true,
                    ..SessionLoad::default()
                },
                reservations: VecDeque::new(),
            },
        );
        drop(sessions);

        let target = service
            .pick_best_session(
                None,
                &sample_immediate_task_spec(),
                ExecutionPreference::CpuOnly,
            )
            .await
            .expect("cpu-only task should find a target");

        assert_eq!(target.node_id, gpu_node);
        assert!(!target.using_gpu_path);
    }

    #[tokio::test]
    async fn gpu_nodes_remain_cpu_candidates_when_gpu_is_unavailable() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://postgres:test@127.0.0.1/postgres")
            .expect("lazy test pool should parse");
        let service = ControlPlaneService::new(Arc::new(TaskRepository::new(pool)));
        let gpu_node = Uuid::parse_str("00000000-0000-0000-0000-000000000023").unwrap();
        let cpu_node = Uuid::parse_str("00000000-0000-0000-0000-000000000024").unwrap();
        let (gpu_sender, _gpu_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
        let (cpu_sender, _cpu_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);

        let mut overloaded_gpu_load = SessionLoad {
            slot_usage: 0.1,
            running_tasks: 1,
            zlm_alive: true,
            ffmpeg_alive: true,
            ..SessionLoad::default()
        };
        overloaded_gpu_load.gpu_runtime = sample_gpu_runtime(99.0, 99.0, 10.0);

        let cpu_load = SessionLoad {
            slot_usage: 0.7,
            running_tasks: 4,
            zlm_alive: true,
            ffmpeg_alive: true,
            ..SessionLoad::default()
        };

        let mut sessions = service.sessions.lock().await;
        sessions.insert(
            gpu_node,
            SessionHandle {
                session_id: 1,
                sender: gpu_sender,
                registration: sample_registration(gpu_node),
                capabilities: sample_gpu_capabilities(),
                load: overloaded_gpu_load,
                reservations: VecDeque::new(),
            },
        );
        sessions.insert(
            cpu_node,
            SessionHandle {
                session_id: 2,
                sender: cpu_sender,
                registration: sample_registration(cpu_node),
                capabilities: SessionCapabilities::default(),
                load: cpu_load,
                reservations: VecDeque::new(),
            },
        );
        drop(sessions);

        let target = service
            .pick_best_session(
                None,
                &sample_immediate_task_spec(),
                ExecutionPreference::CpuOnly,
            )
            .await
            .expect("cpu-only task should fall back to a base-eligible node");

        assert_eq!(target.node_id, gpu_node);
        assert!(!target.using_gpu_path);
        assert!(target.has_gpu_devices);
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

    #[tokio::test]
    async fn dispatch_task_rolls_back_when_agent_channel_is_closed() -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let repository = Arc::new(TaskRepository::new(db.pool.clone()));
        let service = ControlPlaneService::new(repository.clone());
        let task = match repository
            .create_task(
                "dispatch-send-failure",
                "dispatch-send-failure-hash",
                sample_immediate_task_spec(),
            )
            .await?
        {
            crate::repository::CreateTaskResult::Fresh(task)
            | crate::repository::CreateTaskResult::Replay(task) => task,
        };
        let task = repository.ensure_task_queued(task.id).await?;

        let node_id = Uuid::now_v7();
        let (sender, receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
        let _session_id = service
            .bootstrap_session(&sample_registration(node_id), sender)
            .await?;
        drop(receiver);

        let error = service
            .dispatch_task(task.id)
            .await
            .expect_err("closed channel should fail dispatch");
        assert!(matches!(error, ControlPlaneError::NodeDisconnected(id) if id == node_id));

        let summary = repository.get_task_summary(task.id).await?;
        assert_eq!(summary.status, TaskStatus::Queued);
        assert_eq!(summary.assigned_node_id, None);

        let active_lease_count: i64 =
            sqlx::query_scalar("select count(*) from task_leases where task_id = $1")
                .bind(task.id)
                .fetch_one(&db.pool)
                .await?;
        assert_eq!(active_lease_count, 0);

        let attempt = sqlx::query(
            r#"
            select status::text as status, failure_code, failure_reason
              from task_attempts
             where task_id = $1
               and attempt_no = 1
            "#,
        )
        .bind(task.id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(attempt.try_get::<String, _>("status")?, "FAILED");
        assert_eq!(
            attempt.try_get::<Option<String>, _>("failure_code")?,
            Some("dispatch_send_failed".to_string())
        );
        assert!(
            attempt
                .try_get::<Option<String>, _>("failure_reason")?
                .unwrap_or_default()
                .contains("failed to send start_task to agent")
        );

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn dispatch_task_returns_no_connected_node_when_only_node_is_full() -> anyhow::Result<()>
    {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let repository = Arc::new(TaskRepository::new(db.pool.clone()));
        let service = ControlPlaneService::new(repository.clone());
        let mut spec = sample_immediate_task_spec();
        spec.resource.required_labels = vec!["edge".to_string()];
        let task = match repository
            .create_task("full-node-dispatch", "full-node-dispatch-hash", spec)
            .await?
        {
            crate::repository::CreateTaskResult::Fresh(task)
            | crate::repository::CreateTaskResult::Replay(task) => task,
        };
        let task = repository.ensure_task_queued(task.id).await?;

        let node_id = Uuid::parse_str("00000000-0000-0000-0000-000000000010")?;
        let (sender, _receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
        let _session_id = service
            .bootstrap_session(&sample_registration(node_id), sender)
            .await?;
        service
            .update_session_load(node_id, &sample_heartbeat(1, 1.0))
            .await?;

        let error = service
            .dispatch_task(task.id)
            .await
            .expect_err("full node should be filtered out");
        assert!(matches!(error, ControlPlaneError::NoConnectedNode));

        let summary = repository.get_task_summary(task.id).await?;
        assert_eq!(summary.status, TaskStatus::Queued);
        assert_eq!(summary.assigned_node_id, None);

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn stream_retry_after_disconnect_waits_for_original_node() -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let repository = Arc::new(TaskRepository::new(db.pool.clone()));
        let service = ControlPlaneService::new(repository.clone());
        let task = match repository
            .create_task(
                "stream-retry-affinity",
                "stream-retry-affinity-hash",
                sample_immediate_task_spec(),
            )
            .await?
        {
            crate::repository::CreateTaskResult::Fresh(task)
            | crate::repository::CreateTaskResult::Replay(task) => task,
        };
        let task = repository.ensure_task_queued(task.id).await?;

        let original_node = Uuid::parse_str("00000000-0000-0000-0000-000000000041")?;
        let standby_node = Uuid::parse_str("00000000-0000-0000-0000-000000000042")?;
        let (original_sender, _original_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
        let original_session_id = service
            .bootstrap_session(&sample_registration(original_node), original_sender)
            .await?;
        service
            .update_session_load(original_node, &sample_heartbeat(0, 0.0))
            .await?;

        service.dispatch_task(task.id).await?;
        let dispatched = repository.get_task_summary(task.id).await?;
        assert_eq!(dispatched.assigned_node_id, Some(original_node));
        assert_eq!(dispatched.current_attempt_no, 1);

        let (standby_sender, _standby_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
        let _standby_session_id = service
            .bootstrap_session(&sample_registration(standby_node), standby_sender)
            .await?;
        service
            .update_session_load(standby_node, &sample_heartbeat(0, 0.0))
            .await?;

        service
            .close_session(original_node, original_session_id)
            .await;

        let retried = repository.get_task_summary(task.id).await?;
        assert_eq!(retried.status, TaskStatus::Queued);
        assert_eq!(retried.assigned_node_id, None);
        assert_eq!(retried.current_attempt_no, 2);

        let error = service
            .dispatch_task(task.id)
            .await
            .expect_err("stream retry should wait for the original node");
        assert!(matches!(error, ControlPlaneError::NoConnectedNode));

        let waiting = repository.get_task_summary(task.id).await?;
        assert_eq!(waiting.status, TaskStatus::Queued);
        assert_eq!(waiting.assigned_node_id, None);
        assert_eq!(waiting.current_attempt_no, 2);

        let (reconnected_sender, _reconnected_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
        let _reconnected_session_id = service
            .bootstrap_session(&sample_registration(original_node), reconnected_sender)
            .await?;
        service
            .update_session_load(original_node, &sample_heartbeat(0, 0.0))
            .await?;

        service.dispatch_task(task.id).await?;
        let redispatched = repository.get_task_summary(task.id).await?;
        assert_eq!(redispatched.status, TaskStatus::Dispatching);
        assert_eq!(redispatched.assigned_node_id, Some(original_node));
        assert_eq!(redispatched.current_attempt_no, 2);

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn dispatch_task_fails_when_no_online_node_matches_required_labels() -> anyhow::Result<()>
    {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let repository = Arc::new(TaskRepository::new(db.pool.clone()));
        let service = ControlPlaneService::new(repository.clone());
        let mut spec = sample_immediate_task_spec();
        spec.resource.required_labels = vec!["archive".to_string()];
        let task = match repository
            .create_task("required-labels-miss", "required-labels-miss-hash", spec)
            .await?
        {
            crate::repository::CreateTaskResult::Fresh(task)
            | crate::repository::CreateTaskResult::Replay(task) => task,
        };
        let task = repository.ensure_task_queued(task.id).await?;

        let node_id = Uuid::parse_str("00000000-0000-0000-0000-000000000028")?;
        let (sender, _receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
        let _session_id = service
            .bootstrap_session(&sample_registration(node_id), sender)
            .await?;

        service.dispatch_task(task.id).await?;

        let summary = repository.get_task_summary(task.id).await?;
        assert_eq!(summary.status, TaskStatus::Failed);
        assert_eq!(summary.assigned_node_id, None);
        assert_eq!(summary.current_attempt_no, 1);

        let attempt = sqlx::query(
            r#"
            select status::text as status, failure_code, failure_reason
              from task_attempts
             where task_id = $1
               and attempt_no = 1
            "#,
        )
        .bind(task.id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(attempt.try_get::<String, _>("status")?, "FAILED");
        assert_eq!(
            attempt.try_get::<Option<String>, _>("failure_code")?,
            Some("required_labels_unmatched".to_string())
        );
        assert!(
            attempt
                .try_get::<Option<String>, _>("failure_reason")?
                .unwrap_or_default()
                .contains("archive")
        );

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn dispatch_task_second_required_labels_failure_reuses_current_attempt()
    -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let repository = Arc::new(TaskRepository::new(db.pool.clone()));
        let service = ControlPlaneService::new(repository.clone());
        let mut spec = sample_immediate_task_spec();
        spec.resource.required_labels = vec!["archive".to_string()];
        let task = match repository
            .create_task(
                "required-labels-miss-retry",
                "required-labels-miss-retry-hash",
                spec,
            )
            .await?
        {
            crate::repository::CreateTaskResult::Fresh(task)
            | crate::repository::CreateTaskResult::Replay(task) => task,
        };
        let task = repository.ensure_task_queued(task.id).await?;

        let node_id = Uuid::parse_str("00000000-0000-0000-0000-000000000029")?;
        let (sender, _receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
        let _session_id = service
            .bootstrap_session(&sample_registration(node_id), sender)
            .await?;

        service.dispatch_task(task.id).await?;
        repository.retry_task(task.id).await?;
        service.dispatch_task(task.id).await?;

        let summary = repository.get_task_summary(task.id).await?;
        assert_eq!(summary.status, TaskStatus::Failed);
        assert_eq!(summary.current_attempt_no, 2);
        assert_eq!(summary.assigned_node_id, None);

        let first_attempt = sqlx::query(
            r#"
            select status::text as status, failure_code
              from task_attempts
             where task_id = $1
               and attempt_no = 1
            "#,
        )
        .bind(task.id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(first_attempt.try_get::<String, _>("status")?, "FAILED");
        assert_eq!(
            first_attempt.try_get::<Option<String>, _>("failure_code")?,
            Some("required_labels_unmatched".to_string())
        );

        let second_attempt = sqlx::query(
            r#"
            select status::text as status, failure_code, failure_reason
              from task_attempts
             where task_id = $1
               and attempt_no = 2
            "#,
        )
        .bind(task.id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(second_attempt.try_get::<String, _>("status")?, "FAILED");
        assert_eq!(
            second_attempt.try_get::<Option<String>, _>("failure_code")?,
            Some("required_labels_unmatched".to_string())
        );
        assert!(
            second_attempt
                .try_get::<Option<String>, _>("failure_reason")?
                .unwrap_or_default()
                .contains("archive")
        );

        let third_attempt_count: i64 = sqlx::query_scalar(
            r#"
            select count(*)
              from task_attempts
             where task_id = $1
               and attempt_no = 3
            "#,
        )
        .bind(task.id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(third_attempt_count, 0);

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn fail_queued_task_returns_invariant_error_when_current_attempt_row_is_missing()
    -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let repository = Arc::new(TaskRepository::new(db.pool.clone()));
        let task = match repository
            .create_task(
                "queued-attempt-invariant",
                "queued-attempt-invariant-hash",
                sample_immediate_task_spec(),
            )
            .await?
        {
            crate::repository::CreateTaskResult::Fresh(task)
            | crate::repository::CreateTaskResult::Replay(task) => task,
        };
        let task = repository.ensure_task_queued(task.id).await?;
        repository
            .fail_queued_task(task.id, "first_failure", "seed first failure")
            .await?;
        repository.retry_task(task.id).await?;

        sqlx::query(
            r#"
            delete from task_attempts
             where task_id = $1
               and attempt_no = 2
            "#,
        )
        .bind(task.id)
        .execute(&db.pool)
        .await?;

        let error = repository
            .fail_queued_task(
                task.id,
                "second_failure",
                "current pending attempt disappeared",
            )
            .await
            .expect_err("missing current attempt row should fail fast");
        assert!(matches!(
            error,
            RepoError::TaskAttemptInvariant {
                task_id,
                attempt_no: 2,
                ..
            } if task_id == task.id
        ));

        let summary = repository.get_task_summary(task.id).await?;
        assert_eq!(summary.status, TaskStatus::Queued);
        assert_eq!(summary.current_attempt_no, 2);

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn dispatch_task_reserves_slots_to_reduce_burst_skew() -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let repository = Arc::new(TaskRepository::new(db.pool.clone()));
        let service = ControlPlaneService::new(repository.clone());

        let first_task = match repository
            .create_task(
                "burst-reservation-a",
                "burst-reservation-a-hash",
                sample_immediate_task_spec(),
            )
            .await?
        {
            crate::repository::CreateTaskResult::Fresh(task)
            | crate::repository::CreateTaskResult::Replay(task) => task,
        };
        let first_task = repository.ensure_task_queued(first_task.id).await?;

        let second_task = match repository
            .create_task(
                "burst-reservation-b",
                "burst-reservation-b-hash",
                sample_immediate_task_spec(),
            )
            .await?
        {
            crate::repository::CreateTaskResult::Fresh(task)
            | crate::repository::CreateTaskResult::Replay(task) => task,
        };
        let second_task = repository.ensure_task_queued(second_task.id).await?;

        let first_node = Uuid::parse_str("00000000-0000-0000-0000-000000000011")?;
        let second_node = Uuid::parse_str("00000000-0000-0000-0000-000000000012")?;
        let (first_sender, _first_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
        let (second_sender, _second_receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
        let _first_session = service
            .bootstrap_session(&sample_registration(first_node), first_sender)
            .await?;
        let _second_session = service
            .bootstrap_session(&sample_registration(second_node), second_sender)
            .await?;

        service.dispatch_task(first_task.id).await?;
        service.dispatch_task(second_task.id).await?;

        let first_summary = repository.get_task_summary(first_task.id).await?;
        let second_summary = repository.get_task_summary(second_task.id).await?;
        assert_eq!(first_summary.assigned_node_id, Some(first_node));
        assert_eq!(second_summary.assigned_node_id, Some(second_node));

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn close_session_requeues_dispatching_task() -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let repository = Arc::new(TaskRepository::new(db.pool.clone()));
        let service = ControlPlaneService::new(repository.clone());
        let task = match repository
            .create_task(
                "disconnect-dispatching",
                "disconnect-dispatching-hash",
                sample_immediate_task_spec(),
            )
            .await?
        {
            crate::repository::CreateTaskResult::Fresh(task)
            | crate::repository::CreateTaskResult::Replay(task) => task,
        };
        let task = repository.ensure_task_queued(task.id).await?;

        let node_id = Uuid::parse_str("00000000-0000-0000-0000-000000000013")?;
        let (sender, _receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
        let session_id = service
            .bootstrap_session(&sample_registration(node_id), sender)
            .await?;

        service.dispatch_task(task.id).await?;
        service.close_session(node_id, session_id).await;

        let summary = repository.get_task_summary(task.id).await?;
        assert_eq!(summary.status, TaskStatus::Queued);
        assert_eq!(summary.assigned_node_id, None);

        let attempt = sqlx::query(
            r#"
            select status::text as status, failure_code
              from task_attempts
             where task_id = $1
               and attempt_no = 1
            "#,
        )
        .bind(task.id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(attempt.try_get::<String, _>("status")?, "FAILED");
        assert_eq!(
            attempt.try_get::<Option<String>, _>("failure_code")?,
            Some("node_disconnected".to_string())
        );

        db.cleanup().await?;
        Ok(())
    }

    #[tokio::test]
    async fn close_session_retries_running_task_when_recovery_is_enabled() -> anyhow::Result<()> {
        let Some(db) = require_test_database(true).await? else {
            return Ok(());
        };
        let repository = Arc::new(TaskRepository::new(db.pool.clone()));
        let service = ControlPlaneService::new(repository.clone());
        let task = match repository
            .create_task(
                "disconnect-running",
                "disconnect-running-hash",
                sample_immediate_task_spec(),
            )
            .await?
        {
            crate::repository::CreateTaskResult::Fresh(task)
            | crate::repository::CreateTaskResult::Replay(task) => task,
        };
        let task = repository.ensure_task_queued(task.id).await?;

        let node_id = Uuid::parse_str("00000000-0000-0000-0000-000000000014")?;
        let (sender, _receiver) = mpsc::channel(CONTROL_STREAM_BUFFER);
        let session_id = service
            .bootstrap_session(&sample_registration(node_id), sender)
            .await?;

        service.dispatch_task(task.id).await?;
        repository
            .record_agent_task_event(
                node_id,
                AgentTaskEventRecord {
                    task_id: task.id,
                    attempt_no: 1,
                    lease_token: "lease-1".to_string(),
                    event_type: "running".to_string(),
                    event_level: "info".to_string(),
                    message: "task is running".to_string(),
                    payload: Value::Null,
                },
            )
            .await?;

        service.close_session(node_id, session_id).await;

        let summary = repository.get_task_summary(task.id).await?;
        assert_eq!(summary.status, TaskStatus::Queued);
        assert_eq!(summary.current_attempt_no, 2);
        assert_eq!(summary.assigned_node_id, None);

        let attempts = sqlx::query(
            r#"
            select attempt_no, status::text as status, failure_code, node_id
              from task_attempts
             where task_id = $1
             order by attempt_no asc
            "#,
        )
        .bind(task.id)
        .fetch_all(&db.pool)
        .await?;
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].try_get::<i32, _>("attempt_no")?, 1);
        assert_eq!(attempts[0].try_get::<String, _>("status")?, "FAILED");
        assert_eq!(
            attempts[0].try_get::<Option<String>, _>("failure_code")?,
            Some("node_disconnected".to_string())
        );
        assert_eq!(attempts[1].try_get::<i32, _>("attempt_no")?, 2);
        assert_eq!(attempts[1].try_get::<String, _>("status")?, "PENDING");
        assert_eq!(attempts[1].try_get::<Option<Uuid>, _>("node_id")?, None);

        db.cleanup().await?;
        Ok(())
    }
}
