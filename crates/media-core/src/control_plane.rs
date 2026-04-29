#[cfg(test)]
#[path = "tests/control_plane.rs"]
mod tests;

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
    TaskProgress, TaskRecordingControl, TaskSnapshot,
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
    AgentTaskEventRecord, RecordingControlCommand, RepoError, TaskLogBatchRecord,
    TaskProgressRecord, TaskRepository, TaskSnapshotRecord,
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
    occupied_tasks: u32,
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

        if send_core_message(&target.sender, envelope).await.is_err() {
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

        if send_core_message(&target.sender, envelope).await.is_err() {
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
                    starting_tasks = snapshot.starting_tasks,
                    stopping_tasks = snapshot.stopping_tasks,
                    orphaned_tasks = snapshot.orphaned_tasks,
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
            .mark_tasks_reclaiming_for_disconnected_node(node_id)
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
            occupied_tasks: score.occupied_tasks,
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
            occupied_tasks: effective_occupied_tasks(&handle.load, reservation_count(handle)),
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
    let zlm_rtmp_port = u16::try_from(register.zlm_rtmp_port)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| Status::invalid_argument("invalid zlm_rtmp_port"))?;
    let zlm_rtsp_port = u16::try_from(register.zlm_rtsp_port)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| Status::invalid_argument("invalid zlm_rtsp_port"))?;

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
        agent_http_base_url: register.agent_http_base_url.trim().to_string(),
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
        slot_usage: heartbeat.slot_usage,
        zlm_alive: heartbeat.zlm_alive,
        ffmpeg_alive: heartbeat.ffmpeg_alive,
        artifact_cleanup_blocked: heartbeat.artifact_cleanup_blocked,
        artifact_cleanup_block_reason: (!heartbeat.artifact_cleanup_block_reason.trim().is_empty())
            .then(|| heartbeat.artifact_cleanup_block_reason),
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

fn reservation_count(handle: &SessionHandle) -> u32 {
    u32::try_from(handle.reservations.len()).unwrap_or(u32::MAX)
}

fn task_execution_preference(spec: &TaskSpec) -> ExecutionPreference {
    let _ = spec;
    ExecutionPreference::CpuOnly
}

fn occupied_tasks(load: &SessionLoad) -> u32 {
    load.running_tasks
        .saturating_add(load.starting_tasks)
        .saturating_add(load.stopping_tasks)
        .saturating_add(load.orphaned_tasks)
}

fn effective_occupied_tasks(load: &SessionLoad, reserved_dispatches: u32) -> u32 {
    occupied_tasks(load).saturating_add(reserved_dispatches)
}

fn estimated_max_slots(load: &SessionLoad) -> Option<u32> {
    let slot_usage = normalized_slot_usage(load.slot_usage);
    let occupied = occupied_tasks(load);
    if !slot_usage.is_finite() || slot_usage <= 0.0 || occupied == 0 {
        return None;
    }

    let estimate = (occupied as f64 / slot_usage).ceil();
    if !estimate.is_finite() || estimate <= 0.0 {
        return None;
    }

    Some((estimate as u32).max(occupied))
}

fn effective_slot_usage(load: &SessionLoad, reserved_dispatches: u32) -> f64 {
    let base_usage = normalized_slot_usage(load.slot_usage);
    if reserved_dispatches == 0 || !base_usage.is_finite() || base_usage >= 1.0 {
        return base_usage;
    }

    match estimated_max_slots(load) {
        Some(max_slots) if max_slots > 0 => {
            (effective_occupied_tasks(load, reserved_dispatches) as f64 / max_slots as f64)
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
    if load.artifact_cleanup_blocked {
        return false;
    }
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
        "start_rejected" | "succeeded" | "failed" | "canceled"
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
                    occupied_tasks: score.occupied_tasks,
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
        occupied_tasks: effective_occupied_tasks(load, reserved_dispatches),
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
