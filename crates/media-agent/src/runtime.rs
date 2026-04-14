use std::{
    collections::{HashMap, HashSet},
    ffi::CStr,
    fs,
    future::Future,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    process::Stdio,
    ptr,
    str::FromStr,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use chrono::{DateTime, Local, Utc};
use media_domain::{
    ExposeSpec, InputKind, InputSpec, PublishSpec, PublishTargetKind, RecoveryPolicy,
    RuntimeHandle, RuntimeState, SourceMode, StreamIngestRecordMode, TaskSpec, TaskType,
    WorkerKind, normalize_relative_file_input_path,
};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::mpsc,
    time::{sleep, timeout},
};
use uuid::Uuid;

use crate::{
    capability::{
        ffmpeg_supports_decoder, ffmpeg_supports_encoder, ffmpeg_supports_hwaccel,
        gpu_acceleration_enabled, probe_gpu_devices,
    },
    config::AgentSettings,
};

#[derive(Debug, Clone)]
pub struct LocalRuntimeRegistry {
    inner: Arc<RwLock<HashMap<Uuid, RuntimeHandle>>>,
}

impl LocalRuntimeRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn track(&self, handle: RuntimeHandle) {
        let mut runtimes = self.inner.write().expect("runtime registry lock poisoned");
        runtimes.insert(handle.runtime_id, handle);
    }

    pub fn remove(&self, runtime_id: Uuid) -> Option<RuntimeHandle> {
        let mut runtimes = self.inner.write().expect("runtime registry lock poisoned");
        runtimes.remove(&runtime_id)
    }

    pub fn update(
        &self,
        runtime_id: Uuid,
        update: impl FnOnce(&mut RuntimeHandle),
    ) -> Option<RuntimeHandle> {
        let mut runtimes = self.inner.write().expect("runtime registry lock poisoned");
        let handle = runtimes.get_mut(&runtime_id)?;
        update(handle);
        Some(handle.clone())
    }

    pub fn get(&self, runtime_id: Uuid) -> Option<RuntimeHandle> {
        let runtimes = self.inner.read().expect("runtime registry lock poisoned");
        runtimes.get(&runtime_id).cloned()
    }

    pub fn find_by_task_attempt(&self, task_id: Uuid, attempt_no: i32) -> Option<RuntimeHandle> {
        let runtimes = self.inner.read().expect("runtime registry lock poisoned");
        runtimes
            .values()
            .find(|handle| handle.task_id == task_id && handle.attempt_no == attempt_no)
            .cloned()
    }

    pub fn count(&self) -> usize {
        let runtimes = self.inner.read().expect("runtime registry lock poisoned");
        runtimes.len()
    }

    pub fn snapshots(&self, filter: &AdoptFilter) -> Vec<RuntimeHandle> {
        let runtimes = self.inner.read().expect("runtime registry lock poisoned");
        runtimes
            .values()
            .filter(|handle| filter.matches(handle))
            .cloned()
            .collect()
    }
}

impl Default for LocalRuntimeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct StartTaskRequest {
    pub task_id: Uuid,
    pub attempt_no: i32,
    pub task_type: TaskType,
    pub resolved_spec: Value,
    pub execution_mode: String,
    pub lease_token: String,
    pub trace_context: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StopTaskRequest {
    pub task_id: Uuid,
    pub attempt_no: i32,
    pub reason: String,
    pub grace_period_sec: u32,
    pub force_after_sec: u32,
}

#[derive(Debug, Clone, Default)]
pub struct AdoptFilter {
    pub task_ids: Vec<Uuid>,
    pub worker_kinds: Vec<WorkerKind>,
}

impl AdoptFilter {
    fn matches(&self, handle: &RuntimeHandle) -> bool {
        let task_ok = self.task_ids.is_empty() || self.task_ids.contains(&handle.task_id);
        let worker_ok =
            self.worker_kinds.is_empty() || self.worker_kinds.contains(&handle.worker_kind);
        task_ok && worker_ok
    }
}

pub trait LocalExecutor: Send + Sync {
    fn start_task(&self, request: &StartTaskRequest) -> Result<RuntimeHandle, ExecutorError>;
    fn stop_task(&self, request: &StopTaskRequest) -> Result<(), ExecutorError>;
    fn adopt_orphans(&self, filter: &AdoptFilter) -> Vec<RuntimeHandle>;
}

#[derive(Debug, Clone)]
pub struct ManagedProcessExecutor {
    settings: AgentSettings,
    registry: LocalRuntimeRegistry,
    events: RuntimeEventSink,
    runtimes: Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    http_client: Client,
}

#[derive(Debug, Clone)]
struct ManagedRuntime {
    pid: Option<i32>,
    stop_requested: Arc<AtomicBool>,
}

#[derive(Debug, Clone)]
struct ProcessPlan {
    executable: String,
    args: Vec<String>,
    work_dir: PathBuf,
    output_target: String,
    outputs: Vec<String>,
    success_check: SuccessCheck,
    startup_probe: Option<StartupProbe>,
    recording: Option<LiveRelayRecording>,
    managed_file_output_kind: Option<ManagedFileOutputKind>,
}

#[derive(Debug, Clone)]
struct LiveRelayPlan {
    work_dir: PathBuf,
    input_url: String,
    command_line: String,
    outputs: Vec<String>,
    startup_probe: StartupProbe,
    recording: Option<LiveRelayRecording>,
}

#[derive(Debug, Clone)]
struct RtpReceivePlan {
    work_dir: PathBuf,
    stream_id: String,
    requested_port: u16,
    tcp_mode: u8,
    reuse_port: Option<bool>,
    ssrc: Option<u32>,
    command_line: String,
    outputs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum SuccessCheck {
    FileExists(PathBuf),
    ProcessExit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedRuntimeState {
    handle: RuntimeHandle,
    work_dir: PathBuf,
    success_check: SuccessCheck,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StartupProbe {
    schema: Option<String>,
    vhost: String,
    app: String,
    stream: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskRuntimeMode {
    ManagedProcess,
    ZlmProxy,
    ZlmRtpServer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ZlmRecordKind {
    Hls,
    Mp4,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LiveRelayRecording {
    formats: Vec<ZlmRecordKind>,
    root_path: String,
    duration_sec: Option<u32>,
    segment_sec: Option<u32>,
    as_player: bool,
    #[serde(default)]
    recording_started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    auto_stop_requested: bool,
    #[serde(default)]
    completion_reason: Option<String>,
    #[serde(default)]
    started: bool,
    #[serde(default)]
    failed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RtpServerMetadata {
    stream_id: String,
    local_port: u16,
    requested_port: u16,
    tcp_mode: u8,
    reuse_port: Option<bool>,
    ssrc: Option<u32>,
}

const RUNTIME_STATE_FILE: &str = "runtime.json";
const RUNTIME_PID_FILE: &str = "runtime.pid";
const RUNTIME_COMMAND_FILE: &str = "runtime.cmd";
const STARTUP_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const STARTUP_PROBE_POLL_INTERVAL: Duration = Duration::from_secs(1);
const PROCESS_RECOVERY_WAIT_TIMEOUT: Duration = Duration::from_secs(15);
const PROCESS_RECOVERY_POLL_INTERVAL: Duration = Duration::from_secs(1);
const LOG_BATCH_FLUSH_INTERVAL: Duration = Duration::from_millis(250);
const MAX_LOG_BATCH_LINES: usize = 64;
const ZLM_RUNTIME_VHOST: &str = "__defaultVhost__";

#[derive(Debug, Clone)]
pub enum RuntimeNotification {
    TaskEvent(RuntimeTaskEvent),
    TaskLogBatch(RuntimeTaskLogBatch),
    TaskProgress(RuntimeTaskProgress),
    TaskSnapshot(RuntimeHandle),
}

#[derive(Debug, Clone)]
pub struct RuntimeTaskEvent {
    pub task_id: Uuid,
    pub attempt_no: i32,
    pub event_type: String,
    pub event_level: String,
    pub message: String,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub struct RuntimeTaskLogBatch {
    pub task_id: Uuid,
    pub attempt_no: i32,
    pub stream: String,
    pub lines: Vec<String>,
    pub source_line_count: usize,
}

#[derive(Debug, Clone)]
pub struct RuntimeTaskProgress {
    pub task_id: Uuid,
    pub attempt_no: i32,
    pub frame: u64,
    pub fps: f64,
    pub bitrate_kbps: f64,
    pub speed: f64,
    pub out_time_ms: u64,
    pub dup_frames: u64,
    pub drop_frames: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RuntimeLogKey {
    task_id: Uuid,
    attempt_no: i32,
    stream: String,
}

impl RuntimeLogKey {
    fn from_batch(batch: &RuntimeTaskLogBatch) -> Self {
        Self {
            task_id: batch.task_id,
            attempt_no: batch.attempt_no,
            stream: batch.stream.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeEventSink {
    priority_tx: mpsc::UnboundedSender<RuntimeNotification>,
    log_tx: mpsc::Sender<RuntimeTaskLogBatch>,
    suppressed_logs: Arc<RwLock<HashMap<RuntimeLogKey, usize>>>,
}

impl RuntimeEventSink {
    pub fn new(
        priority_tx: mpsc::UnboundedSender<RuntimeNotification>,
        log_tx: mpsc::Sender<RuntimeTaskLogBatch>,
    ) -> Self {
        Self {
            priority_tx,
            log_tx,
            suppressed_logs: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn send(&self, notification: RuntimeNotification) -> Result<(), ()> {
        match notification {
            RuntimeNotification::TaskLogBatch(batch) => self.send_log_batch(batch),
            notification => self.priority_tx.send(notification).map_err(|_| ()),
        }
    }

    fn send_log_batch(&self, mut batch: RuntimeTaskLogBatch) -> Result<(), ()> {
        let key = RuntimeLogKey::from_batch(&batch);
        let suppressed = self
            .suppressed_logs
            .write()
            .expect("suppressed logs lock poisoned")
            .remove(&key)
            .unwrap_or(0);
        if suppressed > 0 {
            batch.lines.insert(
                0,
                format!("suppressed {suppressed} {} log lines", batch.stream),
            );
        }

        match self.log_tx.try_send(batch) {
            Ok(()) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Full(batch)) => {
                let mut suppressed_logs = self
                    .suppressed_logs
                    .write()
                    .expect("suppressed logs lock poisoned");
                *suppressed_logs.entry(key).or_insert(0) += suppressed + batch.source_line_count;
                Ok(())
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => Err(()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TerminalRuntimeReplay {
    pub handle: RuntimeHandle,
    pub event: RuntimeTaskEvent,
}

impl ManagedProcessExecutor {
    pub fn new(
        settings: AgentSettings,
        registry: LocalRuntimeRegistry,
        events: RuntimeEventSink,
    ) -> Self {
        Self {
            settings,
            registry,
            events,
            runtimes: Arc::new(RwLock::new(HashMap::new())),
            http_client: Client::builder()
                .timeout(Duration::from_secs(3))
                .build()
                .expect("failed to build runtime HTTP client"),
        }
    }
}

impl LocalExecutor for ManagedProcessExecutor {
    fn start_task(&self, request: &StartTaskRequest) -> Result<RuntimeHandle, ExecutorError> {
        if request.lease_token.trim().is_empty() {
            return Err(ExecutorError::InvalidRequest(
                "lease_token must not be empty".to_string(),
            ));
        }

        if self.settings.max_runtime_slots > 0 {
            let active_runtimes = u32::try_from(self.registry.count()).unwrap_or(u32::MAX);
            if active_runtimes >= self.settings.max_runtime_slots {
                return Err(ExecutorError::InvalidRequest(format!(
                    "max_runtime_slots exhausted: {active_runtimes}/{}",
                    self.settings.max_runtime_slots
                )));
            }
        }

        let spec = parse_task_spec(request)?;
        match task_runtime_mode(&spec) {
            TaskRuntimeMode::ZlmProxy => self.start_live_relay_task(request),
            TaskRuntimeMode::ZlmRtpServer => self.start_rtp_receive_task(request),
            TaskRuntimeMode::ManagedProcess => self.start_process_task(request),
        }
    }

    fn stop_task(&self, request: &StopTaskRequest) -> Result<(), ExecutorError> {
        let handle = self
            .registry
            .find_by_task_attempt(request.task_id, request.attempt_no)
            .ok_or(ExecutorError::RuntimeNotFound {
                task_id: request.task_id,
                attempt_no: request.attempt_no,
            })?;
        let runtime = self
            .runtimes
            .read()
            .expect("runtime map lock poisoned")
            .get(&handle.runtime_id)
            .cloned()
            .ok_or(ExecutorError::RuntimeNotFound {
                task_id: request.task_id,
                attempt_no: request.attempt_no,
            })?;

        runtime.stop_requested.store(true, Ordering::Relaxed);
        let registry = self.registry.clone();
        let runtime_id = handle.runtime_id;
        let reason = request.reason.clone();
        let grace_period_sec = request.grace_period_sec;
        let force_after_sec = request.force_after_sec;
        registry.update(runtime_id, |runtime| {
            runtime.state = RuntimeState::Stopping;
            runtime.last_progress_at = Some(Utc::now());
            runtime.metadata["stop"] = json!({
                "reason": reason,
                "grace_period_sec": grace_period_sec,
                "force_after_sec": force_after_sec,
            });
            if let Some(mut recording) = runtime
                .metadata
                .get("recording")
                .cloned()
                .and_then(|value| serde_json::from_value::<LiveRelayRecording>(value).ok())
            {
                recording.started = false;
                runtime.metadata["recording"] = json!(recording);
            }
        });

        if let Some(pid) = runtime.pid {
            signal_pid(pid, libc::SIGTERM)
                .map_err(|error| ExecutorError::ProcessSignal(error.to_string()))?;
        } else if matches!(
            task_runtime_mode_from_handle(&handle),
            Some(TaskRuntimeMode::ZlmProxy)
        ) {
            self.stop_live_relay_recording(&handle)?;
            self.close_live_relay(&handle, true)?;
        } else if matches!(
            task_runtime_mode_from_handle(&handle),
            Some(TaskRuntimeMode::ZlmRtpServer)
        ) {
            let stopping_handle = self.registry.get(runtime_id).unwrap_or(handle.clone());
            let work_dir = attempt_work_dir(&self.settings, request.task_id, request.attempt_no);
            let _ = persist_runtime_state(
                &work_dir,
                &stopping_handle,
                &success_check_from_handle(&stopping_handle),
            );
            self.close_rtp_receive(&stopping_handle)?;
            self.runtimes
                .write()
                .expect("runtime map lock poisoned")
                .remove(&runtime_id);
            let exited_handle = self
                .registry
                .update(runtime_id, |runtime| {
                    runtime.state = RuntimeState::Exited;
                    runtime.last_progress_at = Some(Utc::now());
                    runtime.metadata["stream_online"] = json!(false);
                })
                .unwrap_or_else(|| {
                    let mut handle = stopping_handle.clone();
                    handle.state = RuntimeState::Exited;
                    handle.last_progress_at = Some(Utc::now());
                    handle.metadata["stream_online"] = json!(false);
                    handle
                });
            let _ = persist_runtime_state(
                &work_dir,
                &exited_handle,
                &success_check_from_handle(&exited_handle),
            );
            let _ = self
                .events
                .send(RuntimeNotification::TaskSnapshot(exited_handle.clone()));
            let _ = self
                .events
                .send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                    task_id: exited_handle.task_id,
                    attempt_no: exited_handle.attempt_no,
                    event_type: "canceled".to_string(),
                    event_level: "info".to_string(),
                    message: "stream_ingest rtp server stopped".to_string(),
                    payload: json!({
                        "runtime_id": exited_handle.runtime_id,
                        "rtp_stream_id": rtp_stream_id_from_handle(&exited_handle),
                        "reason": request.reason,
                    }),
                }));
            let _ = self.registry.remove(runtime_id);
            return Ok(());
        }
        if let Some(handle) = self
            .registry
            .find_by_task_attempt(request.task_id, request.attempt_no)
        {
            let work_dir = attempt_work_dir(&self.settings, request.task_id, request.attempt_no);
            let _ = persist_runtime_state(&work_dir, &handle, &success_check_from_handle(&handle));
        }

        if let Some(pid) = runtime.pid {
            let runtimes = self.runtimes.clone();
            tokio::spawn(async move {
                if force_after_sec == 0 {
                    return;
                }
                sleep(Duration::from_secs(force_after_sec as u64)).await;
                let still_running = runtimes
                    .read()
                    .expect("runtime map lock poisoned")
                    .contains_key(&runtime_id);
                if still_running {
                    let _ = signal_pid(pid, libc::SIGKILL);
                }
            });
        }

        Ok(())
    }

    fn adopt_orphans(&self, filter: &AdoptFilter) -> Vec<RuntimeHandle> {
        let mut snapshots = self.registry.snapshots(filter);
        let mut seen = snapshots
            .iter()
            .map(|handle| (handle.task_id, handle.attempt_no))
            .collect::<HashSet<_>>();

        for persisted in scan_persisted_runtimes(&self.settings.work_root) {
            let key = (persisted.handle.task_id, persisted.handle.attempt_no);
            if seen.contains(&key) || !filter.matches(&persisted.handle) {
                continue;
            }

            if let Some(pid) = persisted.handle.pid {
                if !is_pid_running(pid) {
                    continue;
                }

                let mut handle = persisted.handle.clone();
                handle.state = RuntimeState::Orphaned;
                handle.metadata["orphaned"] = json!(true);

                self.registry.track(handle.clone());
                self.runtimes
                    .write()
                    .expect("runtime map lock poisoned")
                    .insert(
                        handle.runtime_id,
                        ManagedRuntime {
                            pid: Some(pid),
                            stop_requested: Arc::new(AtomicBool::new(false)),
                        },
                    );
                let _ =
                    persist_runtime_state(&persisted.work_dir, &handle, &persisted.success_check);
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
                        self.settings.clone(),
                        self.http_client.clone(),
                        self.registry.clone(),
                        self.runtimes.clone(),
                        self.events.clone(),
                    );
                }
                spawn_adopted_runtime_monitor(
                    handle.clone(),
                    persisted.work_dir,
                    persisted.success_check,
                    self.registry.clone(),
                    self.runtimes.clone(),
                    self.events.clone(),
                );
                snapshots.push(handle);
                seen.insert(key);
                continue;
            }

            match task_runtime_mode_from_handle(&persisted.handle) {
                Some(TaskRuntimeMode::ZlmRtpServer) => {
                    let Some(rtp_server) = rtp_server_from_handle(&persisted.handle) else {
                        continue;
                    };

                    if let Ok(Some(local_port)) =
                        self.rtp_server_port_blocking(&rtp_server.stream_id)
                    {
                        let mut handle = persisted.handle.clone();
                        handle.state = RuntimeState::Orphaned;
                        handle.metadata["orphaned"] = json!(true);
                        handle.metadata["rtp_server"] = json!(RtpServerMetadata {
                            local_port,
                            ..rtp_server.clone()
                        });

                        self.registry.track(handle.clone());
                        self.runtimes
                            .write()
                            .expect("runtime map lock poisoned")
                            .insert(
                                handle.runtime_id,
                                ManagedRuntime {
                                    pid: None,
                                    stop_requested: Arc::new(AtomicBool::new(false)),
                                },
                            );
                        let _ = persist_runtime_state(
                            &persisted.work_dir,
                            &handle,
                            &persisted.success_check,
                        );
                        let _ =
                            self.events
                                .send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                                    task_id: handle.task_id,
                                    attempt_no: handle.attempt_no,
                                    event_type: "adopted".to_string(),
                                    event_level: "info".to_string(),
                                    message: "reattached persisted stream_ingest rtp runtime"
                                        .to_string(),
                                    payload: json!({
                                        "runtime_id": handle.runtime_id,
                                        "orphaned": true,
                                        "rtp_stream_id": rtp_server.stream_id,
                                        "local_port": local_port,
                                        "re_use_port": rtp_server.reuse_port,
                                        "ssrc": rtp_server.ssrc,
                                    }),
                                }));
                        spawn_rtp_receive_monitor(
                            handle.runtime_id,
                            persisted.work_dir,
                            rtp_server.stream_id,
                            self.settings.clone(),
                            self.http_client.clone(),
                            self.registry.clone(),
                            self.runtimes.clone(),
                            self.events.clone(),
                        );
                        snapshots.push(handle);
                        seen.insert(key);
                        continue;
                    }

                    let Ok(request) = restart_request_from_handle(&persisted.handle) else {
                        continue;
                    };
                    let Ok(handle) = self.start_rtp_receive_task(&request) else {
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

            if self
                .zlm_stream_online_blocking(&startup_probe)
                .unwrap_or(false)
            {
                let mut handle = persisted.handle.clone();
                handle.state = RuntimeState::Orphaned;
                handle.metadata["orphaned"] = json!(true);
                handle.metadata["stream_online"] = json!(true);
                handle.metadata["stream_binding"] = json!({
                    "schema": startup_probe.schema,
                    "vhost": startup_probe.vhost,
                    "app": startup_probe.app,
                    "stream": startup_probe.stream,
                });

                self.registry.track(handle.clone());
                self.runtimes
                    .write()
                    .expect("runtime map lock poisoned")
                    .insert(
                        handle.runtime_id,
                        ManagedRuntime {
                            pid: None,
                            stop_requested: Arc::new(AtomicBool::new(false)),
                        },
                    );
                let _ =
                    persist_runtime_state(&persisted.work_dir, &handle, &persisted.success_check);
                let _ = self
                    .events
                    .send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                        task_id: handle.task_id,
                        attempt_no: handle.attempt_no,
                        event_type: "adopted".to_string(),
                        event_level: "info".to_string(),
                        message: "reattached persisted stream_ingest runtime".to_string(),
                        payload: json!({
                            "runtime_id": handle.runtime_id,
                            "orphaned": true,
                            "vhost": startup_probe.vhost,
                            "app": startup_probe.app,
                            "stream": startup_probe.stream,
                        }),
                    }));
                spawn_live_relay_monitor(
                    handle.runtime_id,
                    persisted.work_dir,
                    startup_probe,
                    self.settings.clone(),
                    self.http_client.clone(),
                    self.registry.clone(),
                    self.runtimes.clone(),
                    self.events.clone(),
                );
                snapshots.push(handle);
                seen.insert(key);
                continue;
            }

            let Ok(request) = restart_request_from_handle(&persisted.handle) else {
                continue;
            };
            let Ok(handle) = self.start_live_relay_task(&request) else {
                continue;
            };
            snapshots.push(handle);
            seen.insert(key);
        }

        snapshots
    }
}

impl ManagedProcessExecutor {
    fn start_process_task(
        &self,
        request: &StartTaskRequest,
    ) -> Result<RuntimeHandle, ExecutorError> {
        let plan = build_process_plan(&self.settings, request)?;
        prepare_plan_paths(&plan)?;

        let command_line = render_command_line(&plan.executable, &plan.args);
        let mut child = Command::new(&plan.executable);
        child
            .args(&plan.args)
            .current_dir(&plan.work_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = child
            .spawn()
            .map_err(|error| ExecutorError::ProcessSpawn(error.to_string()))?;
        let pid = child
            .id()
            .map(|pid| pid as i32)
            .ok_or_else(|| ExecutorError::ProcessSpawn("spawned child has no pid".to_string()))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let runtime_id = Uuid::now_v7();
        let stop_requested = Arc::new(AtomicBool::new(false));
        let require_stream_online = plan.startup_probe.is_some();

        let handle = RuntimeHandle {
            runtime_id,
            task_id: request.task_id,
            attempt_no: request.attempt_no,
            worker_kind: WorkerKind::ZlmProxy,
            pid: Some(pid),
            started_at: Utc::now(),
            last_progress_at: None,
            state: RuntimeState::Starting,
            command_line: Some(command_line),
            outputs: plan.outputs.clone(),
            metadata: json!({
                "task_type": request.task_type,
                "execution_mode": request.execution_mode,
                "lease_token": request.lease_token,
                "trace_context": request.trace_context,
                "resolved_spec": request.resolved_spec,
                "work_dir": plan.work_dir,
                "output_target": plan.output_target,
                "outputs": plan.outputs,
                "startup_probe": plan.startup_probe,
                "stream_online": plan.startup_probe.is_none(),
                "recording": plan.recording,
                "managed_file_output_kind": plan.managed_file_output_kind,
            }),
        };
        self.registry.track(handle.clone());
        persist_runtime_state(&plan.work_dir, &handle, &plan.success_check)?;

        self.runtimes
            .write()
            .expect("runtime map lock poisoned")
            .insert(
                runtime_id,
                ManagedRuntime {
                    pid: Some(pid),
                    stop_requested: stop_requested.clone(),
                },
            );

        if let Some(stdout) = stdout {
            let events = self.events.clone();
            let registry = self.registry.clone();
            tokio::spawn(async move {
                read_progress_stream(
                    stdout,
                    runtime_id,
                    handle.task_id,
                    handle.attempt_no,
                    registry,
                    events,
                    require_stream_online,
                )
                .await;
            });
        }
        if let Some(stderr) = stderr {
            let events = self.events.clone();
            tokio::spawn(async move {
                read_log_stream(
                    stderr,
                    handle.task_id,
                    handle.attempt_no,
                    "stderr".to_string(),
                    events,
                )
                .await;
            });
        }

        if let Some(startup_probe) = plan.startup_probe.clone() {
            spawn_startup_probe_monitor(
                runtime_id,
                plan.work_dir.clone(),
                plan.success_check.clone(),
                startup_probe,
                self.settings.clone(),
                self.http_client.clone(),
                self.registry.clone(),
                self.runtimes.clone(),
                self.events.clone(),
            );
        } else {
            let running_handle = self
                .registry
                .update(runtime_id, |runtime| {
                    runtime.state = RuntimeState::Running;
                })
                .unwrap_or_else(|| handle.clone());
            persist_runtime_state(&plan.work_dir, &running_handle, &plan.success_check)?;
            let _ = self
                .events
                .send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                    task_id: running_handle.task_id,
                    attempt_no: running_handle.attempt_no,
                    event_type: "running".to_string(),
                    event_level: "info".to_string(),
                    message: "child process started".to_string(),
                    payload: json!({
                        "runtime_id": running_handle.runtime_id,
                        "pid": running_handle.pid,
                    }),
                }));
            let _ = self
                .events
                .send(RuntimeNotification::TaskSnapshot(running_handle.clone()));
        }

        let registry = self.registry.clone();
        let events = self.events.clone();
        let runtimes = self.runtimes.clone();
        let work_dir = plan.work_dir.clone();
        let output_target = plan.output_target.clone();
        let success_check = plan.success_check.clone();
        let wait_handle = handle.clone();
        let restart_executor = self.clone();
        tokio::spawn(async move {
            let status = child.wait().await;
            let was_stopped = stop_requested.load(Ordering::Relaxed);
            runtimes
                .write()
                .expect("runtime map lock poisoned")
                .remove(&runtime_id);

            let mut exited_handle = registry
                .update(runtime_id, |runtime| {
                    runtime.state = RuntimeState::Exited;
                    runtime.last_progress_at = Some(Utc::now());
                })
                .unwrap_or_else(|| RuntimeHandle {
                    runtime_id,
                    task_id: wait_handle.task_id,
                    attempt_no: wait_handle.attempt_no,
                    worker_kind: wait_handle.worker_kind,
                    pid: wait_handle.pid,
                    started_at: wait_handle.started_at,
                    last_progress_at: Some(Utc::now()),
                    state: RuntimeState::Exited,
                    command_line: wait_handle.command_line.clone(),
                    outputs: wait_handle.outputs.clone(),
                    metadata: wait_handle.metadata.clone(),
                });

            attach_file_artifact_metadata(&mut exited_handle, &success_check);

            if should_auto_restart_process(&exited_handle, was_stopped, &status) {
                let _ = persist_runtime_state(&work_dir, &exited_handle, &success_check);
                let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                    task_id: exited_handle.task_id,
                    attempt_no: exited_handle.attempt_no,
                    event_type: "recovering".to_string(),
                    event_level: "warn".to_string(),
                    message:
                        "managed process exited after stream was online; attempting local recovery"
                            .to_string(),
                    payload: json!({
                        "exit_code": status.as_ref().ok().and_then(|value| value.code()),
                        "output_target": output_target,
                        "task_type": task_type_from_handle(&exited_handle),
                    }),
                }));
                let _ = registry.remove(runtime_id);

                if restart_executor
                    .restart_process_task_after_failure(&exited_handle)
                    .await
                    .is_ok()
                {
                    return;
                }
            }

            let completion_reason = completion_reason_from_handle(&exited_handle);
            let fatal_recording_error = fatal_recording_error_from_handle(&exited_handle);
            let (event_type, event_level, message, payload) = match status {
                Ok(status)
                    if was_stopped
                        && completion_reason.as_deref() == Some("record_duration_reached") =>
                {
                    (
                        "succeeded",
                        "info",
                        "child process completed after recording duration reached".to_string(),
                        json!({
                            "exit_code": status.code(),
                            "output_target": output_target,
                            "reason": "record_duration_reached",
                        }),
                    )
                }
                Ok(status) if was_stopped => (
                    "canceled",
                    "info",
                    "child process stopped".to_string(),
                    json!({
                        "exit_code": status.code(),
                        "output_target": output_target,
                    }),
                ),
                Ok(status) if fatal_recording_error.is_some() => (
                    "failed",
                    "error",
                    format!(
                        "child process stopped after recording startup failed: {}",
                        fatal_recording_error
                            .as_deref()
                            .unwrap_or("unknown recording error")
                    ),
                    json!({
                        "exit_code": status.code(),
                        "output_target": output_target,
                        "recording_error": fatal_recording_error,
                    }),
                ),
                Ok(status)
                    if status.success()
                        && requires_stream_online(&exited_handle)
                        && !stream_online(&exited_handle) =>
                {
                    (
                        "failed",
                        "error",
                        "child process exited before ZLM stream became online".to_string(),
                        json!({
                            "exit_code": status.code(),
                            "output_target": output_target,
                        }),
                    )
                }
                Ok(status) if status.success() => match &success_check {
                    SuccessCheck::FileExists(path) if path.exists() => (
                        "succeeded",
                        "info",
                        "child process completed".to_string(),
                        json!({
                            "exit_code": status.code(),
                            "output_target": output_target,
                        }),
                    ),
                    SuccessCheck::FileExists(path) => (
                        "failed",
                        "error",
                        format!(
                            "child process finished without artifact: {}",
                            path.display()
                        ),
                        json!({
                            "exit_code": status.code(),
                            "output_target": output_target,
                        }),
                    ),
                    SuccessCheck::ProcessExit => (
                        "succeeded",
                        "info",
                        "child process completed".to_string(),
                        json!({
                            "exit_code": status.code(),
                            "output_target": output_target,
                        }),
                    ),
                },
                Ok(status) => (
                    "failed",
                    "error",
                    format!("child process exited unsuccessfully: {:?}", status.code()),
                    json!({
                        "exit_code": status.code(),
                        "output_target": output_target,
                    }),
                ),
                Err(error) if fatal_recording_error.is_some() => (
                    "failed",
                    "error",
                    format!(
                        "child process stopped after recording startup failed: {}",
                        fatal_recording_error
                            .as_deref()
                            .unwrap_or("unknown recording error")
                    ),
                    json!({
                        "output_target": output_target,
                        "recording_error": fatal_recording_error,
                        "wait_error": error.to_string(),
                    }),
                ),
                Err(error) => (
                    "failed",
                    "error",
                    format!("failed to wait child process: {error}"),
                    json!({
                        "output_target": output_target,
                    }),
                ),
            };

            let _ = persist_runtime_state(&work_dir, &exited_handle, &success_check);
            let _ = events.send(RuntimeNotification::TaskSnapshot(exited_handle.clone()));
            let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                task_id: exited_handle.task_id,
                attempt_no: exited_handle.attempt_no,
                event_type: event_type.to_string(),
                event_level: event_level.to_string(),
                message,
                payload,
            }));

            let _ = registry.remove(runtime_id);
        });

        Ok(handle)
    }

    async fn restart_process_task_after_failure(
        &self,
        exited_handle: &RuntimeHandle,
    ) -> Result<RuntimeHandle, ExecutorError> {
        wait_for_zlm_api_ready(
            &self.http_client,
            &self.settings,
            PROCESS_RECOVERY_WAIT_TIMEOUT,
        )
        .await;

        let request = restart_request_from_handle(exited_handle)?;
        let restarted = self.start_process_task(&request)?;
        let _ = self
            .events
            .send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                task_id: restarted.task_id,
                attempt_no: restarted.attempt_no,
                event_type: "starting".to_string(),
                event_level: "info".to_string(),
                message: "runtime handle recreated after local recovery".to_string(),
                payload: json!({
                    "runtime_id": restarted.runtime_id,
                    "worker_kind": restarted.worker_kind,
                    "recovered": true,
                }),
            }));
        let _ = self
            .events
            .send(RuntimeNotification::TaskSnapshot(restarted.clone()));
        Ok(restarted)
    }

    fn start_live_relay_task(
        &self,
        request: &StartTaskRequest,
    ) -> Result<RuntimeHandle, ExecutorError> {
        let spec = parse_task_spec(request)?;
        let plan = build_live_relay_plan(&self.settings, request, &spec)?;
        prepare_work_dir(&plan.work_dir)?;

        let response = self.call_zlm_api_sync(
            "/index/api/addStreamProxy",
            &build_live_relay_api_params(
                &self.settings,
                &spec,
                &plan.startup_probe,
                &plan.input_url,
            ),
        )?;
        let proxy_key = extract_zlm_proxy_key(&response);
        let runtime_id = Uuid::now_v7();
        let stop_requested = Arc::new(AtomicBool::new(false));
        let handle = RuntimeHandle {
            runtime_id,
            task_id: request.task_id,
            attempt_no: request.attempt_no,
            worker_kind: request.task_type.default_worker_kind(),
            pid: None,
            started_at: Utc::now(),
            last_progress_at: None,
            state: RuntimeState::Starting,
            command_line: Some(plan.command_line),
            outputs: plan.outputs.clone(),
            metadata: json!({
                "task_type": request.task_type,
                "execution_mode": request.execution_mode,
                "lease_token": request.lease_token,
                "trace_context": request.trace_context,
                "resolved_spec": request.resolved_spec,
                "work_dir": plan.work_dir,
                "output_target": plan.outputs.first(),
                "outputs": plan.outputs,
                "startup_probe": plan.startup_probe,
                "stream_online": false,
                "stream_binding": {
                    "schema": plan.startup_probe.schema,
                    "vhost": plan.startup_probe.vhost,
                    "app": plan.startup_probe.app,
                    "stream": plan.startup_probe.stream,
                },
                "recording": plan.recording,
                "zlm_proxy_key": proxy_key,
                "source_url": plan.input_url,
            }),
        };
        self.registry.track(handle.clone());
        persist_runtime_state(&plan.work_dir, &handle, &SuccessCheck::ProcessExit)?;
        self.runtimes
            .write()
            .expect("runtime map lock poisoned")
            .insert(
                runtime_id,
                ManagedRuntime {
                    pid: None,
                    stop_requested,
                },
            );
        let _ = self
            .events
            .send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                task_id: handle.task_id,
                attempt_no: handle.attempt_no,
                event_type: "zlm_proxy_created".to_string(),
                event_level: "info".to_string(),
                message: "stream_ingest proxy created in ZLM".to_string(),
                payload: json!({
                    "runtime_id": handle.runtime_id,
                    "vhost": plan.startup_probe.vhost,
                    "app": plan.startup_probe.app,
                    "stream": plan.startup_probe.stream,
                    "zlm_proxy_key": extract_zlm_proxy_key(&response),
                }),
            }));
        spawn_live_relay_monitor(
            runtime_id,
            plan.work_dir,
            plan.startup_probe,
            self.settings.clone(),
            self.http_client.clone(),
            self.registry.clone(),
            self.runtimes.clone(),
            self.events.clone(),
        );
        Ok(handle)
    }

    fn start_rtp_receive_task(
        &self,
        request: &StartTaskRequest,
    ) -> Result<RuntimeHandle, ExecutorError> {
        let spec = parse_task_spec(request)?;
        let plan = build_rtp_receive_plan(&self.settings, request, &spec)?;
        prepare_work_dir(&plan.work_dir)?;

        let response = self.call_zlm_api_sync(
            "/index/api/openRtpServer",
            &build_open_rtp_server_params(&plan),
        )?;
        let local_port = extract_zlm_local_port(&response).unwrap_or(plan.requested_port);
        let rtp_server = RtpServerMetadata {
            stream_id: plan.stream_id.clone(),
            local_port,
            requested_port: plan.requested_port,
            tcp_mode: plan.tcp_mode,
            reuse_port: plan.reuse_port,
            ssrc: plan.ssrc,
        };
        let runtime_id = Uuid::now_v7();
        let stop_requested = Arc::new(AtomicBool::new(false));
        let handle = RuntimeHandle {
            runtime_id,
            task_id: request.task_id,
            attempt_no: request.attempt_no,
            worker_kind: WorkerKind::ZlmRtpServer,
            pid: None,
            started_at: Utc::now(),
            last_progress_at: None,
            state: RuntimeState::Starting,
            command_line: Some(plan.command_line),
            outputs: plan.outputs.clone(),
            metadata: json!({
                "task_type": request.task_type,
                "execution_mode": request.execution_mode,
                "lease_token": request.lease_token,
                "trace_context": request.trace_context,
                "resolved_spec": request.resolved_spec,
                "work_dir": plan.work_dir,
                "output_target": plan.outputs.first(),
                "outputs": plan.outputs,
                "stream_online": false,
                "rtp_stream_id": rtp_server.stream_id,
                "rtp_server": rtp_server,
            }),
        };
        self.registry.track(handle.clone());
        persist_runtime_state(&plan.work_dir, &handle, &SuccessCheck::ProcessExit)?;
        self.runtimes
            .write()
            .expect("runtime map lock poisoned")
            .insert(
                runtime_id,
                ManagedRuntime {
                    pid: None,
                    stop_requested,
                },
            );
        let _ = self
            .events
            .send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                task_id: handle.task_id,
                attempt_no: handle.attempt_no,
                event_type: "rtp_server_opened".to_string(),
                event_level: "info".to_string(),
                message: "stream_ingest rtp server opened in ZLM".to_string(),
                payload: json!({
                    "runtime_id": handle.runtime_id,
                    "rtp_stream_id": handle.metadata["rtp_stream_id"],
                    "requested_port": plan.requested_port,
                    "local_port": local_port,
                    "tcp_mode": plan.tcp_mode,
                    "re_use_port": plan.reuse_port,
                    "ssrc": plan.ssrc,
                }),
            }));
        spawn_rtp_receive_monitor(
            runtime_id,
            plan.work_dir,
            plan.stream_id,
            self.settings.clone(),
            self.http_client.clone(),
            self.registry.clone(),
            self.runtimes.clone(),
            self.events.clone(),
        );
        Ok(handle)
    }

    fn close_live_relay(&self, handle: &RuntimeHandle, force: bool) -> Result<(), ExecutorError> {
        let binding = stream_binding_from_handle(handle).ok_or_else(|| {
            ExecutorError::InvalidRequest(
                "live_relay runtime is missing stream binding metadata".to_string(),
            )
        })?;
        let _ = self.call_zlm_api_sync(
            "/index/api/close_streams",
            &build_close_stream_params(&binding, force),
        )?;
        Ok(())
    }

    fn stop_live_relay_recording(&self, handle: &RuntimeHandle) -> Result<(), ExecutorError> {
        let Some(recording) = live_relay_recording_from_handle(handle) else {
            return Ok(());
        };
        if !recording.started {
            return Ok(());
        }
        let binding = stream_binding_from_handle(handle).ok_or_else(|| {
            ExecutorError::InvalidRequest(
                "live_relay runtime is missing stream binding metadata".to_string(),
            )
        })?;
        self.run_sync(stop_live_relay_recording(
            &self.http_client,
            &self.settings,
            &binding,
            &recording,
        ))
    }

    fn close_rtp_receive(&self, handle: &RuntimeHandle) -> Result<(), ExecutorError> {
        let stream_id = rtp_stream_id_from_handle(handle).ok_or_else(|| {
            ExecutorError::InvalidRequest(
                "rtp_receive runtime is missing rtp_stream_id metadata".to_string(),
            )
        })?;
        let _ = self.call_zlm_api_sync(
            "/index/api/closeRtpServer",
            &[("stream_id".to_string(), stream_id)],
        )?;
        Ok(())
    }

    fn zlm_stream_online_blocking(&self, target: &StartupProbe) -> Result<bool, ExecutorError> {
        self.run_sync(async {
            zlm_stream_online(&self.http_client, &self.settings, target)
                .await
                .map_err(|error| ExecutorError::ApiCall(error.to_string()))
        })
    }

    fn rtp_server_port_blocking(&self, stream_id: &str) -> Result<Option<u16>, ExecutorError> {
        self.run_sync(async {
            zlm_rtp_server_port(&self.http_client, &self.settings, stream_id).await
        })
    }

    fn call_zlm_api_sync(
        &self,
        path: &str,
        params: &[(String, String)],
    ) -> Result<Value, ExecutorError> {
        self.run_sync(call_zlm_api(
            &self.http_client,
            &self.settings,
            path,
            params,
        ))
    }

    fn run_sync<T>(
        &self,
        future: impl Future<Output = Result<T, ExecutorError>>,
    ) -> Result<T, ExecutorError> {
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => tokio::task::block_in_place(|| handle.block_on(future)),
            Err(_) => {
                let runtime = tokio::runtime::Runtime::new()
                    .map_err(|error| ExecutorError::ApiCall(error.to_string()))?;
                runtime.block_on(future)
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum ExecutorError {
    #[error("runtime {task_id}/{attempt_no} was not found")]
    RuntimeNotFound { task_id: Uuid, attempt_no: i32 },
    #[error("{0}")]
    InvalidRequest(String),
    #[error("ZLM API call failed: {0}")]
    ApiCall(String),
    #[error("failed to spawn process: {0}")]
    ProcessSpawn(String),
    #[error("failed to signal process: {0}")]
    ProcessSignal(String),
}

pub fn rejected_runtime_handle(request: &StartTaskRequest) -> RuntimeHandle {
    RuntimeHandle {
        runtime_id: Uuid::now_v7(),
        task_id: request.task_id,
        attempt_no: request.attempt_no,
        worker_kind: request.task_type.default_worker_kind(),
        pid: None,
        started_at: Utc::now(),
        last_progress_at: None,
        state: RuntimeState::Pending,
        command_line: None,
        outputs: Vec::new(),
        metadata: json!({
            "task_type": request.task_type,
            "execution_mode": request.execution_mode,
            "lease_token": request.lease_token,
            "trace_context": request.trace_context,
        }),
    }
}

pub fn dedup_worker_kinds(worker_kinds: Vec<WorkerKind>) -> Vec<WorkerKind> {
    let mut seen = HashSet::new();
    worker_kinds
        .into_iter()
        .filter(|kind| seen.insert(*kind))
        .collect()
}

async fn read_progress_stream(
    stdout: tokio::process::ChildStdout,
    runtime_id: Uuid,
    task_id: Uuid,
    attempt_no: i32,
    registry: LocalRuntimeRegistry,
    events: RuntimeEventSink,
    require_stream_online: bool,
) {
    let mut reader = BufReader::new(stdout).lines();
    let mut current = HashMap::<String, String>::new();

    while let Ok(Some(line)) = reader.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        current.insert(key.to_string(), value.to_string());
        if key == "progress" {
            if require_stream_online
                && !registry
                    .get(runtime_id)
                    .and_then(|runtime| {
                        runtime
                            .metadata
                            .get("stream_online")
                            .and_then(Value::as_bool)
                    })
                    .unwrap_or(false)
            {
                current.clear();
                continue;
            }
            let progress = RuntimeTaskProgress {
                task_id,
                attempt_no,
                frame: parse_u64(current.get("frame")),
                fps: parse_f64(current.get("fps")),
                bitrate_kbps: parse_bitrate_kbps(current.get("bitrate")),
                speed: parse_speed(current.get("speed")),
                out_time_ms: parse_u64(current.get("out_time_ms")),
                dup_frames: parse_u64(current.get("dup_frames")),
                drop_frames: parse_u64(current.get("drop_frames")),
            };
            let _ = registry.update(runtime_id, |runtime| {
                runtime.last_progress_at = Some(Utc::now());
                runtime.state = RuntimeState::Running;
            });
            let _ = events.send(RuntimeNotification::TaskProgress(progress));
            current.clear();
        }
    }
}

fn flush_log_batch(
    task_id: Uuid,
    attempt_no: i32,
    stream: &str,
    batch: &mut Vec<(String, usize)>,
    source_line_count: &mut usize,
    events: &RuntimeEventSink,
) {
    if batch.is_empty() {
        return;
    }

    let lines = batch
        .drain(..)
        .map(|(line, count)| match count {
            0 | 1 => line,
            count => format!("{line} (repeated {count} times)"),
        })
        .collect::<Vec<_>>();
    let emitted_line_count = *source_line_count;
    *source_line_count = 0;

    let _ = events.send(RuntimeNotification::TaskLogBatch(RuntimeTaskLogBatch {
        task_id,
        attempt_no,
        stream: stream.to_string(),
        lines,
        source_line_count: emitted_line_count,
    }));
}

async fn read_log_stream(
    stderr: tokio::process::ChildStderr,
    task_id: Uuid,
    attempt_no: i32,
    stream: String,
    events: RuntimeEventSink,
) {
    let mut reader = BufReader::new(stderr).lines();
    let mut batch = Vec::new();
    let mut source_line_count = 0usize;

    'outer: loop {
        let next_line = if batch.is_empty() {
            reader.next_line().await
        } else {
            match timeout(LOG_BATCH_FLUSH_INTERVAL, reader.next_line()).await {
                Ok(result) => result,
                Err(_) => {
                    flush_log_batch(
                        task_id,
                        attempt_no,
                        &stream,
                        &mut batch,
                        &mut source_line_count,
                        &events,
                    );
                    continue;
                }
            }
        };

        let Ok(line) = next_line else {
            break;
        };
        let Some(line) = line else {
            break;
        };
        let line = line.trim_end().to_string();
        if line.is_empty() {
            continue;
        }

        source_line_count += 1;
        if let Some((last_line, count)) = batch.last_mut() {
            if *last_line == line {
                *count += 1;
            } else {
                batch.push((line, 1));
            }
        } else {
            batch.push((line, 1));
        }

        if batch.len() >= MAX_LOG_BATCH_LINES || source_line_count >= MAX_LOG_BATCH_LINES {
            flush_log_batch(
                task_id,
                attempt_no,
                &stream,
                &mut batch,
                &mut source_line_count,
                &events,
            );
            continue 'outer;
        }
    }

    flush_log_batch(
        task_id,
        attempt_no,
        &stream,
        &mut batch,
        &mut source_line_count,
        &events,
    );
}

fn build_process_plan(
    settings: &AgentSettings,
    request: &StartTaskRequest,
) -> Result<ProcessPlan, ExecutorError> {
    let spec = parse_task_spec(request)?;

    match request.task_type {
        TaskType::FileTranscode => build_file_transcode_plan(settings, request, &spec),
        TaskType::StreamBridge => build_multicast_bridge_plan(settings, request, &spec),
        TaskType::StreamIngest => {
            if task_runtime_mode(&spec) != TaskRuntimeMode::ManagedProcess {
                return Err(ExecutorError::InvalidRequest(
                    "stream_ingest task should not run in the managed process executor".to_string(),
                ));
            }
            build_stream_ingest_plan(settings, request, &spec)
        }
    }
}

fn build_stream_ingest_plan(
    settings: &AgentSettings,
    request: &StartTaskRequest,
    spec: &TaskSpec,
) -> Result<ProcessPlan, ExecutorError> {
    match spec.stream_ingest_record_mode() {
        Some(StreamIngestRecordMode::Fast) => {
            build_stream_ingest_fast_record_plan(settings, request, spec)
        }
        _ => build_file_to_live_plan(settings, request, spec),
    }
}

fn parse_task_spec(request: &StartTaskRequest) -> Result<TaskSpec, ExecutorError> {
    serde_json::from_value(request.resolved_spec.clone()).map_err(|error| {
        ExecutorError::InvalidRequest(format!("invalid resolved_spec for task execution: {error}"))
    })
}

fn task_runtime_mode(spec: &TaskSpec) -> TaskRuntimeMode {
    match spec.task_type {
        TaskType::FileTranscode | TaskType::StreamBridge => TaskRuntimeMode::ManagedProcess,
        TaskType::StreamIngest => match (spec.input.kind, spec.input.source_mode) {
            (Some(InputKind::GbRtp), _) => TaskRuntimeMode::ZlmRtpServer,
            (Some(InputKind::Rtsp | InputKind::Rtmp | InputKind::HttpFlv), _) => {
                TaskRuntimeMode::ZlmProxy
            }
            (Some(InputKind::Hls | InputKind::HttpTs), Some(SourceMode::Live)) => {
                TaskRuntimeMode::ZlmProxy
            }
            _ => TaskRuntimeMode::ManagedProcess,
        },
    }
}

const ZLM_RECORD_HTTP_ROOT: &str = "/data/zlm/www/record";
const TRANSCODE_ARTIFACT_ROOT: &str = "/data/zlm/www/artifacts/transcode";
const BRIDGE_ARTIFACT_ROOT: &str = "/data/zlm/www/artifacts/bridge";
const STREAM_INGEST_RECORD_ARTIFACT_ROOT: &str = "/data/zlm/www/artifacts/stream-ingest-record";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ManagedFileOutputKind {
    Transcode,
    Bridge,
    StreamIngestRecord,
}

impl ManagedFileOutputKind {
    fn root(self) -> &'static str {
        match self {
            Self::Transcode => TRANSCODE_ARTIFACT_ROOT,
            Self::Bridge => BRIDGE_ARTIFACT_ROOT,
            Self::StreamIngestRecord => STREAM_INGEST_RECORD_ARTIFACT_ROOT,
        }
    }

    fn metadata_key(self) -> &'static str {
        match self {
            Self::Transcode => "transcode_artifact",
            Self::Bridge => "bridge_artifact",
            Self::StreamIngestRecord => "stream_ingest_record_artifacts",
        }
    }
}

fn managed_file_output_kind_for_task(task_type: TaskType) -> Option<ManagedFileOutputKind> {
    match task_type {
        TaskType::FileTranscode => Some(ManagedFileOutputKind::Transcode),
        TaskType::StreamBridge => Some(ManagedFileOutputKind::Bridge),
        _ => None,
    }
}

fn normalize_optional_publish_format(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase)
}

fn default_file_extension_for_format(format: &str) -> String {
    match format {
        "hls" => "m3u8".to_string(),
        "mp4" => "mp4".to_string(),
        "flv" => "flv".to_string(),
        "mpegts" | "rtp_mpegts" => "ts".to_string(),
        "matroska" => "mkv".to_string(),
        "mov" => "mov".to_string(),
        "webm" => "webm".to_string(),
        other => {
            let sanitized: String = other
                .chars()
                .filter(|value| {
                    value.is_ascii_alphanumeric() || matches!(value, '.' | '_' | '+' | '-')
                })
                .collect();
            if sanitized.is_empty() {
                "bin".to_string()
            } else {
                sanitized
            }
        }
    }
}

fn allocate_managed_output(
    kind: ManagedFileOutputKind,
    requested_format: Option<&str>,
) -> PublishOutput {
    let format =
        normalize_optional_publish_format(requested_format).unwrap_or_else(|| "mp4".to_string());
    let extension = default_file_extension_for_format(&format);
    let timestamp = Local::now().naive_local();
    let relative_dir = timestamp.format("%Y/%m/%d").to_string();
    let file_stem = timestamp.format("%H%M%S").to_string();
    let dir = PathBuf::from(kind.root()).join(relative_dir);
    let mut path = dir.join(format!("{file_stem}.{extension}"));
    let mut suffix = 1_u32;
    while path.exists() {
        path = dir.join(format!("{file_stem}-{suffix:02}.{extension}"));
        suffix += 1;
    }

    let target = path.to_string_lossy().to_string();
    PublishOutput {
        success_check: SuccessCheck::FileExists(PathBuf::from(&target)),
        target,
        format,
    }
}

fn hls_segment_template(playlist_path: &str) -> String {
    let path = Path::new(playlist_path);
    let parent = path
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("segment");
    parent
        .join(format!("{stem}-%05d.ts"))
        .to_string_lossy()
        .to_string()
}

fn allocate_managed_file_output(
    kind: ManagedFileOutputKind,
    publish: &PublishSpec,
) -> Result<PublishOutput, ExecutorError> {
    if publish
        .url
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
    {
        return Err(ExecutorError::InvalidRequest(
            "publish.url must not be provided for file output; output path is managed by the platform".to_string(),
        ));
    }

    Ok(allocate_managed_output(kind, publish.format.as_deref()))
}

fn build_file_transcode_plan(
    settings: &AgentSettings,
    request: &StartTaskRequest,
    spec: &TaskSpec,
) -> Result<ProcessPlan, ExecutorError> {
    let input_url = build_input_url(settings, &spec.input)?;

    let work_dir = attempt_work_dir(settings, request.task_id, request.attempt_no);
    let output = match spec.publish.kind {
        Some(PublishTargetKind::File) => {
            allocate_managed_file_output(ManagedFileOutputKind::Transcode, &spec.publish)?
        }
        Some(_) => {
            return Err(ExecutorError::InvalidRequest(
                "file_transcode requires publish.kind=file".to_string(),
            ));
        }
        None => {
            return Err(ExecutorError::InvalidRequest(
                "file_transcode requires publish.kind".to_string(),
            ));
        }
    };
    let mut args = ffmpeg_base_args(input_url.clone(), false);
    let audio_copy_decoration = append_process_args(
        &mut args,
        settings,
        spec,
        "copy_or_transcode",
        input_url.as_str(),
        output.format.as_str(),
        VideoOutputPolicy::KeepSourceFamily,
        AudioOutputPolicy::Aac,
    )?;
    if let Some(filter) =
        audio_copy_decoration.and_then(|value| value.filter_for_output(output.format.as_str()))
    {
        append_audio_bitstream_filter_arg(&mut args, filter);
    }

    args.extend([
        "-threads".to_string(),
        "0".to_string(),
        "-f".to_string(),
        output.format.clone(),
        output.target.clone(),
    ]);

    Ok(ProcessPlan {
        executable: settings.ffmpeg_bin.clone(),
        args,
        work_dir,
        output_target: output.target.clone(),
        outputs: vec![output.target.clone()],
        success_check: output.success_check,
        startup_probe: None,
        recording: None,
        managed_file_output_kind: Some(ManagedFileOutputKind::Transcode),
    })
}

fn build_file_to_live_plan(
    settings: &AgentSettings,
    request: &StartTaskRequest,
    spec: &TaskSpec,
) -> Result<ProcessPlan, ExecutorError> {
    let input_url = build_input_url(settings, &spec.input)?;
    let work_dir = attempt_work_dir(settings, request.task_id, request.attempt_no);
    let startup_probe = build_startup_probe(request.task_id, spec)?;
    let publish_output = build_internal_stream_output(settings, &startup_probe);
    let mut outputs = vec![publish_output.target.clone()];
    let mut success_check = publish_output.success_check.clone();
    let mut recording = None;

    let mut args = ffmpeg_base_args(input_url.clone(), true);
    if should_loop_file_to_live_input(spec) {
        insert_ffmpeg_input_args(
            &mut args,
            vec!["-stream_loop".to_string(), "-1".to_string()],
        );
    }
    let audio_copy_decoration = append_process_args(
        &mut args,
        settings,
        spec,
        "copy_or_transcode",
        input_url.as_str(),
        publish_output.format.as_str(),
        VideoOutputPolicy::ForceH264,
        AudioOutputPolicy::Aac,
    )?;
    args.extend(["-threads".to_string(), "0".to_string()]);
    if let Some(duration_sec) = spec.record.duration_sec {
        args.extend(["-t".to_string(), duration_sec.to_string()]);
    }

    if spec.record.enabled.unwrap_or(false) {
        match spec
            .record
            .format
            .unwrap_or(media_domain::RecordFormat::Mp4)
        {
            media_domain::RecordFormat::Mp4 => {
                let record_format = "mp4".to_string();
                let record_path = work_dir.join("record.mp4").to_string_lossy().to_string();
                let tee_target = format!(
                    "{}|{}",
                    tee_slave_target(
                        publish_output.format.as_str(),
                        &publish_output.target,
                        true,
                        audio_copy_decoration,
                    ),
                    tee_slave_target(
                        record_format.as_str(),
                        &record_path,
                        true,
                        audio_copy_decoration,
                    ),
                );
                args.extend(["-f".to_string(), "tee".to_string(), tee_target]);
                outputs.push(record_path.clone());
                success_check = SuccessCheck::FileExists(PathBuf::from(record_path));
            }
            media_domain::RecordFormat::Hls | media_domain::RecordFormat::Both => {
                if let Some(filter) = audio_copy_decoration
                    .and_then(|value| value.filter_for_output(publish_output.format.as_str()))
                {
                    append_audio_bitstream_filter_arg(&mut args, filter);
                }
                args.extend([
                    "-f".to_string(),
                    publish_output.format.clone(),
                    publish_output.target.clone(),
                ]);
                recording = build_live_relay_recording(spec, &work_dir)?;
                if let Some(recording_plan) = &recording {
                    outputs.push(recording_plan.root_path.clone());
                }
            }
        }
    } else {
        if let Some(filter) = audio_copy_decoration
            .and_then(|value| value.filter_for_output(publish_output.format.as_str()))
        {
            append_audio_bitstream_filter_arg(&mut args, filter);
        }
        args.extend([
            "-f".to_string(),
            publish_output.format.clone(),
            publish_output.target.clone(),
        ]);
    }

    Ok(ProcessPlan {
        executable: settings.ffmpeg_bin.clone(),
        args,
        work_dir,
        output_target: publish_output.target,
        outputs,
        success_check,
        startup_probe: Some(startup_probe),
        recording,
        managed_file_output_kind: None,
    })
}

fn build_stream_ingest_fast_record_plan(
    settings: &AgentSettings,
    request: &StartTaskRequest,
    spec: &TaskSpec,
) -> Result<ProcessPlan, ExecutorError> {
    let input_url = build_input_url(settings, &spec.input)?;
    let work_dir = attempt_work_dir(settings, request.task_id, request.attempt_no);
    let mut args = ffmpeg_base_args_without_maps(input_url.clone(), false);
    let preferred_output_format = match spec
        .record
        .format
        .unwrap_or(media_domain::RecordFormat::Mp4)
    {
        media_domain::RecordFormat::Mp4 | media_domain::RecordFormat::Both => "mp4",
        media_domain::RecordFormat::Hls => "hls",
    };
    if should_loop_file_to_live_input(spec) {
        insert_ffmpeg_input_args(
            &mut args,
            vec!["-stream_loop".to_string(), "-1".to_string()],
        );
    }
    let audio_copy_decoration = append_process_args(
        &mut args,
        settings,
        spec,
        "copy_or_transcode",
        input_url.as_str(),
        preferred_output_format,
        VideoOutputPolicy::KeepSourceFamily,
        AudioOutputPolicy::Aac,
    )?;
    args.extend(["-threads".to_string(), "0".to_string()]);
    if let Some(duration_sec) = spec.record.duration_sec {
        args.extend(["-t".to_string(), duration_sec.to_string()]);
    }

    let mut outputs = Vec::new();
    let primary_output = match spec
        .record
        .format
        .unwrap_or(media_domain::RecordFormat::Mp4)
    {
        media_domain::RecordFormat::Mp4 => {
            let output =
                allocate_managed_output(ManagedFileOutputKind::StreamIngestRecord, Some("mp4"));
            append_default_output_maps(&mut args);
            if let Some(filter) = audio_copy_decoration
                .and_then(|value| value.filter_for_output(output.format.as_str()))
            {
                append_audio_bitstream_filter_arg(&mut args, filter);
            }
            args.extend([
                "-f".to_string(),
                output.format.clone(),
                output.target.clone(),
            ]);
            outputs.push(output.target.clone());
            output
        }
        media_domain::RecordFormat::Hls => {
            let output =
                allocate_managed_output(ManagedFileOutputKind::StreamIngestRecord, Some("hls"));
            let segment_template = hls_segment_template(output.target.as_str());
            append_default_output_maps(&mut args);
            args.extend([
                "-f".to_string(),
                "hls".to_string(),
                "-hls_time".to_string(),
                spec.record.segment_sec.unwrap_or(6).to_string(),
                "-hls_list_size".to_string(),
                "0".to_string(),
                "-hls_segment_filename".to_string(),
                segment_template,
                output.target.clone(),
            ]);
            outputs.push(output.target.clone());
            output
        }
        media_domain::RecordFormat::Both => {
            let mp4_output =
                allocate_managed_output(ManagedFileOutputKind::StreamIngestRecord, Some("mp4"));
            let hls_output =
                allocate_managed_output(ManagedFileOutputKind::StreamIngestRecord, Some("hls"));
            let segment_template = hls_segment_template(hls_output.target.as_str());
            args.extend([
                "-map".to_string(),
                "0:v?".to_string(),
                "-map".to_string(),
                "0:a?".to_string(),
            ]);
            if let Some(filter) = audio_copy_decoration
                .and_then(|value| value.filter_for_output(mp4_output.format.as_str()))
            {
                append_audio_bitstream_filter_arg(&mut args, filter);
            }
            args.extend([
                "-f".to_string(),
                "mp4".to_string(),
                mp4_output.target.clone(),
                "-map".to_string(),
                "0:v?".to_string(),
                "-map".to_string(),
                "0:a?".to_string(),
                "-f".to_string(),
                "hls".to_string(),
                "-hls_time".to_string(),
                spec.record.segment_sec.unwrap_or(6).to_string(),
                "-hls_list_size".to_string(),
                "0".to_string(),
                "-hls_segment_filename".to_string(),
                segment_template,
                hls_output.target.clone(),
            ]);
            outputs.push(mp4_output.target.clone());
            outputs.push(hls_output.target.clone());
            mp4_output
        }
    };

    Ok(ProcessPlan {
        executable: settings.ffmpeg_bin.clone(),
        args,
        work_dir,
        output_target: primary_output.target.clone(),
        outputs,
        success_check: primary_output.success_check,
        startup_probe: None,
        recording: None,
        managed_file_output_kind: Some(ManagedFileOutputKind::StreamIngestRecord),
    })
}

fn build_multicast_bridge_plan(
    settings: &AgentSettings,
    request: &StartTaskRequest,
    spec: &TaskSpec,
) -> Result<ProcessPlan, ExecutorError> {
    let input_url = build_input_url(settings, &spec.input)?;
    let work_dir = attempt_work_dir(settings, request.task_id, request.attempt_no);
    let output = build_publish_output(settings, spec.task_type, &spec.publish)?;
    let startup_probe = None;
    let realtime = spec.input.source_mode == Some(SourceMode::Vod)
        && matches!(
            spec.publish.kind,
            Some(
                PublishTargetKind::UdpMpegtsMulticast
                    | PublishTargetKind::RtpMulticast
                    | PublishTargetKind::RtmpPush
            )
        );
    let mut args = ffmpeg_base_args(input_url.clone(), realtime);
    if spec.input.source_mode != Some(SourceMode::Vod) {
        insert_ffmpeg_input_args(
            &mut args,
            vec![
                "-use_wallclock_as_timestamps".to_string(),
                "1".to_string(),
                "-fflags".to_string(),
                "+genpts".to_string(),
            ],
        );
    }
    if should_stabilize_live_mpegts_multicast_bridge(spec, &output) {
        // ZLM-published live inputs can surface unset/non-monotonic DTS when copied
        // directly into MPEG-TS. Re-encode video to regenerate timestamps while
        // keeping audio copy so the bridge stays close to passthrough semantics.
        append_live_mpegts_multicast_bridge_args(&mut args, settings, spec, input_url.as_str());
    } else {
        let audio_copy_decoration = append_process_args(
            &mut args,
            settings,
            spec,
            "passthrough",
            input_url.as_str(),
            output.format.as_str(),
            VideoOutputPolicy::ForceH264,
            AudioOutputPolicy::Aac,
        )?;
        if let Some(filter) =
            audio_copy_decoration.and_then(|value| value.filter_for_output(output.format.as_str()))
        {
            append_audio_bitstream_filter_arg(&mut args, filter);
        }
    }
    args.extend([
        "-threads".to_string(),
        "0".to_string(),
        "-f".to_string(),
        output.format.clone(),
        output.target.clone(),
    ]);

    Ok(ProcessPlan {
        executable: settings.ffmpeg_bin.clone(),
        args,
        work_dir,
        output_target: output.target.clone(),
        outputs: vec![output.target],
        success_check: output.success_check,
        startup_probe,
        recording: None,
        managed_file_output_kind: Some(ManagedFileOutputKind::Bridge),
    })
}

fn should_stabilize_live_mpegts_multicast_bridge(spec: &TaskSpec, output: &PublishOutput) -> bool {
    spec.process.mode.as_deref().unwrap_or("passthrough") == "passthrough"
        && requires_live_mpegts_multicast_video_stabilization(spec, output.format.as_str())
}

fn requires_live_mpegts_multicast_video_stabilization(
    spec: &TaskSpec,
    output_format: &str,
) -> bool {
    output_format.eq_ignore_ascii_case("mpegts")
        && matches!(
            spec.input.kind,
            Some(
                InputKind::Rtsp
                    | InputKind::Rtmp
                    | InputKind::Hls
                    | InputKind::HttpFlv
                    | InputKind::HttpTs
            )
        )
        && matches!(
            spec.publish.kind,
            Some(PublishTargetKind::UdpMpegtsMulticast)
        )
}

fn append_live_mpegts_multicast_bridge_args(
    args: &mut Vec<String>,
    settings: &AgentSettings,
    spec: &TaskSpec,
    input_url: &str,
) {
    let selection = resolve_transcode_selection(
        settings,
        input_url,
        VideoOutputPolicy::ForceH264,
        AudioOutputPolicy::Copy,
    );
    let video_codec = selection.video_encoder;
    if !selection.input_args.is_empty() {
        insert_ffmpeg_input_args(args, selection.input_args);
    }

    args.extend([
        "-c:v".to_string(),
        video_codec.clone(),
        "-c:a".to_string(),
        selection.audio_encoder,
    ]);

    if let Some(bitrate) = spec.process.bitrate {
        args.extend(["-b:v".to_string(), format!("{bitrate}k")]);
    }
    if let Some(fps) = spec.process.fps {
        args.extend(["-r".to_string(), fps.to_string()]);
    }

    let gop = spec.process.gop.unwrap_or(24);
    args.extend([
        "-g".to_string(),
        gop.to_string(),
        "-sc_threshold".to_string(),
        "0".to_string(),
    ]);

    if video_codec == "libx264" {
        args.extend(["-preset".to_string(), "ultrafast".to_string()]);
    }
}

fn build_live_relay_plan(
    settings: &AgentSettings,
    request: &StartTaskRequest,
    spec: &TaskSpec,
) -> Result<LiveRelayPlan, ExecutorError> {
    let input_url = required_nonempty("input.url", spec.input.url.as_deref())?;
    let startup_probe = build_startup_probe(request.task_id, spec)?;
    let work_dir = attempt_work_dir(settings, request.task_id, request.attempt_no);
    let recording = build_live_relay_recording(spec, &work_dir)?;
    let command_line = format!(
        "zlm addStreamProxy --url {} --vhost {} --app {} --stream {}",
        input_url, startup_probe.vhost, startup_probe.app, startup_probe.stream
    );
    let mut outputs = vec![format!(
        "zlm://{}/{}/{}",
        startup_probe.vhost, startup_probe.app, startup_probe.stream
    )];
    if let Some(recording) = &recording {
        outputs.push(recording.root_path.clone());
    }

    Ok(LiveRelayPlan {
        work_dir,
        input_url,
        command_line,
        outputs,
        startup_probe,
        recording,
    })
}

fn build_rtp_receive_plan(
    settings: &AgentSettings,
    request: &StartTaskRequest,
    spec: &TaskSpec,
) -> Result<RtpReceivePlan, ExecutorError> {
    if spec.task_type != TaskType::StreamIngest || spec.input.kind != Some(InputKind::GbRtp) {
        return Err(ExecutorError::InvalidRequest(
            "stream_ingest rtp mode requires input.kind=gb_rtp".to_string(),
        ));
    }
    let requested_port = spec
        .input
        .port
        .ok_or_else(|| ExecutorError::InvalidRequest("input.port must be provided".to_string()))?;
    let tcp_mode = spec.input.tcp_mode.unwrap_or(0);
    if tcp_mode > 2 {
        return Err(ExecutorError::InvalidRequest(
            "input.tcp_mode must be one of 0 (udp), 1 (tcp_passive), 2 (tcp_active)".to_string(),
        ));
    }
    let reuse_port = spec.input.reuse;
    let ssrc = spec.input.ssrc;

    let stream_id = build_rtp_stream_id(request.task_id, request.attempt_no);
    let work_dir = attempt_work_dir(settings, request.task_id, request.attempt_no);
    let mut command_line = format!(
        "zlm openRtpServer --port {} --tcp_mode {} --stream_id {}",
        requested_port, tcp_mode, stream_id
    );
    if let Some(reuse_port) = reuse_port {
        command_line.push_str(&format!(
            " --re_use_port {}",
            if reuse_port { 1 } else { 0 }
        ));
    }
    if let Some(ssrc) = ssrc {
        command_line.push_str(&format!(" --ssrc {ssrc}"));
    }
    Ok(RtpReceivePlan {
        work_dir,
        stream_id: stream_id.clone(),
        requested_port,
        tcp_mode,
        reuse_port,
        ssrc,
        command_line,
        outputs: vec![format!("rtp_receive://{stream_id}")],
    })
}

fn prepare_work_dir(work_dir: &Path) -> Result<(), ExecutorError> {
    fs::create_dir_all(work_dir).map_err(|error| {
        ExecutorError::ProcessSpawn(format!(
            "failed to prepare work dir {}: {error}",
            work_dir.display()
        ))
    })
}

fn prepare_plan_paths(plan: &ProcessPlan) -> Result<(), ExecutorError> {
    prepare_work_dir(&plan.work_dir)?;

    if let SuccessCheck::FileExists(path) = &plan.success_check {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(|error| {
                ExecutorError::ProcessSpawn(format!(
                    "failed to prepare output dir {}: {error}",
                    parent.display()
                ))
            })?;
        }
    }

    Ok(())
}

fn insert_ffmpeg_input_args(args: &mut Vec<String>, extra_args: Vec<String>) {
    let input_index = args
        .iter()
        .position(|arg| arg == "-i")
        .expect("ffmpeg args should always include an input marker");
    args.splice(input_index..input_index, extra_args);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VideoOutputPolicy {
    KeepSourceFamily,
    ForceH264,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioOutputPolicy {
    Copy,
    Aac,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioBitstreamFilter {
    AacAdtsToAsc,
}

impl AudioBitstreamFilter {
    fn as_ffmpeg_name(self) -> &'static str {
        match self {
            Self::AacAdtsToAsc => "aac_adtstoasc",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioCopyDecoration {
    NeedsAdtsToAscForFlvAndMp4,
}

impl AudioCopyDecoration {
    fn filter_for_output(self, output_format: &str) -> Option<AudioBitstreamFilter> {
        match output_format.trim().to_ascii_lowercase().as_str() {
            "flv" | "mp4" | "mov" => Some(AudioBitstreamFilter::AacAdtsToAsc),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VideoCodecFamily {
    H264,
    Hevc,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputSourceFamily {
    MpegTs,
    Hls,
    Mp4Mov,
    Matroska,
    RtspRtmp,
    Unknown,
}

#[derive(Debug, Clone)]
struct TranscodeSelection {
    input_args: Vec<String>,
    video_encoder: String,
    audio_encoder: String,
    audio_copy_decoration: Option<AudioCopyDecoration>,
}

fn build_input_url(settings: &AgentSettings, input: &InputSpec) -> Result<String, ExecutorError> {
    match input.kind {
        Some(InputKind::File) => {
            let raw_value = input.url.as_deref().ok_or_else(|| {
                ExecutorError::InvalidRequest("input.url must be provided".to_string())
            })?;
            let normalized = normalize_relative_file_input_path(raw_value)
                .map_err(|message| ExecutorError::InvalidRequest(format!("input.url {message}")))?;
            Ok(PathBuf::from(&settings.work_root)
                .join(normalized)
                .to_string_lossy()
                .to_string())
        }
        Some(
            InputKind::Rtsp
            | InputKind::Rtmp
            | InputKind::Hls
            | InputKind::Ftp
            | InputKind::HttpMp4
            | InputKind::HttpFlv
            | InputKind::HttpTs,
        ) => input
            .url
            .clone()
            .ok_or_else(|| ExecutorError::InvalidRequest("input.url must be provided".to_string())),
        Some(InputKind::UdpMpegtsMulticast | InputKind::RtpMulticast) => build_multicast_url(
            input.kind.expect("kind checked"),
            input.group.as_deref(),
            input.port,
            resolve_interface_binding_ip(
                input.interface_name.as_deref(),
                input.interface_ip.as_deref(),
                Some(settings.multicast_interface_name.as_str()),
                Some(settings.multicast_interface_ip.as_str()),
                "input",
                true,
            )?
            .as_deref(),
            input.ttl,
            input.reuse,
            input.pkt_size,
            input.dscp,
            input.buffer_size,
            input.fifo_size,
            true,
            "input",
        ),
        Some(InputKind::GbRtp) | None => Err(ExecutorError::InvalidRequest(
            "managed executor requires a supported input kind".to_string(),
        )),
    }
}

fn should_loop_file_to_live_input(spec: &TaskSpec) -> bool {
    spec.task_type == TaskType::StreamIngest
        && spec.input.loop_enabled.unwrap_or(false)
        && spec.input.source_mode == Some(SourceMode::Vod)
        && matches!(
            spec.input.kind,
            Some(InputKind::File | InputKind::HttpMp4 | InputKind::Hls | InputKind::HttpTs)
        )
}

fn ffmpeg_base_args_without_maps(input_url: String, realtime: bool) -> Vec<String> {
    let mut args = vec![
        "-hide_banner".to_string(),
        "-nostdin".to_string(),
        "-y".to_string(),
        "-loglevel".to_string(),
        "info".to_string(),
        "-stats_period".to_string(),
        "1".to_string(),
        "-progress".to_string(),
        "pipe:1".to_string(),
    ];
    if realtime {
        args.push("-re".to_string());
    }
    args.extend(["-i".to_string(), input_url]);
    args
}

fn ffmpeg_base_args(input_url: String, realtime: bool) -> Vec<String> {
    let mut args = ffmpeg_base_args_without_maps(input_url, realtime);
    args.extend([
        "-map".to_string(),
        "0:v?".to_string(),
        "-map".to_string(),
        "0:a?".to_string(),
    ]);
    args
}

fn append_default_output_maps(args: &mut Vec<String>) {
    args.extend([
        "-map".to_string(),
        "0:v?".to_string(),
        "-map".to_string(),
        "0:a?".to_string(),
    ]);
}

fn append_audio_bitstream_filter_arg(args: &mut Vec<String>, filter: AudioBitstreamFilter) {
    args.extend(["-bsf:a".to_string(), filter.as_ffmpeg_name().to_string()]);
}

fn tee_slave_target(
    output_format: &str,
    target: &str,
    onfail_ignore: bool,
    audio_copy_decoration: Option<AudioCopyDecoration>,
) -> String {
    let mut options = vec![format!("f={output_format}")];
    if onfail_ignore {
        options.push("onfail=ignore".to_string());
    }
    if let Some(filter) =
        audio_copy_decoration.and_then(|value| value.filter_for_output(output_format))
    {
        options.push(format!("bsfs/a={}", filter.as_ffmpeg_name()));
    }

    format!("[{}]{}", options.join(":"), escape_tee_target(target))
}

fn normalized_process_mode<'a>(spec: &'a TaskSpec, default_mode: &'a str) -> &'a str {
    match spec.process.mode.as_deref().unwrap_or(default_mode) {
        "transcode" => "force_transcode",
        value => value,
    }
}

fn append_process_args(
    args: &mut Vec<String>,
    settings: &AgentSettings,
    spec: &TaskSpec,
    default_mode: &str,
    input_url: &str,
    output_format: &str,
    video_policy: VideoOutputPolicy,
    audio_policy: AudioOutputPolicy,
) -> Result<Option<AudioCopyDecoration>, ExecutorError> {
    let mode = normalized_process_mode(spec, default_mode);
    match mode {
        "passthrough" => {
            let audio_copy_decoration =
                resolve_passthrough_audio_copy_decoration(settings, spec, input_url, output_format);
            args.extend([
                "-c:v".to_string(),
                "copy".to_string(),
                "-c:a".to_string(),
                "copy".to_string(),
            ]);
            Ok(audio_copy_decoration)
        }
        "copy_or_transcode" | "force_transcode" => {
            let selection = resolve_process_selection(
                settings,
                spec,
                mode,
                input_url,
                output_format,
                video_policy,
                audio_policy,
            );
            if !selection.input_args.is_empty() {
                insert_ffmpeg_input_args(args, selection.input_args);
            }
            args.extend([
                "-c:v".to_string(),
                selection.video_encoder,
                "-c:a".to_string(),
                selection.audio_encoder,
            ]);
            if let Some(bitrate) = spec.process.bitrate {
                args.extend(["-b:v".to_string(), format!("{bitrate}k")]);
            }
            if let Some(fps) = spec.process.fps {
                args.extend(["-r".to_string(), fps.to_string()]);
            }
            if let Some(gop) = spec.process.gop {
                args.extend(["-g".to_string(), gop.to_string()]);
            }
            Ok(selection.audio_copy_decoration)
        }
        other => Err(ExecutorError::InvalidRequest(format!(
            "unsupported process.mode: {other}"
        ))),
    }
}

fn resolve_process_selection(
    settings: &AgentSettings,
    spec: &TaskSpec,
    mode: &str,
    input_url: &str,
    output_format: &str,
    video_policy: VideoOutputPolicy,
    audio_policy: AudioOutputPolicy,
) -> TranscodeSelection {
    if mode == "force_transcode" {
        return resolve_transcode_selection(settings, input_url, video_policy, audio_policy);
    }

    let profile = probe_input_media_profile(settings, spec, input_url);
    let video_copy = should_copy_video_stream(spec, output_format, &profile, video_policy);
    let audio_copy = resolve_audio_copy_selection(spec, output_format, &profile, audio_policy);
    if video_copy && audio_copy.copy {
        return TranscodeSelection {
            input_args: Vec::new(),
            video_encoder: "copy".to_string(),
            audio_encoder: "copy".to_string(),
            audio_copy_decoration: audio_copy.decoration,
        };
    }

    let transcode = resolve_transcode_selection_for_input_family(
        settings,
        profile.video_family,
        video_policy,
        audio_policy,
    );

    TranscodeSelection {
        input_args: if video_copy {
            Vec::new()
        } else {
            transcode.input_args
        },
        video_encoder: if video_copy {
            "copy".to_string()
        } else {
            transcode.video_encoder
        },
        audio_encoder: if audio_copy.copy {
            "copy".to_string()
        } else {
            transcode.audio_encoder
        },
        audio_copy_decoration: if audio_copy.copy {
            audio_copy.decoration
        } else {
            None
        },
    }
}

fn should_copy_video_stream(
    spec: &TaskSpec,
    output_format: &str,
    profile: &InputMediaProfile,
    video_policy: VideoOutputPolicy,
) -> bool {
    if !profile.has_video {
        return true;
    }
    if process_requires_video_transcode(spec)
        || requires_live_mpegts_multicast_video_stabilization(spec, output_format)
    {
        return false;
    }

    let required_family = output_video_family(profile.video_family, video_policy);
    profile.video_family == required_family
        && format_supports_video_family_copy(output_format, profile.video_family)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AudioCopySelection {
    copy: bool,
    decoration: Option<AudioCopyDecoration>,
}

fn resolve_audio_copy_selection(
    spec: &TaskSpec,
    output_format: &str,
    profile: &InputMediaProfile,
    audio_policy: AudioOutputPolicy,
) -> AudioCopySelection {
    if !profile.has_audio {
        return AudioCopySelection {
            copy: true,
            decoration: None,
        };
    }
    if process_requires_audio_transcode(spec) {
        return AudioCopySelection {
            copy: false,
            decoration: None,
        };
    }

    match audio_policy {
        AudioOutputPolicy::Copy => AudioCopySelection {
            copy: format_supports_audio_codec_copy(
                output_format,
                profile.audio_codec_name.as_deref(),
            ),
            decoration: None,
        },
        AudioOutputPolicy::Aac => {
            let copy = profile.audio_codec_name.as_deref() == Some("aac")
                && format_supports_audio_codec_copy(output_format, Some("aac"));
            AudioCopySelection {
                copy,
                decoration: if copy {
                    resolve_audio_copy_decoration(
                        profile.source_family,
                        profile.audio_codec_name.as_deref(),
                    )
                } else {
                    None
                },
            }
        }
    }
}

fn resolve_passthrough_audio_copy_decoration(
    settings: &AgentSettings,
    spec: &TaskSpec,
    input_url: &str,
    output_format: &str,
) -> Option<AudioCopyDecoration> {
    let profile = probe_input_media_profile(settings, spec, input_url);
    if !profile.has_audio
        || !format_supports_audio_codec_copy(output_format, profile.audio_codec_name.as_deref())
    {
        return None;
    }

    resolve_audio_copy_decoration(profile.source_family, profile.audio_codec_name.as_deref())
}

fn resolve_audio_copy_decoration(
    source_family: InputSourceFamily,
    codec_name: Option<&str>,
) -> Option<AudioCopyDecoration> {
    (matches!(
        source_family,
        InputSourceFamily::MpegTs | InputSourceFamily::Hls
    ) && codec_name == Some("aac"))
    .then_some(AudioCopyDecoration::NeedsAdtsToAscForFlvAndMp4)
}

fn process_requires_video_transcode(spec: &TaskSpec) -> bool {
    spec.process.bitrate.is_some()
        || spec.process.fps.is_some()
        || spec.process.gop.is_some()
        || spec
            .process
            .video_codec
            .as_deref()
            .map(str::trim)
            .is_some_and(|value| !value.is_empty())
        || spec
            .process
            .profile
            .as_deref()
            .map(str::trim)
            .is_some_and(|value| !value.is_empty())
        || spec
            .process
            .preset
            .as_deref()
            .map(str::trim)
            .is_some_and(|value| !value.is_empty())
}

fn process_requires_audio_transcode(spec: &TaskSpec) -> bool {
    spec.process
        .audio_codec
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
}

fn format_supports_video_family_copy(output_format: &str, video_family: VideoCodecFamily) -> bool {
    match output_format.trim().to_ascii_lowercase().as_str() {
        "flv" => matches!(video_family, VideoCodecFamily::H264),
        "mp4" | "mov" | "matroska" | "mkv" | "mpegts" | "rtp_mpegts" | "hls" => {
            matches!(
                video_family,
                VideoCodecFamily::H264 | VideoCodecFamily::Hevc
            )
        }
        _ => false,
    }
}

fn format_supports_audio_codec_copy(output_format: &str, codec_name: Option<&str>) -> bool {
    let Some(codec_name) = codec_name.map(str::trim).map(str::to_ascii_lowercase) else {
        return false;
    };

    match output_format.trim().to_ascii_lowercase().as_str() {
        "flv" | "mp4" | "mov" | "matroska" | "mkv" | "mpegts" | "rtp_mpegts" | "hls" => {
            codec_name == "aac"
        }
        _ => false,
    }
}

fn resolve_transcode_selection(
    settings: &AgentSettings,
    input_url: &str,
    video_policy: VideoOutputPolicy,
    audio_policy: AudioOutputPolicy,
) -> TranscodeSelection {
    let (input_family, _) = resolve_video_families(settings, input_url, video_policy);
    resolve_transcode_selection_for_input_family(settings, input_family, video_policy, audio_policy)
}

fn resolve_transcode_selection_for_input_family(
    settings: &AgentSettings,
    input_family: VideoCodecFamily,
    video_policy: VideoOutputPolicy,
    audio_policy: AudioOutputPolicy,
) -> TranscodeSelection {
    let output_family = output_video_family(input_family, video_policy);
    let use_gpu = gpu_acceleration_enabled(settings)
        && !probe_gpu_devices(settings).is_empty()
        && ffmpeg_supports_hwaccel(&settings.ffmpeg_bin, "cuda")
        && matches!(
            output_family,
            VideoCodecFamily::H264 | VideoCodecFamily::Hevc
        );

    let mut input_args = Vec::new();
    let video_encoder = if use_gpu {
        match output_family {
            VideoCodecFamily::Hevc
                if ffmpeg_supports_encoder(&settings.ffmpeg_bin, "hevc_nvenc") =>
            {
                maybe_add_cuda_decoder(&mut input_args, settings, input_family);
                "hevc_nvenc".to_string()
            }
            _ if ffmpeg_supports_encoder(&settings.ffmpeg_bin, "h264_nvenc") => {
                maybe_add_cuda_decoder(&mut input_args, settings, input_family);
                "h264_nvenc".to_string()
            }
            VideoCodecFamily::Hevc => "libx265".to_string(),
            _ => "libx264".to_string(),
        }
    } else {
        match output_family {
            VideoCodecFamily::Hevc => "libx265".to_string(),
            _ => "libx264".to_string(),
        }
    };

    let audio_encoder = match audio_policy {
        AudioOutputPolicy::Copy => "copy".to_string(),
        AudioOutputPolicy::Aac => "aac".to_string(),
    };

    TranscodeSelection {
        input_args,
        video_encoder,
        audio_encoder,
        audio_copy_decoration: None,
    }
}

fn output_video_family(
    input_family: VideoCodecFamily,
    video_policy: VideoOutputPolicy,
) -> VideoCodecFamily {
    match video_policy {
        VideoOutputPolicy::KeepSourceFamily => match input_family {
            VideoCodecFamily::Hevc => VideoCodecFamily::Hevc,
            _ => VideoCodecFamily::H264,
        },
        VideoOutputPolicy::ForceH264 => VideoCodecFamily::H264,
    }
}

fn resolve_video_families(
    settings: &AgentSettings,
    input_url: &str,
    video_policy: VideoOutputPolicy,
) -> (VideoCodecFamily, VideoCodecFamily) {
    let input_family = probe_primary_video_codec_family(settings, input_url);
    let output_family = output_video_family(input_family, video_policy);
    (input_family, output_family)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InputMediaProfile {
    has_video: bool,
    video_family: VideoCodecFamily,
    has_audio: bool,
    audio_codec_name: Option<String>,
    source_family: InputSourceFamily,
}

impl Default for InputMediaProfile {
    fn default() -> Self {
        Self {
            has_video: false,
            video_family: VideoCodecFamily::Unknown,
            has_audio: false,
            audio_codec_name: None,
            source_family: InputSourceFamily::Unknown,
        }
    }
}

#[derive(Debug, Deserialize)]
struct FfprobeMediaResponse {
    #[serde(default)]
    streams: Vec<FfprobeStream>,
    format: Option<FfprobeFormat>,
}

#[derive(Debug, Deserialize)]
struct FfprobeFormat {
    format_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FfprobeStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
}

fn probe_input_media_profile(
    settings: &AgentSettings,
    spec: &TaskSpec,
    input_url: &str,
) -> InputMediaProfile {
    let default_profile = InputMediaProfile {
        source_family: infer_input_source_family(spec, input_url, None),
        ..InputMediaProfile::default()
    };
    let output = std::process::Command::new(&settings.ffprobe_bin)
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type,codec_name:format=format_name",
            "-of",
            "json",
            input_url,
        ])
        .output();

    let Ok(output) = output else {
        return default_profile;
    };
    if !output.status.success() {
        return default_profile;
    }

    let Ok(parsed) = serde_json::from_slice::<FfprobeMediaResponse>(&output.stdout) else {
        return default_profile;
    };

    let mut profile = InputMediaProfile {
        source_family: infer_input_source_family(
            spec,
            input_url,
            parsed
                .format
                .as_ref()
                .and_then(|format| format.format_name.as_deref()),
        ),
        ..InputMediaProfile::default()
    };
    for stream in parsed.streams {
        match stream.codec_type.as_deref() {
            Some("video") if !profile.has_video => {
                profile.has_video = true;
                profile.video_family = match stream
                    .codec_name
                    .as_deref()
                    .map(str::trim)
                    .map(str::to_ascii_lowercase)
                    .as_deref()
                {
                    Some("h264") => VideoCodecFamily::H264,
                    Some("hevc") | Some("h265") => VideoCodecFamily::Hevc,
                    _ => VideoCodecFamily::Unknown,
                };
            }
            Some("audio") if !profile.has_audio => {
                profile.has_audio = true;
                profile.audio_codec_name = stream
                    .codec_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_ascii_lowercase);
            }
            _ => {}
        }
    }

    profile
}

fn infer_input_source_family(
    spec: &TaskSpec,
    input_url: &str,
    probed_format_name: Option<&str>,
) -> InputSourceFamily {
    match spec.input.kind {
        Some(InputKind::Hls) => InputSourceFamily::Hls,
        Some(InputKind::HttpTs | InputKind::UdpMpegtsMulticast) => InputSourceFamily::MpegTs,
        Some(InputKind::HttpMp4) => InputSourceFamily::Mp4Mov,
        Some(InputKind::Rtsp | InputKind::Rtmp | InputKind::HttpFlv) => InputSourceFamily::RtspRtmp,
        _ => classify_input_source_family_from_format_name(probed_format_name)
            .unwrap_or_else(|| classify_input_source_family_from_path(input_url)),
    }
}

fn classify_input_source_family_from_format_name(
    format_name: Option<&str>,
) -> Option<InputSourceFamily> {
    let names = format_name?
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty());

    for name in names {
        match name.to_ascii_lowercase().as_str() {
            "mpegts" => return Some(InputSourceFamily::MpegTs),
            "hls" | "applehttp" => return Some(InputSourceFamily::Hls),
            "mov" | "mp4" | "m4a" | "3gp" | "3g2" | "mj2" => {
                return Some(InputSourceFamily::Mp4Mov);
            }
            "matroska" | "webm" => return Some(InputSourceFamily::Matroska),
            "rtsp" | "rtmp" | "flv" | "live_flv" => return Some(InputSourceFamily::RtspRtmp),
            _ => {}
        }
    }

    None
}

fn classify_input_source_family_from_path(input_url: &str) -> InputSourceFamily {
    let extension = Path::new(input_url)
        .extension()
        .and_then(|value| value.to_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase);

    match extension.as_deref() {
        Some("ts" | "m2ts" | "mts") => InputSourceFamily::MpegTs,
        Some("m3u8") => InputSourceFamily::Hls,
        Some("mp4" | "mov" | "m4v" | "m4a" | "3gp" | "3g2") => InputSourceFamily::Mp4Mov,
        Some("mkv" | "webm") => InputSourceFamily::Matroska,
        _ => InputSourceFamily::Unknown,
    }
}

fn maybe_add_cuda_decoder(
    input_args: &mut Vec<String>,
    settings: &AgentSettings,
    input_family: VideoCodecFamily,
) {
    let decoder = match input_family {
        VideoCodecFamily::H264 if ffmpeg_supports_decoder(&settings.ffmpeg_bin, "h264_cuvid") => {
            Some("h264_cuvid")
        }
        VideoCodecFamily::Hevc if ffmpeg_supports_decoder(&settings.ffmpeg_bin, "hevc_cuvid") => {
            Some("hevc_cuvid")
        }
        _ => None,
    };

    if let Some(decoder) = decoder {
        input_args.extend([
            "-hwaccel".to_string(),
            "cuda".to_string(),
            "-hwaccel_output_format".to_string(),
            "cuda".to_string(),
            "-c:v".to_string(),
            decoder.to_string(),
        ]);
    }
}

fn probe_primary_video_codec_family(settings: &AgentSettings, input_url: &str) -> VideoCodecFamily {
    let output = std::process::Command::new(&settings.ffprobe_bin)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
            input_url,
        ])
        .output();

    let Ok(output) = output else {
        return VideoCodecFamily::Unknown;
    };
    if !output.status.success() {
        return VideoCodecFamily::Unknown;
    }

    match String::from_utf8_lossy(&output.stdout).trim() {
        "h264" => VideoCodecFamily::H264,
        "hevc" | "h265" => VideoCodecFamily::Hevc,
        _ => VideoCodecFamily::Unknown,
    }
}

#[derive(Debug, Clone)]
struct PublishOutput {
    target: String,
    format: String,
    success_check: SuccessCheck,
}

fn build_internal_stream_output(settings: &AgentSettings, probe: &StartupProbe) -> PublishOutput {
    PublishOutput {
        success_check: SuccessCheck::ProcessExit,
        target: build_internal_stream_target(settings, probe),
        // Internal ingest always pushes into ZLM via RTMP, regardless of which
        // playback protocols are later exposed for the stream.
        format: "flv".to_string(),
    }
}

fn build_internal_stream_target(settings: &AgentSettings, probe: &StartupProbe) -> String {
    let host = Url::parse(&settings.zlm_api_base)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_else(|| "127.0.0.1".to_string());
    format!("rtmp://{host}/{}/{}", probe.app, probe.stream)
}

fn build_publish_output(
    settings: &AgentSettings,
    task_type: TaskType,
    publish: &PublishSpec,
) -> Result<PublishOutput, ExecutorError> {
    match publish.kind {
        Some(PublishTargetKind::File) => managed_file_output_kind_for_task(task_type)
            .ok_or_else(|| {
                ExecutorError::InvalidRequest(
                    "publish.kind=file is only supported for managed file output tasks".to_string(),
                )
            })
            .and_then(|kind| allocate_managed_file_output(kind, publish)),
        Some(PublishTargetKind::UdpMpegtsMulticast | PublishTargetKind::RtpMulticast) => {
            let target = build_multicast_url(
                match publish.kind.expect("kind checked") {
                    PublishTargetKind::UdpMpegtsMulticast => InputKind::UdpMpegtsMulticast,
                    PublishTargetKind::RtpMulticast => InputKind::RtpMulticast,
                    _ => unreachable!(),
                },
                publish.group.as_deref(),
                publish.port,
                resolve_interface_binding_ip(
                    publish.interface_name.as_deref(),
                    publish.interface_ip.as_deref(),
                    Some(settings.multicast_interface_name.as_str()),
                    Some(settings.multicast_interface_ip.as_str()),
                    "publish",
                    false,
                )?
                .as_deref(),
                publish.ttl,
                publish.reuse,
                publish.pkt_size,
                publish.dscp,
                publish.buffer_size,
                publish.fifo_size,
                false,
                "publish",
            )?;
            let format = publish
                .format
                .clone()
                .unwrap_or_else(|| match publish.kind {
                    Some(PublishTargetKind::RtpMulticast) => "rtp_mpegts".to_string(),
                    _ => "mpegts".to_string(),
                });
            Ok(PublishOutput {
                success_check: SuccessCheck::ProcessExit,
                target,
                format,
            })
        }
        Some(PublishTargetKind::RtmpPush) => Ok(PublishOutput {
            success_check: SuccessCheck::ProcessExit,
            target: required_nonempty("publish.url", publish.url.as_deref())?,
            format: "flv".to_string(),
        }),
        None => Err(ExecutorError::InvalidRequest(
            "publish.kind must be provided".to_string(),
        )),
    }
}

fn resolve_interface_binding_ip(
    explicit_name: Option<&str>,
    explicit_ip: Option<&str>,
    default_name: Option<&str>,
    default_ip: Option<&str>,
    field_prefix: &str,
    required: bool,
) -> Result<Option<String>, ExecutorError> {
    if let Some(ip) = nonempty(explicit_ip) {
        return Ok(Some(ip.to_string()));
    }
    if let Some(name) = nonempty(explicit_name) {
        let ip = resolve_interface_name_to_ipv4(name).ok_or_else(|| {
            ExecutorError::InvalidRequest(format!(
                "{field_prefix}.interface_name refers to an unknown interface or one without IPv4: {name}"
            ))
        })?;
        return Ok(Some(ip));
    }
    if let Some(name) = nonempty(default_name) {
        if let Some(ip) = resolve_interface_name_to_ipv4(name) {
            return Ok(Some(ip));
        }
        if let Some(ip) = nonempty(default_ip) {
            return Ok(Some(ip.to_string()));
        }
        return Err(ExecutorError::InvalidRequest(format!(
            "configured default multicast interface has no IPv4 address: {name}"
        )));
    }
    if let Some(ip) = nonempty(default_ip) {
        return Ok(Some(ip.to_string()));
    }
    if required {
        return Err(ExecutorError::InvalidRequest(format!(
            "{field_prefix}.interface_name or a configured default multicast interface must be provided"
        )));
    }
    Ok(None)
}

fn resolve_interface_name_to_ipv4(name: &str) -> Option<String> {
    let target = name.trim();
    if target.is_empty() {
        return None;
    }

    unsafe {
        let mut addrs: *mut libc::ifaddrs = ptr::null_mut();
        if libc::getifaddrs(&mut addrs) != 0 || addrs.is_null() {
            return None;
        }

        let mut current = addrs;
        let mut resolved = None;
        while !current.is_null() {
            let ifa = &*current;
            if !ifa.ifa_name.is_null()
                && !ifa.ifa_addr.is_null()
                && (*ifa.ifa_addr).sa_family as i32 == libc::AF_INET
            {
                let if_name = CStr::from_ptr(ifa.ifa_name).to_string_lossy();
                if if_name == target {
                    let addr = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    let ip = Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr));
                    resolved = Some(ip.to_string());
                    break;
                }
            }
            current = ifa.ifa_next;
        }
        libc::freeifaddrs(addrs);
        resolved
    }
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn build_live_relay_api_params(
    settings: &AgentSettings,
    spec: &TaskSpec,
    startup_probe: &StartupProbe,
    input_url: &str,
) -> Vec<(String, String)> {
    let mut params = vec![
        ("vhost".to_string(), startup_probe.vhost.clone()),
        ("app".to_string(), startup_probe.app.clone()),
        ("stream".to_string(), startup_probe.stream.clone()),
        ("url".to_string(), input_url.to_string()),
        ("retry_count".to_string(), "-1".to_string()),
        (
            "timeout_sec".to_string(),
            input_timeout_seconds(spec.input.probe_timeout_ms).to_string(),
        ),
        ("enable_audio".to_string(), "1".to_string()),
        ("add_mute_audio".to_string(), "1".to_string()),
        ("modify_stamp".to_string(), "2".to_string()),
        (
            "enable_rtsp".to_string(),
            bool_as_flag(spec.expose.enable_rtsp.unwrap_or(true)),
        ),
        (
            "enable_rtmp".to_string(),
            bool_as_flag(spec.expose.enable_rtmp.unwrap_or(true)),
        ),
        (
            "enable_hls".to_string(),
            bool_as_flag(spec.expose.enable_hls.unwrap_or(false) || spec.record.wants_hls()),
        ),
        (
            "enable_ts".to_string(),
            bool_as_flag(spec.expose.enable_http_ts.unwrap_or(true)),
        ),
        (
            "enable_fmp4".to_string(),
            bool_as_flag(spec.expose.enable_http_fmp4.unwrap_or(true)),
        ),
        (
            "enable_mp4".to_string(),
            bool_as_flag(spec.record.wants_mp4()),
        ),
        (
            "auto_close".to_string(),
            bool_as_flag(live_relay_auto_close_enabled(settings, spec)),
        ),
    ];

    if matches!(spec.input.kind, Some(InputKind::Rtsp)) {
        params.push(("rtp_type".to_string(), "0".to_string()));
    }

    params
}

fn build_open_rtp_server_params(plan: &RtpReceivePlan) -> Vec<(String, String)> {
    let mut params = vec![
        ("port".to_string(), plan.requested_port.to_string()),
        ("tcp_mode".to_string(), plan.tcp_mode.to_string()),
        ("stream_id".to_string(), plan.stream_id.clone()),
    ];
    if let Some(reuse_port) = plan.reuse_port {
        params.push((
            "re_use_port".to_string(),
            if reuse_port { "1" } else { "0" }.to_string(),
        ));
    }
    if let Some(ssrc) = plan.ssrc {
        params.push(("ssrc".to_string(), ssrc.to_string()));
    }
    params
}

fn build_live_relay_recording(
    spec: &TaskSpec,
    _work_dir: &Path,
) -> Result<Option<LiveRelayRecording>, ExecutorError> {
    if !spec.record.enabled.unwrap_or(false) {
        return Ok(None);
    }

    let formats = match spec
        .record
        .format
        .unwrap_or(media_domain::RecordFormat::Mp4)
    {
        media_domain::RecordFormat::Mp4 => vec![ZlmRecordKind::Mp4],
        media_domain::RecordFormat::Hls => vec![ZlmRecordKind::Hls],
        media_domain::RecordFormat::Both => vec![ZlmRecordKind::Mp4, ZlmRecordKind::Hls],
    };
    let root_path = ZLM_RECORD_HTTP_ROOT.to_string();

    Ok(Some(LiveRelayRecording {
        formats,
        root_path,
        duration_sec: spec.record.duration_sec,
        segment_sec: spec.record.segment_sec,
        as_player: spec.record.as_player.unwrap_or(false),
        recording_started_at: None,
        auto_stop_requested: false,
        completion_reason: None,
        started: false,
        failed: false,
    }))
}

fn build_startup_probe(task_id: Uuid, spec: &TaskSpec) -> Result<StartupProbe, ExecutorError> {
    let app = spec
        .stream
        .app
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("live")
        .to_string();
    let stream = spec
        .stream
        .name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| task_id.to_string());
    Ok(StartupProbe {
        schema: Some(preferred_publish_schema(&spec.expose)),
        vhost: spec
            .stream
            .vhost
            .clone()
            .unwrap_or_else(|| ZLM_RUNTIME_VHOST.to_string()),
        app,
        stream,
    })
}

fn preferred_publish_schema(expose: &ExposeSpec) -> String {
    if expose.enable_rtmp.unwrap_or(true) {
        "rtmp".to_string()
    } else if expose.enable_rtsp.unwrap_or(true) {
        "rtsp".to_string()
    } else if expose.enable_http_ts.unwrap_or(true) {
        "ts".to_string()
    } else if expose.enable_http_fmp4.unwrap_or(true) {
        "fmp4".to_string()
    } else if expose.enable_hls.unwrap_or(false) {
        "hls".to_string()
    } else {
        "rtmp".to_string()
    }
}

fn bool_as_flag(value: bool) -> String {
    if value { "1" } else { "0" }.to_string()
}

fn input_timeout_seconds(timeout_ms: Option<u64>) -> u64 {
    timeout_ms
        .map(|value| value / 1000)
        .filter(|value| *value > 0)
        .unwrap_or(15)
}

#[allow(clippy::too_many_arguments)]
fn build_multicast_url(
    kind: InputKind,
    group: Option<&str>,
    port: Option<u16>,
    interface_ip: Option<&str>,
    ttl: Option<u8>,
    reuse: Option<bool>,
    pkt_size: Option<u16>,
    dscp: Option<u8>,
    buffer_size: Option<u32>,
    fifo_size: Option<u32>,
    require_interface_ip: bool,
    field_prefix: &str,
) -> Result<String, ExecutorError> {
    let group = required_nonempty(&format!("{field_prefix}.group"), group)?;
    let port = port.ok_or_else(|| {
        ExecutorError::InvalidRequest(format!("{field_prefix}.port must be provided"))
    })?;
    let scheme = match kind {
        InputKind::UdpMpegtsMulticast => "udp",
        InputKind::RtpMulticast => "rtp",
        _ => {
            return Err(ExecutorError::InvalidRequest(format!(
                "{field_prefix}.kind must be a multicast kind"
            )));
        }
    };

    let mut query = Vec::new();
    match interface_ip
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(interface_ip) => query.push(format!("localaddr={interface_ip}")),
        None if require_interface_ip => {
            return Err(ExecutorError::InvalidRequest(format!(
                "{field_prefix}.interface_ip must be provided"
            )));
        }
        None => {}
    }
    if let Some(reuse) = reuse {
        query.push(format!("reuse={}", if reuse { 1 } else { 0 }));
    }
    if let Some(ttl) = ttl {
        query.push(format!("ttl={ttl}"));
    }
    if let Some(pkt_size) = pkt_size {
        query.push(format!("pkt_size={pkt_size}"));
    }
    if let Some(dscp) = dscp {
        query.push(format!("dscp={dscp}"));
    }
    if let Some(buffer_size) = buffer_size {
        query.push(format!("buffer_size={buffer_size}"));
    }
    if let Some(fifo_size) = fifo_size {
        query.push(format!("fifo_size={fifo_size}"));
    }

    if query.is_empty() {
        Ok(format!("{scheme}://{group}:{port}"))
    } else {
        Ok(format!("{scheme}://{group}:{port}?{}", query.join("&")))
    }
}

fn required_nonempty(field: &str, value: Option<&str>) -> Result<String, ExecutorError> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| ExecutorError::InvalidRequest(format!("{field} must be provided")))
}

fn attempt_work_dir(settings: &AgentSettings, task_id: Uuid, attempt_no: i32) -> PathBuf {
    PathBuf::from(&settings.work_root)
        .join(task_id.to_string())
        .join(format!("attempt-{attempt_no}"))
}

fn build_rtp_stream_id(task_id: Uuid, attempt_no: i32) -> String {
    format!("{task_id}-{attempt_no}")
}

fn escape_tee_target(value: &str) -> String {
    value.replace('\\', "\\\\").replace('|', "\\|")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StreamBinding {
    schema: Option<String>,
    vhost: String,
    app: String,
    stream: String,
}

fn requires_stream_online(handle: &RuntimeHandle) -> bool {
    handle
        .metadata
        .get("startup_probe")
        .map(|value| !value.is_null())
        .unwrap_or(false)
}

fn stream_online(handle: &RuntimeHandle) -> bool {
    handle
        .metadata
        .get("stream_online")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn completion_reason_from_handle(handle: &RuntimeHandle) -> Option<String> {
    handle
        .metadata
        .get("completion_reason")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            live_relay_recording_from_handle(handle)
                .and_then(|recording| recording.completion_reason)
        })
}

fn fatal_recording_error_from_handle(handle: &RuntimeHandle) -> Option<String> {
    handle
        .metadata
        .get("recording_fatal_error")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn stream_binding_from_handle(handle: &RuntimeHandle) -> Option<StreamBinding> {
    handle
        .metadata
        .get("stream_binding")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
}

fn rtp_stream_id_from_handle(handle: &RuntimeHandle) -> Option<String> {
    handle
        .metadata
        .get("rtp_stream_id")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn rtp_server_from_handle(handle: &RuntimeHandle) -> Option<RtpServerMetadata> {
    handle
        .metadata
        .get("rtp_server")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
}

fn live_relay_recording_from_handle(handle: &RuntimeHandle) -> Option<LiveRelayRecording> {
    handle
        .metadata
        .get("recording")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
}

fn should_start_live_relay_recording(recording: &LiveRelayRecording) -> bool {
    !recording.started && !recording.failed
}

fn should_fail_on_recording_start_error(recording: &LiveRelayRecording) -> bool {
    recording.duration_sec.is_some()
}

fn recording_duration_reached(recording: &LiveRelayRecording, now: DateTime<Utc>) -> bool {
    let Some(duration_sec) = recording.duration_sec else {
        return false;
    };
    let Some(started_at) = recording.recording_started_at else {
        return false;
    };
    now >= started_at + chrono::Duration::seconds(i64::from(duration_sec))
}

fn mark_recording_started(
    recording: &LiveRelayRecording,
    now: DateTime<Utc>,
) -> LiveRelayRecording {
    let mut updated = recording.clone();
    updated.started = true;
    updated.failed = false;
    updated.recording_started_at = Some(now);
    updated.auto_stop_requested = false;
    updated.completion_reason = None;
    updated
}

fn mark_recording_failed(recording: &LiveRelayRecording) -> LiveRelayRecording {
    let mut updated = recording.clone();
    updated.started = false;
    updated.failed = true;
    updated
}

fn mark_recording_completion(
    recording: &LiveRelayRecording,
    reason: impl Into<String>,
) -> LiveRelayRecording {
    let mut updated = recording.clone();
    updated.auto_stop_requested = true;
    updated.completion_reason = Some(reason.into());
    updated
}

fn live_relay_auto_close_enabled(settings: &AgentSettings, spec: &TaskSpec) -> bool {
    settings.zlm_auto_close_on_no_reader_enabled && spec.expose.stop_on_no_reader.unwrap_or(false)
}

fn live_relay_auto_close_enabled_from_handle(
    settings: &AgentSettings,
    handle: &RuntimeHandle,
) -> bool {
    handle
        .metadata
        .get("resolved_spec")
        .cloned()
        .and_then(|value| serde_json::from_value::<TaskSpec>(value).ok())
        .map(|spec| live_relay_auto_close_enabled(settings, &spec))
        .unwrap_or(false)
}

fn recovery_policy_from_handle(handle: &RuntimeHandle) -> Option<RecoveryPolicy> {
    handle
        .metadata
        .get("resolved_spec")
        .cloned()
        .and_then(|value| serde_json::from_value::<TaskSpec>(value).ok())
        .and_then(|spec| spec.recovery.policy)
}

fn should_auto_restart_process(
    handle: &RuntimeHandle,
    was_stopped: bool,
    status: &Result<std::process::ExitStatus, std::io::Error>,
) -> bool {
    if was_stopped
        || task_type_from_handle(handle) != Some(TaskType::StreamIngest)
        || task_runtime_mode_from_handle(handle) != Some(TaskRuntimeMode::ManagedProcess)
        || !stream_online(handle)
        || fatal_recording_error_from_handle(handle).is_some()
    {
        return false;
    }

    if !matches!(
        recovery_policy_from_handle(handle),
        Some(RecoveryPolicy::Auto)
    ) {
        return false;
    }

    match status {
        Ok(exit_status) => !exit_status.success(),
        Err(_) => true,
    }
}

fn restart_request_from_handle(handle: &RuntimeHandle) -> Result<StartTaskRequest, ExecutorError> {
    Ok(StartTaskRequest {
        task_id: handle.task_id,
        attempt_no: handle.attempt_no,
        task_type: task_type_from_handle(handle).ok_or_else(|| {
            ExecutorError::InvalidRequest("persisted runtime is missing task_type".to_string())
        })?,
        resolved_spec: handle
            .metadata
            .get("resolved_spec")
            .cloned()
            .ok_or_else(|| {
                ExecutorError::InvalidRequest(
                    "persisted runtime is missing resolved_spec".to_string(),
                )
            })?,
        execution_mode: handle
            .metadata
            .get("execution_mode")
            .and_then(Value::as_str)
            .unwrap_or("managed")
            .to_string(),
        lease_token: handle
            .metadata
            .get("lease_token")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ExecutorError::InvalidRequest(
                    "persisted runtime is missing lease_token".to_string(),
                )
            })?
            .to_string(),
        trace_context: handle
            .metadata
            .get("trace_context")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn persist_runtime_state(
    work_dir: &Path,
    handle: &RuntimeHandle,
    success_check: &SuccessCheck,
) -> Result<(), ExecutorError> {
    fs::create_dir_all(work_dir).map_err(|error| {
        ExecutorError::ProcessSpawn(format!(
            "failed to prepare runtime dir {}: {error}",
            work_dir.display()
        ))
    })?;

    let state = PersistedRuntimeState {
        handle: handle.clone(),
        work_dir: work_dir.to_path_buf(),
        success_check: success_check.clone(),
    };
    let state_json = serde_json::to_vec_pretty(&state)
        .map_err(|error| ExecutorError::ProcessSpawn(error.to_string()))?;
    fs::write(work_dir.join(RUNTIME_STATE_FILE), state_json).map_err(|error| {
        ExecutorError::ProcessSpawn(format!(
            "failed to write runtime state {}: {error}",
            work_dir.join(RUNTIME_STATE_FILE).display()
        ))
    })?;

    let pid_path = work_dir.join(RUNTIME_PID_FILE);
    if let Some(pid) = handle.pid {
        fs::write(&pid_path, pid.to_string()).map_err(|error| {
            ExecutorError::ProcessSpawn(format!(
                "failed to write runtime pid {}: {error}",
                pid_path.display()
            ))
        })?;
    } else {
        let _ = fs::remove_file(&pid_path);
    }

    let command_path = work_dir.join(RUNTIME_COMMAND_FILE);
    if let Some(command_line) = handle.command_line.as_deref() {
        fs::write(&command_path, command_line).map_err(|error| {
            ExecutorError::ProcessSpawn(format!(
                "failed to write runtime command {}: {error}",
                command_path.display()
            ))
        })?;
    } else {
        let _ = fs::remove_file(&command_path);
    }

    Ok(())
}

fn success_check_from_handle(handle: &RuntimeHandle) -> SuccessCheck {
    handle
        .outputs
        .iter()
        .rev()
        .find(|output| !output.contains("://"))
        .map(|output| SuccessCheck::FileExists(PathBuf::from(output)))
        .unwrap_or(SuccessCheck::ProcessExit)
}

fn scan_persisted_runtimes(work_root: &str) -> Vec<PersistedRuntimeState> {
    let root = Path::new(work_root);
    if !root.exists() {
        return Vec::new();
    }

    let mut pending = vec![root.to_path_buf()];
    let mut states = Vec::new();
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            if path.file_name().and_then(|name| name.to_str()) != Some(RUNTIME_STATE_FILE) {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            let Ok(state) = serde_json::from_slice::<PersistedRuntimeState>(&bytes) else {
                continue;
            };
            if matches!(
                state.handle.state,
                RuntimeState::Exited | RuntimeState::Pending
            ) {
                continue;
            }
            states.push(state);
        }
    }
    states
}

fn scan_exited_persisted_runtimes(work_root: &str) -> Vec<PersistedRuntimeState> {
    let root = Path::new(work_root);
    if !root.exists() {
        return Vec::new();
    }

    let mut pending = vec![root.to_path_buf()];
    let mut states = Vec::new();
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            if path.file_name().and_then(|name| name.to_str()) != Some(RUNTIME_STATE_FILE) {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            let Ok(state) = serde_json::from_slice::<PersistedRuntimeState>(&bytes) else {
                continue;
            };
            if state.handle.state != RuntimeState::Exited {
                continue;
            }
            states.push(state);
        }
    }
    states
}

fn stop_requested_from_persisted_handle(handle: &RuntimeHandle) -> bool {
    handle
        .metadata
        .get("stop")
        .map(|value| !value.is_null())
        .unwrap_or(false)
}

fn classify_replayed_exit(
    handle: &RuntimeHandle,
    success_check: &SuccessCheck,
) -> (&'static str, &'static str, String, Value) {
    let (event_type, event_level, message, mut payload) = classify_adopted_exit(
        handle,
        success_check,
        stop_requested_from_persisted_handle(handle),
    );
    if let Some(object) = payload.as_object_mut() {
        object.remove("orphaned");
        object.insert("replayed".to_string(), json!(true));
    }
    (event_type, event_level, message, payload)
}

pub fn collect_terminal_runtime_replays(
    work_root: &str,
    registry: &LocalRuntimeRegistry,
) -> Vec<TerminalRuntimeReplay> {
    scan_exited_persisted_runtimes(work_root)
        .into_iter()
        .filter(|state| stop_requested_from_persisted_handle(&state.handle))
        .filter(|state| {
            registry
                .find_by_task_attempt(state.handle.task_id, state.handle.attempt_no)
                .is_none()
        })
        .map(|state| {
            let (event_type, event_level, message, payload) =
                classify_replayed_exit(&state.handle, &state.success_check);
            TerminalRuntimeReplay {
                handle: state.handle.clone(),
                event: RuntimeTaskEvent {
                    task_id: state.handle.task_id,
                    attempt_no: state.handle.attempt_no,
                    event_type: event_type.to_string(),
                    event_level: event_level.to_string(),
                    message,
                    payload,
                },
            }
        })
        .collect()
}

pub fn cleanup_persisted_runtime_state(work_root: &str, task_id: Uuid, attempt_no: i32) {
    let work_dir = Path::new(work_root)
        .join(task_id.to_string())
        .join(format!("attempt-{attempt_no}"));
    let _ = fs::remove_file(work_dir.join(RUNTIME_STATE_FILE));
    let _ = fs::remove_file(work_dir.join(RUNTIME_PID_FILE));
    let _ = fs::remove_file(work_dir.join(RUNTIME_COMMAND_FILE));
}

pub fn is_terminal_runtime_event(event_type: &str) -> bool {
    matches!(event_type, "canceled" | "failed" | "succeeded")
}

fn is_pid_running(pid: i32) -> bool {
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        true
    } else {
        matches!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        )
    }
}

fn spawn_adopted_runtime_monitor(
    handle: RuntimeHandle,
    work_dir: PathBuf,
    success_check: SuccessCheck,
    registry: LocalRuntimeRegistry,
    runtimes: Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    events: RuntimeEventSink,
) {
    let runtime_id = handle.runtime_id;
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(2)).await;

            let runtime = {
                runtimes
                    .read()
                    .expect("runtime map lock poisoned")
                    .get(&runtime_id)
                    .cloned()
            };
            let Some(runtime) = runtime else {
                return;
            };
            let Some(pid) = runtime.pid else {
                return;
            };
            if is_pid_running(pid) {
                continue;
            }

            let stop_requested = runtime.stop_requested.load(Ordering::Relaxed);
            runtimes
                .write()
                .expect("runtime map lock poisoned")
                .remove(&runtime_id);

            let mut exited_handle = registry
                .update(runtime_id, |runtime| {
                    runtime.state = RuntimeState::Exited;
                    runtime.last_progress_at = Some(Utc::now());
                })
                .unwrap_or_else(|| {
                    let mut handle = handle.clone();
                    handle.state = RuntimeState::Exited;
                    handle.last_progress_at = Some(Utc::now());
                    handle
                });
            attach_file_artifact_metadata(&mut exited_handle, &success_check);

            let (event_type, event_level, message, payload) =
                classify_adopted_exit(&exited_handle, &success_check, stop_requested);
            let _ = persist_runtime_state(&work_dir, &exited_handle, &success_check);
            let _ = events.send(RuntimeNotification::TaskSnapshot(exited_handle.clone()));
            let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                task_id: exited_handle.task_id,
                attempt_no: exited_handle.attempt_no,
                event_type: event_type.to_string(),
                event_level: event_level.to_string(),
                message,
                payload,
            }));
            let _ = registry.remove(runtime_id);
            return;
        }
    });
}

fn spawn_startup_probe_monitor(
    runtime_id: Uuid,
    work_dir: PathBuf,
    success_check: SuccessCheck,
    startup_probe: StartupProbe,
    settings: AgentSettings,
    http_client: Client,
    registry: LocalRuntimeRegistry,
    runtimes: Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    events: RuntimeEventSink,
) {
    tokio::spawn(async move {
        let started_at = tokio::time::Instant::now();
        loop {
            if started_at.elapsed() >= STARTUP_PROBE_TIMEOUT {
                let updated = registry.update(runtime_id, |runtime| {
                    runtime.metadata["startup_timeout"] = json!(true);
                    runtime.metadata["stream_online"] = json!(false);
                });
                if let Some(handle) = updated {
                    let _ = persist_runtime_state(&work_dir, &handle, &success_check);
                    let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                        task_id: handle.task_id,
                        attempt_no: handle.attempt_no,
                        event_type: "startup_timeout".to_string(),
                        event_level: "error".to_string(),
                        message: format!(
                            "ZLM stream {}/{}/{} did not become online within {} seconds",
                            startup_probe.vhost,
                            startup_probe.app,
                            startup_probe.stream,
                            STARTUP_PROBE_TIMEOUT.as_secs()
                        ),
                        payload: json!({
                            "schema": startup_probe.schema,
                            "vhost": startup_probe.vhost,
                            "app": startup_probe.app,
                            "stream": startup_probe.stream,
                        }),
                    }));
                }
                if let Some(runtime) = runtimes
                    .read()
                    .expect("runtime map lock poisoned")
                    .get(&runtime_id)
                    .cloned()
                {
                    if let Some(pid) = runtime.pid {
                        let _ = signal_pid(pid, libc::SIGTERM);
                    }
                }
                return;
            }

            let handle = registry.get(runtime_id);
            let Some(handle) = handle else {
                return;
            };
            let Some(pid) = handle.pid else {
                return;
            };
            if !is_pid_running(pid) {
                return;
            }

            if zlm_stream_online(&http_client, &settings, &startup_probe)
                .await
                .unwrap_or(false)
            {
                let binding = stream_binding_from_handle(&handle).unwrap_or(StreamBinding {
                    schema: startup_probe.schema.clone(),
                    vhost: startup_probe.vhost.clone(),
                    app: startup_probe.app.clone(),
                    stream: startup_probe.stream.clone(),
                });
                let mut recording_started = false;
                if let Some(recording) = live_relay_recording_from_handle(&handle)
                    .filter(should_start_live_relay_recording)
                {
                    match start_stream_recording(
                        &http_client,
                        &settings,
                        &binding,
                        &recording,
                        Utc::now(),
                    )
                    .await
                    {
                        Ok(updated_recording) => {
                            let updated_handle = registry
                                .update(runtime_id, |runtime| {
                                    runtime.metadata["recording"] =
                                        json!(updated_recording.clone());
                                })
                                .unwrap_or_else(|| {
                                    let mut handle = handle.clone();
                                    handle.metadata["recording"] = json!(updated_recording);
                                    handle
                                });
                            let _ =
                                persist_runtime_state(&work_dir, &updated_handle, &success_check);
                            let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                                task_id: updated_handle.task_id,
                                attempt_no: updated_handle.attempt_no,
                                event_type: "recording_started".to_string(),
                                event_level: "info".to_string(),
                                message: "stream recording started".to_string(),
                                payload: json!({
                                    "formats": recording.formats,
                                    "root_path": recording.root_path,
                                    "duration_sec": recording.duration_sec,
                                    "segment_sec": recording.segment_sec,
                                    "as_player": recording.as_player,
                                }),
                            }));
                            recording_started = true;
                        }
                        Err(error) => {
                            let failed_recording = mark_recording_failed(&recording);
                            let fatal = should_fail_on_recording_start_error(&recording);
                            let updated_handle = registry
                                .update(runtime_id, |runtime| {
                                    runtime.last_progress_at = Some(Utc::now());
                                    runtime.metadata["stream_online"] = json!(true);
                                    runtime.metadata["stream_binding"] = json!({
                                        "schema": binding.schema,
                                        "vhost": binding.vhost,
                                        "app": binding.app,
                                        "stream": binding.stream,
                                    });
                                    runtime.metadata["recording_error"] = json!(error.to_string());
                                    runtime.metadata["recording"] = json!(failed_recording.clone());
                                    if fatal {
                                        runtime.metadata["recording_fatal_error"] =
                                            json!(error.to_string());
                                    }
                                })
                                .unwrap_or_else(|| {
                                    let mut handle = handle.clone();
                                    handle.last_progress_at = Some(Utc::now());
                                    handle.metadata["stream_online"] = json!(true);
                                    handle.metadata["stream_binding"] = json!({
                                        "schema": binding.schema,
                                        "vhost": binding.vhost,
                                        "app": binding.app,
                                        "stream": binding.stream,
                                    });
                                    handle.metadata["recording_error"] = json!(error.to_string());
                                    handle.metadata["recording"] = json!(failed_recording);
                                    if fatal {
                                        handle.metadata["recording_fatal_error"] =
                                            json!(error.to_string());
                                    }
                                    handle
                                });
                            let _ =
                                persist_runtime_state(&work_dir, &updated_handle, &success_check);
                            let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                                task_id: updated_handle.task_id,
                                attempt_no: updated_handle.attempt_no,
                                event_type: "zlm_api_error".to_string(),
                                event_level: "error".to_string(),
                                message: format!("failed to start stream recording: {error}"),
                                payload: json!({
                                    "schema": binding.schema,
                                    "vhost": binding.vhost,
                                    "app": binding.app,
                                    "stream": binding.stream,
                                    "record_root": recording.root_path,
                                    "duration_sec": recording.duration_sec,
                                }),
                            }));
                            if fatal {
                                let _ =
                                    events.send(RuntimeNotification::TaskSnapshot(updated_handle));
                                let _ = signal_pid(pid, libc::SIGTERM);
                                return;
                            }
                            let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                                task_id: updated_handle.task_id,
                                attempt_no: updated_handle.attempt_no,
                                event_type: "recording_degraded".to_string(),
                                event_level: "warn".to_string(),
                                message:
                                    "stream recording startup failed; continuing without recording"
                                        .to_string(),
                                payload: json!({
                                    "schema": binding.schema,
                                    "vhost": binding.vhost,
                                    "app": binding.app,
                                    "stream": binding.stream,
                                    "record_root": recording.root_path,
                                }),
                            }));
                            let _ = events.send(RuntimeNotification::TaskSnapshot(updated_handle));
                        }
                    }
                }
                let running_handle = registry
                    .update(runtime_id, |runtime| {
                        runtime.state = RuntimeState::Running;
                        runtime.last_progress_at = Some(Utc::now());
                        runtime.metadata["stream_online"] = json!(true);
                        runtime.metadata["stream_binding"] = json!({
                            "schema": startup_probe.schema,
                            "vhost": startup_probe.vhost,
                            "app": startup_probe.app,
                            "stream": startup_probe.stream,
                        });
                    })
                    .unwrap_or(handle);
                let _ = persist_runtime_state(&work_dir, &running_handle, &success_check);
                let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                    task_id: running_handle.task_id,
                    attempt_no: running_handle.attempt_no,
                    event_type: "running".to_string(),
                    event_level: "info".to_string(),
                    message: "ZLM stream is online".to_string(),
                    payload: json!({
                        "runtime_id": running_handle.runtime_id,
                        "pid": running_handle.pid,
                        "schema": startup_probe.schema,
                        "vhost": startup_probe.vhost,
                        "app": startup_probe.app,
                        "stream": startup_probe.stream,
                        "recording_started": recording_started,
                    }),
                }));
                let _ = events.send(RuntimeNotification::TaskSnapshot(running_handle));
                return;
            }

            sleep(STARTUP_PROBE_POLL_INTERVAL).await;
        }
    });
}

fn spawn_live_relay_monitor(
    runtime_id: Uuid,
    work_dir: PathBuf,
    startup_probe: StartupProbe,
    settings: AgentSettings,
    http_client: Client,
    registry: LocalRuntimeRegistry,
    runtimes: Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    events: RuntimeEventSink,
) {
    tokio::spawn(async move {
        let started_at = tokio::time::Instant::now();
        loop {
            let runtime = {
                runtimes
                    .read()
                    .expect("runtime map lock poisoned")
                    .get(&runtime_id)
                    .cloned()
            };
            let Some(runtime) = runtime else {
                return;
            };
            let stop_requested = runtime.stop_requested.load(Ordering::Relaxed);
            let handle = registry.get(runtime_id);
            let Some(handle) = handle else {
                runtimes
                    .write()
                    .expect("runtime map lock poisoned")
                    .remove(&runtime_id);
                return;
            };

            let stream_state = zlm_stream_online(&http_client, &settings, &startup_probe).await;
            match stream_state {
                Ok(true) => {
                    let mut recording_started = false;
                    if let Some(recording) = live_relay_recording_from_handle(&handle)
                        .filter(should_start_live_relay_recording)
                    {
                        let binding =
                            stream_binding_from_handle(&handle).unwrap_or(StreamBinding {
                                schema: startup_probe.schema.clone(),
                                vhost: startup_probe.vhost.clone(),
                                app: startup_probe.app.clone(),
                                stream: startup_probe.stream.clone(),
                            });
                        match start_stream_recording(
                            &http_client,
                            &settings,
                            &binding,
                            &recording,
                            Utc::now(),
                        )
                        .await
                        {
                            Ok(updated_recording) => {
                                let updated_handle = registry
                                    .update(runtime_id, |runtime| {
                                        runtime.metadata["recording"] =
                                            json!(updated_recording.clone());
                                    })
                                    .unwrap_or_else(|| {
                                        let mut handle = handle.clone();
                                        handle.metadata["recording"] = json!(updated_recording);
                                        handle
                                    });
                                let _ = persist_runtime_state(
                                    &work_dir,
                                    &updated_handle,
                                    &SuccessCheck::ProcessExit,
                                );
                                let _ =
                                    events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                                        task_id: updated_handle.task_id,
                                        attempt_no: updated_handle.attempt_no,
                                        event_type: "recording_started".to_string(),
                                        event_level: "info".to_string(),
                                        message: "live_relay recording started".to_string(),
                                        payload: json!({
                                            "formats": recording.formats,
                                            "root_path": recording.root_path,
                                            "duration_sec": recording.duration_sec,
                                            "segment_sec": recording.segment_sec,
                                            "as_player": recording.as_player,
                                        }),
                                    }));
                                recording_started = true;
                            }
                            Err(error) => {
                                let failed_recording = mark_recording_failed(&recording);
                                let fatal = should_fail_on_recording_start_error(&recording);
                                let degraded_handle = registry
                                    .update(runtime_id, |runtime| {
                                        runtime.last_progress_at = Some(Utc::now());
                                        runtime.metadata["stream_online"] = json!(true);
                                        runtime.metadata["recording_error"] =
                                            json!(error.to_string());
                                        runtime.metadata["recording"] =
                                            json!(failed_recording.clone());
                                        if fatal {
                                            runtime.metadata["recording_fatal_error"] =
                                                json!(error.to_string());
                                        }
                                    })
                                    .unwrap_or_else(|| {
                                        let mut handle = handle.clone();
                                        handle.last_progress_at = Some(Utc::now());
                                        handle.metadata["stream_online"] = json!(true);
                                        handle.metadata["recording_error"] =
                                            json!(error.to_string());
                                        handle.metadata["recording"] = json!(failed_recording);
                                        if fatal {
                                            handle.metadata["recording_fatal_error"] =
                                                json!(error.to_string());
                                        }
                                        handle
                                    });
                                let _ =
                                    events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                                        task_id: degraded_handle.task_id,
                                        attempt_no: degraded_handle.attempt_no,
                                        event_type: "zlm_api_error".to_string(),
                                        event_level: "error".to_string(),
                                        message: format!(
                                            "failed to start live_relay recording: {error}"
                                        ),
                                        payload: json!({
                                            "schema": binding.schema,
                                            "vhost": binding.vhost,
                                            "app": binding.app,
                                            "stream": binding.stream,
                                            "record_root": recording.root_path,
                                            "duration_sec": recording.duration_sec,
                                        }),
                                    }));
                                let _ = persist_runtime_state(
                                    &work_dir,
                                    &degraded_handle,
                                    &SuccessCheck::ProcessExit,
                                );
                                if fatal {
                                    let _ = events.send(RuntimeNotification::TaskSnapshot(
                                        degraded_handle.clone(),
                                    ));
                                    let _ = stop_live_relay_recording(
                                        &http_client,
                                        &settings,
                                        &binding,
                                        &recording,
                                    )
                                    .await;
                                    let _ = call_zlm_api(
                                        &http_client,
                                        &settings,
                                        "/index/api/close_streams",
                                        &build_close_stream_params(&binding, true),
                                    )
                                    .await;
                                    let failed_handle = registry
                                        .update(runtime_id, |runtime| {
                                            runtime.state = RuntimeState::Exited;
                                            runtime.last_progress_at = Some(Utc::now());
                                        })
                                        .unwrap_or(degraded_handle.clone());
                                    let _ = persist_runtime_state(
                                        &work_dir,
                                        &failed_handle,
                                        &SuccessCheck::ProcessExit,
                                    );
                                    let _ = events.send(RuntimeNotification::TaskSnapshot(
                                        failed_handle.clone(),
                                    ));
                                    let _ = events.send(RuntimeNotification::TaskEvent(
                                        RuntimeTaskEvent {
                                            task_id: failed_handle.task_id,
                                            attempt_no: failed_handle.attempt_no,
                                            event_type: "failed".to_string(),
                                            event_level: "error".to_string(),
                                            message: "live_relay recording startup failed"
                                                .to_string(),
                                            payload: json!({
                                                "schema": binding.schema,
                                                "vhost": binding.vhost,
                                                "app": binding.app,
                                                "stream": binding.stream,
                                                "record_root": recording.root_path,
                                                "reason": "recording_start_failed",
                                            }),
                                        },
                                    ));
                                    runtimes
                                        .write()
                                        .expect("runtime map lock poisoned")
                                        .remove(&runtime_id);
                                    let _ = registry.remove(runtime_id);
                                    return;
                                }
                                let _ =
                                    events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                                        task_id: degraded_handle.task_id,
                                        attempt_no: degraded_handle.attempt_no,
                                        event_type: "recording_degraded".to_string(),
                                        event_level: "warn".to_string(),
                                        message: "live_relay recording startup failed; continuing without recording"
                                            .to_string(),
                                        payload: json!({
                                            "schema": binding.schema,
                                            "vhost": binding.vhost,
                                            "app": binding.app,
                                            "stream": binding.stream,
                                            "record_root": recording.root_path,
                                        }),
                                    }));
                                let _ =
                                    events.send(RuntimeNotification::TaskSnapshot(degraded_handle));
                            }
                        }
                    }
                    let handle = registry.get(runtime_id).unwrap_or(handle.clone());
                    if let Some(recording) = live_relay_recording_from_handle(&handle)
                        .filter(|recording| recording.started)
                        .filter(|recording| recording_duration_reached(recording, Utc::now()))
                    {
                        let completion_handle = if recording.auto_stop_requested {
                            handle.clone()
                        } else {
                            let completed_recording =
                                mark_recording_completion(&recording, "record_duration_reached");
                            registry
                                .update(runtime_id, |runtime| {
                                    runtime.last_progress_at = Some(Utc::now());
                                    runtime.metadata["recording"] =
                                        json!(completed_recording.clone());
                                    runtime.metadata["completion_reason"] =
                                        json!("record_duration_reached");
                                })
                                .unwrap_or_else(|| {
                                    let mut handle = handle.clone();
                                    handle.last_progress_at = Some(Utc::now());
                                    handle.metadata["recording"] =
                                        json!(completed_recording.clone());
                                    handle.metadata["completion_reason"] =
                                        json!("record_duration_reached");
                                    handle
                                })
                        };
                        let _ = persist_runtime_state(
                            &work_dir,
                            &completion_handle,
                            &SuccessCheck::ProcessExit,
                        );
                        if let Some(runtime) = runtimes
                            .read()
                            .expect("runtime map lock poisoned")
                            .get(&runtime_id)
                            .cloned()
                        {
                            runtime.stop_requested.store(true, Ordering::Relaxed);
                        }
                        let binding = stream_binding_from_handle(&completion_handle).unwrap_or(
                            StreamBinding {
                                schema: startup_probe.schema.clone(),
                                vhost: startup_probe.vhost.clone(),
                                app: startup_probe.app.clone(),
                                stream: startup_probe.stream.clone(),
                            },
                        );
                        let _ = stop_live_relay_recording(
                            &http_client,
                            &settings,
                            &binding,
                            &recording,
                        )
                        .await;
                        let _ = call_zlm_api(
                            &http_client,
                            &settings,
                            "/index/api/close_streams",
                            &build_close_stream_params(&binding, true),
                        )
                        .await;
                        continue;
                    }
                    let should_emit_running = handle.state != RuntimeState::Running
                        || !stream_online(&handle)
                        || recording_started;
                    if should_emit_running {
                        let running_handle = registry
                            .update(runtime_id, |runtime| {
                                runtime.state = RuntimeState::Running;
                                runtime.last_progress_at = Some(Utc::now());
                                runtime.metadata["stream_online"] = json!(true);
                                runtime.metadata["stream_binding"] = json!({
                                    "schema": startup_probe.schema,
                                    "vhost": startup_probe.vhost,
                                    "app": startup_probe.app,
                                    "stream": startup_probe.stream,
                                });
                            })
                            .unwrap_or(handle);
                        let _ = persist_runtime_state(
                            &work_dir,
                            &running_handle,
                            &SuccessCheck::ProcessExit,
                        );
                        let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                            task_id: running_handle.task_id,
                            attempt_no: running_handle.attempt_no,
                            event_type: "running".to_string(),
                            event_level: "info".to_string(),
                            message: "ZLM live_relay stream is online".to_string(),
                            payload: json!({
                                "runtime_id": running_handle.runtime_id,
                                "schema": startup_probe.schema,
                                "vhost": startup_probe.vhost,
                                "app": startup_probe.app,
                                "stream": startup_probe.stream,
                            }),
                        }));
                        let _ = events.send(RuntimeNotification::TaskSnapshot(running_handle));
                    }
                }
                Ok(false)
                    if !stream_online(&handle) && started_at.elapsed() >= STARTUP_PROBE_TIMEOUT =>
                {
                    let failed_handle = registry
                        .update(runtime_id, |runtime| {
                            runtime.state = RuntimeState::Exited;
                            runtime.last_progress_at = Some(Utc::now());
                            runtime.metadata["startup_timeout"] = json!(true);
                            runtime.metadata["stream_online"] = json!(false);
                        })
                        .unwrap_or_else(|| {
                            let mut handle = handle.clone();
                            handle.state = RuntimeState::Exited;
                            handle.last_progress_at = Some(Utc::now());
                            handle
                        });
                    let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                        task_id: failed_handle.task_id,
                        attempt_no: failed_handle.attempt_no,
                        event_type: "startup_timeout".to_string(),
                        event_level: "error".to_string(),
                        message: format!(
                            "live_relay stream {}/{}/{} did not become online within {} seconds",
                            startup_probe.vhost,
                            startup_probe.app,
                            startup_probe.stream,
                            STARTUP_PROBE_TIMEOUT.as_secs()
                        ),
                        payload: json!({
                            "schema": startup_probe.schema,
                            "vhost": startup_probe.vhost,
                            "app": startup_probe.app,
                            "stream": startup_probe.stream,
                        }),
                    }));
                    let _ = events.send(RuntimeNotification::TaskSnapshot(failed_handle.clone()));
                    let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                        task_id: failed_handle.task_id,
                        attempt_no: failed_handle.attempt_no,
                        event_type: "failed".to_string(),
                        event_level: "error".to_string(),
                        message: "live_relay startup timed out".to_string(),
                        payload: json!({
                            "schema": startup_probe.schema,
                            "vhost": startup_probe.vhost,
                            "app": startup_probe.app,
                            "stream": startup_probe.stream,
                        }),
                    }));
                    let _ = persist_runtime_state(
                        &work_dir,
                        &failed_handle,
                        &SuccessCheck::ProcessExit,
                    );
                    runtimes
                        .write()
                        .expect("runtime map lock poisoned")
                        .remove(&runtime_id);
                    let _ = registry.remove(runtime_id);
                    return;
                }
                Ok(false) if stream_online(&handle) => {
                    let exited_handle = registry
                        .update(runtime_id, |runtime| {
                            runtime.state = RuntimeState::Exited;
                            runtime.last_progress_at = Some(Utc::now());
                            runtime.metadata["stream_online"] = json!(false);
                        })
                        .unwrap_or_else(|| {
                            let mut handle = handle.clone();
                            handle.state = RuntimeState::Exited;
                            handle.last_progress_at = Some(Utc::now());
                            handle
                        });
                    let auto_close_enabled =
                        live_relay_auto_close_enabled_from_handle(&settings, &handle);
                    let completion_reason = completion_reason_from_handle(&exited_handle);
                    let (event_type, event_level, message, reason) =
                        if completion_reason.as_deref() == Some("record_duration_reached") {
                            (
                                "succeeded",
                                "info",
                                "live_relay completed after recording duration reached".to_string(),
                                "record_duration_reached",
                            )
                        } else if stop_requested {
                            (
                                "canceled",
                                "info",
                                "live_relay stream stopped".to_string(),
                                "stop_requested",
                            )
                        } else if auto_close_enabled {
                            (
                                "canceled",
                                "info",
                                "live_relay stopped after no-reader auto-close policy".to_string(),
                                "no_reader_auto_close",
                            )
                        } else {
                            (
                                "failed",
                                "error",
                                "live_relay stream went offline unexpectedly".to_string(),
                                "unexpected_offline",
                            )
                        };
                    let _ = persist_runtime_state(
                        &work_dir,
                        &exited_handle,
                        &SuccessCheck::ProcessExit,
                    );
                    let _ = events.send(RuntimeNotification::TaskSnapshot(exited_handle.clone()));
                    let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                        task_id: exited_handle.task_id,
                        attempt_no: exited_handle.attempt_no,
                        event_type: event_type.to_string(),
                        event_level: event_level.to_string(),
                        message,
                        payload: json!({
                            "schema": startup_probe.schema,
                            "vhost": startup_probe.vhost,
                            "app": startup_probe.app,
                            "stream": startup_probe.stream,
                            "reason": reason,
                            "orphaned": exited_handle.metadata.get("orphaned").and_then(Value::as_bool).unwrap_or(false),
                        }),
                    }));
                    runtimes
                        .write()
                        .expect("runtime map lock poisoned")
                        .remove(&runtime_id);
                    let _ = registry.remove(runtime_id);
                    return;
                }
                Ok(false) | Err(_) => {}
            }

            sleep(STARTUP_PROBE_POLL_INTERVAL).await;
        }
    });
}

fn spawn_rtp_receive_monitor(
    runtime_id: Uuid,
    work_dir: PathBuf,
    stream_id: String,
    settings: AgentSettings,
    http_client: Client,
    registry: LocalRuntimeRegistry,
    runtimes: Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    events: RuntimeEventSink,
) {
    tokio::spawn(async move {
        loop {
            let runtime = {
                runtimes
                    .read()
                    .expect("runtime map lock poisoned")
                    .get(&runtime_id)
                    .cloned()
            };
            let Some(runtime) = runtime else {
                return;
            };
            let stop_requested = runtime.stop_requested.load(Ordering::Relaxed);
            let handle = registry.get(runtime_id);
            let Some(handle) = handle else {
                runtimes
                    .write()
                    .expect("runtime map lock poisoned")
                    .remove(&runtime_id);
                return;
            };

            match zlm_rtp_server_port(&http_client, &settings, &stream_id).await {
                Ok(Some(local_port)) => {
                    let should_emit_running =
                        handle.state != RuntimeState::Running || !stream_online(&handle);
                    if should_emit_running {
                        if let Ok(Some(binding)) =
                            zlm_stream_binding_by_stream_id(&http_client, &settings, &stream_id)
                                .await
                        {
                            let running_handle = registry
                                .update(runtime_id, |runtime| {
                                    runtime.state = RuntimeState::Running;
                                    runtime.last_progress_at = Some(Utc::now());
                                    runtime.metadata["stream_online"] = json!(true);
                                    runtime.metadata["stream_binding"] = json!({
                                        "schema": binding.schema,
                                        "vhost": binding.vhost,
                                        "app": binding.app,
                                        "stream": binding.stream,
                                    });
                                    if let Some(mut rtp_server) = runtime
                                        .metadata
                                        .get("rtp_server")
                                        .cloned()
                                        .and_then(|value| {
                                            serde_json::from_value::<RtpServerMetadata>(value).ok()
                                        })
                                    {
                                        rtp_server.local_port = local_port;
                                        runtime.metadata["rtp_server"] = json!(rtp_server);
                                    }
                                })
                                .unwrap_or_else(|| handle.clone());
                            let _ = persist_runtime_state(
                                &work_dir,
                                &running_handle,
                                &SuccessCheck::ProcessExit,
                            );
                            let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                                task_id: running_handle.task_id,
                                attempt_no: running_handle.attempt_no,
                                event_type: "running".to_string(),
                                event_level: "info".to_string(),
                                message: "rtp_receive stream is online".to_string(),
                                payload: json!({
                                    "runtime_id": running_handle.runtime_id,
                                    "rtp_stream_id": stream_id.clone(),
                                    "local_port": local_port,
                                    "schema": binding.schema,
                                    "vhost": binding.vhost,
                                    "app": binding.app,
                                    "stream": binding.stream,
                                }),
                            }));
                            let _ = events.send(RuntimeNotification::TaskSnapshot(running_handle));
                        }
                    }
                }
                Ok(None) => {
                    runtimes
                        .write()
                        .expect("runtime map lock poisoned")
                        .remove(&runtime_id);
                    let exited_handle = registry
                        .update(runtime_id, |runtime| {
                            runtime.state = RuntimeState::Exited;
                            runtime.last_progress_at = Some(Utc::now());
                            runtime.metadata["stream_online"] = json!(false);
                        })
                        .unwrap_or_else(|| {
                            let mut handle = handle.clone();
                            handle.state = RuntimeState::Exited;
                            handle.last_progress_at = Some(Utc::now());
                            handle.metadata["stream_online"] = json!(false);
                            handle
                        });
                    let _ = persist_runtime_state(
                        &work_dir,
                        &exited_handle,
                        &SuccessCheck::ProcessExit,
                    );
                    if !stop_requested {
                        let _ =
                            events.send(RuntimeNotification::TaskSnapshot(exited_handle.clone()));
                        let _ =
                            events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                                task_id: exited_handle.task_id,
                                attempt_no: exited_handle.attempt_no,
                                event_type: "rtp_server_closed".to_string(),
                                event_level: "warn".to_string(),
                                message: "rtp_receive server disappeared from ZLM".to_string(),
                                payload: json!({
                                    "rtp_stream_id": stream_id.clone(),
                                    "orphaned": exited_handle.metadata.get("orphaned").and_then(Value::as_bool).unwrap_or(false),
                                }),
                            }));
                    }
                    let _ = registry.remove(runtime_id);
                    return;
                }
                Err(_) => {}
            }

            sleep(STARTUP_PROBE_POLL_INTERVAL).await;
        }
    });
}

async fn start_live_relay_recording(
    client: &Client,
    settings: &AgentSettings,
    binding: &StreamBinding,
    recording: &LiveRelayRecording,
) -> Result<(), ExecutorError> {
    for kind in &recording.formats {
        call_zlm_api(
            client,
            settings,
            "/index/api/startRecord",
            &build_record_api_params(binding, recording, kind),
        )
        .await?;
    }
    Ok(())
}

async fn start_stream_recording(
    client: &Client,
    settings: &AgentSettings,
    binding: &StreamBinding,
    recording: &LiveRelayRecording,
    now: DateTime<Utc>,
) -> Result<LiveRelayRecording, ExecutorError> {
    start_live_relay_recording(client, settings, binding, recording).await?;
    Ok(mark_recording_started(recording, now))
}

async fn stop_live_relay_recording(
    client: &Client,
    settings: &AgentSettings,
    binding: &StreamBinding,
    recording: &LiveRelayRecording,
) -> Result<(), ExecutorError> {
    for kind in &recording.formats {
        call_zlm_api(
            client,
            settings,
            "/index/api/stopRecord",
            &build_record_api_params(binding, recording, kind),
        )
        .await?;
    }
    Ok(())
}

async fn zlm_stream_online(
    client: &Client,
    settings: &AgentSettings,
    target: &StartupProbe,
) -> anyhow::Result<bool> {
    let url = build_zlm_url(settings, "/index/api/getMediaList")?;
    let response = client.get(url).send().await?.error_for_status()?;
    let body: Value = response.json().await?;
    Ok(zlm_stream_online_in_body(&body, target))
}

async fn zlm_rtp_server_port(
    client: &Client,
    settings: &AgentSettings,
    stream_id: &str,
) -> Result<Option<u16>, ExecutorError> {
    let body = call_zlm_api(client, settings, "/index/api/listRtpServer", &[]).await?;
    Ok(body
        .get("data")
        .and_then(Value::as_array)
        .and_then(|servers| {
            servers.iter().find_map(|entry| {
                let matches_stream =
                    entry.get("stream_id").and_then(Value::as_str) == Some(stream_id);
                if !matches_stream {
                    return None;
                }
                entry
                    .get("port")
                    .and_then(Value::as_u64)
                    .and_then(|value| u16::try_from(value).ok())
            })
        }))
}

async fn zlm_stream_binding_by_stream_id(
    client: &Client,
    settings: &AgentSettings,
    stream_id: &str,
) -> anyhow::Result<Option<StreamBinding>> {
    let url = build_zlm_url(settings, "/index/api/getMediaList")?;
    let response = client.get(url).send().await?.error_for_status()?;
    let body: Value = response.json().await?;
    Ok(body
        .get("data")
        .and_then(Value::as_array)
        .and_then(|media| {
            media.iter().find_map(|entry| {
                if entry.get("stream").and_then(Value::as_str) != Some(stream_id) {
                    return None;
                }
                Some(StreamBinding {
                    schema: entry
                        .get("schema")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    vhost: entry
                        .get("vhost")
                        .and_then(Value::as_str)
                        .unwrap_or(ZLM_RUNTIME_VHOST)
                        .to_string(),
                    app: entry.get("app").and_then(Value::as_str)?.to_string(),
                    stream: entry.get("stream").and_then(Value::as_str)?.to_string(),
                })
            })
        }))
}

async fn wait_for_zlm_api_ready(
    client: &Client,
    settings: &AgentSettings,
    timeout: Duration,
) -> bool {
    let started_at = tokio::time::Instant::now();
    loop {
        if zlm_api_ready(client, settings).await {
            return true;
        }
        if started_at.elapsed() >= timeout {
            return false;
        }
        sleep(PROCESS_RECOVERY_POLL_INTERVAL).await;
    }
}

async fn zlm_api_ready(client: &Client, settings: &AgentSettings) -> bool {
    let Ok(url) = build_zlm_url(settings, "/index/api/version") else {
        return false;
    };
    match client.get(url).send().await {
        Ok(response) => response.error_for_status().is_ok(),
        Err(_) => false,
    }
}

async fn call_zlm_api(
    client: &Client,
    settings: &AgentSettings,
    path: &str,
    params: &[(String, String)],
) -> Result<Value, ExecutorError> {
    let mut url = build_zlm_url(settings, path)?;
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in params {
            query.append_pair(key, value);
        }
    }
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| ExecutorError::ApiCall(error.to_string()))?
        .error_for_status()
        .map_err(|error| ExecutorError::ApiCall(error.to_string()))?;
    let body: Value = response
        .json()
        .await
        .map_err(|error| ExecutorError::ApiCall(error.to_string()))?;
    ensure_zlm_success(path, body)
}

fn build_zlm_url(settings: &AgentSettings, path: &str) -> Result<Url, ExecutorError> {
    let mut url = Url::parse(settings.zlm_api_base.trim())
        .map_err(|error| ExecutorError::ApiCall(error.to_string()))?
        .join(path)
        .map_err(|error| ExecutorError::ApiCall(error.to_string()))?;
    if !settings.zlm_api_secret.trim().is_empty() {
        url.query_pairs_mut()
            .append_pair("secret", settings.zlm_api_secret.trim());
    }
    Ok(url)
}

fn ensure_zlm_success(path: &str, body: Value) -> Result<Value, ExecutorError> {
    match body.get("code").and_then(Value::as_i64) {
        Some(0) | None => Ok(body),
        Some(code) => Err(ExecutorError::ApiCall(format!(
            "{path} returned code {code}: {}",
            body.get("msg")
                .and_then(Value::as_str)
                .unwrap_or("unknown ZLM error")
        ))),
    }
}

fn zlm_stream_online_in_body(body: &Value, target: &StartupProbe) -> bool {
    body.get("data")
        .and_then(Value::as_array)
        .map(|media| {
            media.iter().any(|entry| {
                entry.get("app").and_then(Value::as_str) == Some(target.app.as_str())
                    && entry.get("stream").and_then(Value::as_str) == Some(target.stream.as_str())
                    && entry.get("vhost").and_then(Value::as_str) == Some(target.vhost.as_str())
                    && target.schema.as_deref().is_none_or(|schema| {
                        entry.get("schema").and_then(Value::as_str) == Some(schema)
                    })
            })
        })
        .unwrap_or(false)
}

fn extract_zlm_proxy_key(body: &Value) -> Option<String> {
    body.get("data")
        .and_then(|data| data.get("key").or_else(|| data.get("proxy_key")))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn extract_zlm_local_port(body: &Value) -> Option<u16> {
    body.get("port")
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .or_else(|| {
            body.get("data")
                .and_then(|data| data.get("port"))
                .and_then(Value::as_u64)
                .and_then(|value| u16::try_from(value).ok())
        })
}

fn build_record_api_params(
    binding: &StreamBinding,
    recording: &LiveRelayRecording,
    kind: &ZlmRecordKind,
) -> Vec<(String, String)> {
    let mut params = vec![
        ("type".to_string(), zlm_record_kind_code(kind).to_string()),
        ("vhost".to_string(), binding.vhost.clone()),
        ("app".to_string(), binding.app.clone()),
        ("stream".to_string(), binding.stream.clone()),
        ("customized_path".to_string(), recording.root_path.clone()),
    ];
    if let Some(schema) = &binding.schema {
        params.push(("schema".to_string(), schema.clone()));
    }
    if matches!(kind, ZlmRecordKind::Mp4) {
        params.push((
            "max_second".to_string(),
            recording.segment_sec.unwrap_or(3600).to_string(),
        ));
    }
    params
}

fn build_close_stream_params(binding: &StreamBinding, force: bool) -> Vec<(String, String)> {
    let mut params = vec![
        ("vhost".to_string(), binding.vhost.clone()),
        ("app".to_string(), binding.app.clone()),
        ("stream".to_string(), binding.stream.clone()),
        (
            "force".to_string(),
            if force { "1" } else { "0" }.to_string(),
        ),
    ];
    if let Some(schema) = &binding.schema {
        params.push(("schema".to_string(), schema.clone()));
    }
    params
}

fn zlm_record_kind_code(kind: &ZlmRecordKind) -> u8 {
    match kind {
        ZlmRecordKind::Hls => 0,
        ZlmRecordKind::Mp4 => 1,
    }
}

fn attach_file_artifact_metadata(handle: &mut RuntimeHandle, success_check: &SuccessCheck) {
    let SuccessCheck::FileExists(path) = success_check else {
        return;
    };
    let Some(kind) = managed_file_output_kind_from_handle(handle) else {
        return;
    };

    if kind == ManagedFileOutputKind::StreamIngestRecord {
        let mut artifacts = handle
            .outputs
            .iter()
            .filter_map(|output| file_artifact_metadata_from_path(Path::new(output)))
            .collect::<Vec<_>>();
        if artifacts.is_empty() {
            if let Some(metadata) = file_artifact_metadata_from_path(path) {
                artifacts.push(metadata);
            }
        }
        let Some(object) = handle.metadata.as_object_mut() else {
            return;
        };
        if !artifacts.is_empty() {
            object.insert(kind.metadata_key().to_string(), Value::Array(artifacts));
        }
        return;
    }

    let Some(metadata) = file_artifact_metadata_from_path(path) else {
        return;
    };
    let Some(object) = handle.metadata.as_object_mut() else {
        return;
    };
    object.insert(kind.metadata_key().to_string(), metadata);
}

fn classify_adopted_exit(
    handle: &RuntimeHandle,
    success_check: &SuccessCheck,
    stop_requested: bool,
) -> (&'static str, &'static str, String, Value) {
    let output_target = handle.outputs.first().cloned().unwrap_or_default();
    if let Some(reason) =
        completion_reason_from_handle(handle).filter(|reason| reason == "record_duration_reached")
    {
        return (
            "succeeded",
            "info",
            "adopted child process completed after recording duration reached".to_string(),
            json!({
                "output_target": output_target,
                "orphaned": true,
                "reason": reason,
            }),
        );
    }
    if let Some(error) = fatal_recording_error_from_handle(handle) {
        return (
            "failed",
            "error",
            format!("adopted child process stopped after recording startup failed: {error}"),
            json!({
                "output_target": output_target,
                "orphaned": true,
                "recording_error": error,
            }),
        );
    }
    if stop_requested {
        return (
            "canceled",
            "info",
            "adopted child process stopped".to_string(),
            json!({
                "output_target": output_target,
                "orphaned": true,
            }),
        );
    }

    match success_check {
        _ if requires_stream_online(handle) && !stream_online(handle) => (
            "failed",
            "error",
            "adopted child process exited before ZLM stream became online".to_string(),
            json!({
                "output_target": output_target,
                "orphaned": true,
            }),
        ),
        SuccessCheck::FileExists(path) if path.exists() => (
            "succeeded",
            "info",
            "adopted child process completed".to_string(),
            json!({
                "output_target": output_target,
                "orphaned": true,
            }),
        ),
        SuccessCheck::FileExists(path) => (
            "failed",
            "error",
            format!(
                "adopted child process exited without artifact: {}",
                path.display()
            ),
            json!({
                "output_target": output_target,
                "orphaned": true,
            }),
        ),
        SuccessCheck::ProcessExit => match task_type_from_handle(handle) {
            Some(TaskType::StreamIngest)
                if task_runtime_mode_from_handle(handle)
                    == Some(TaskRuntimeMode::ManagedProcess) =>
            {
                (
                    "succeeded",
                    "info",
                    "adopted stream_ingest process exited; treating as completed".to_string(),
                    json!({
                        "output_target": output_target,
                        "orphaned": true,
                    }),
                )
            }
            _ => (
                "failed",
                "error",
                "adopted child process disappeared without exit status".to_string(),
                json!({
                    "output_target": output_target,
                    "orphaned": true,
                }),
            ),
        },
    }
}

fn task_type_from_handle(handle: &RuntimeHandle) -> Option<TaskType> {
    handle
        .metadata
        .get("task_type")
        .and_then(Value::as_str)
        .and_then(|value| TaskType::from_str(value).ok())
}

fn managed_file_output_kind_from_handle(handle: &RuntimeHandle) -> Option<ManagedFileOutputKind> {
    handle
        .metadata
        .get("managed_file_output_kind")
        .cloned()
        .and_then(|value| serde_json::from_value::<ManagedFileOutputKind>(value).ok())
}

fn file_artifact_metadata_from_path(path: &Path) -> Option<Value> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }

    Some(json!({
        "file_name": path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_string(),
        "file_path": path.to_string_lossy().to_string(),
        "file_size": i64::try_from(metadata.len()).unwrap_or(i64::MAX),
    }))
}

fn resolved_spec_from_handle(handle: &RuntimeHandle) -> Option<TaskSpec> {
    handle
        .metadata
        .get("resolved_spec")
        .cloned()
        .and_then(|value| serde_json::from_value::<TaskSpec>(value).ok())
}

fn task_runtime_mode_from_handle(handle: &RuntimeHandle) -> Option<TaskRuntimeMode> {
    resolved_spec_from_handle(handle).map(|spec| task_runtime_mode(&spec))
}

fn startup_probe_from_handle(handle: &RuntimeHandle) -> Option<StartupProbe> {
    handle
        .metadata
        .get("startup_probe")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
}

fn render_command_line(executable: &str, args: &[String]) -> String {
    std::iter::once(executable.to_string())
        .chain(args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ")
}

fn signal_pid(pid: i32, signal: i32) -> std::io::Result<()> {
    let rc = unsafe { libc::kill(pid, signal) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn parse_u64(value: Option<&String>) -> u64 {
    value
        .and_then(|value| value.parse().ok())
        .unwrap_or_default()
}

fn parse_f64(value: Option<&String>) -> f64 {
    value
        .and_then(|value| value.parse().ok())
        .unwrap_or_default()
}

fn parse_speed(value: Option<&String>) -> f64 {
    value
        .map(|value| value.trim_end_matches('x'))
        .and_then(|value| value.parse().ok())
        .unwrap_or_default()
}

fn parse_bitrate_kbps(value: Option<&String>) -> f64 {
    let Some(value) = value else {
        return 0.0;
    };
    let value = value.trim();
    if let Some(value) = value.strip_suffix("kbits/s") {
        return value.trim().parse().unwrap_or_default();
    }
    if let Some(value) = value.strip_suffix("bits/s") {
        let bits: f64 = value.trim().parse().unwrap_or_default();
        return bits / 1000.0;
    }
    value.parse().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn test_settings(work_root: &str) -> AgentSettings {
        AgentSettings {
            http_addr: "127.0.0.1:8081".to_string(),
            node_id: String::new(),
            node_name: "node-1".to_string(),
            core_endpoint: "http://127.0.0.1:50051".to_string(),
            cert_path: String::new(),
            key_path: String::new(),
            ca_path: String::new(),
            tls_domain_name: String::new(),
            ffmpeg_bin: "ffmpeg".to_string(),
            ffprobe_bin: "ffprobe".to_string(),
            zlm_api_base: String::new(),
            zlm_api_secret: String::new(),
            zlm_auto_close_on_no_reader_enabled: false,
            agent_stream_addr: "http://127.0.0.1:8081".to_string(),
            primary_interface_name: String::new(),
            primary_interface_ip: String::new(),
            multicast_interface_name: String::new(),
            multicast_interface_ip: String::new(),
            network_mode: "bridge".to_string(),
            acceleration_mode: "cpu".to_string(),
            labels: Vec::new(),
            max_runtime_slots: 2,
            work_root: work_root.to_string(),
        }
    }

    fn write_executable(path: &Path, body: &str) {
        fs::write(path, body).expect("script should write");
        let mut permissions = fs::metadata(path)
            .expect("script metadata should exist")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("script permissions should update");
    }

    fn create_mock_ffmpeg_binary(root: &Path) -> String {
        let path = root.join("mock-ffmpeg.sh");
        write_executable(
            &path,
            r#"#!/usr/bin/env bash
set -euo pipefail
case "${1:-}" in
  -version)
    echo "ffmpeg version mock"
    ;;
  -hwaccels)
    printf '%s\n' "Hardware acceleration methods:" "cuda"
    ;;
  -encoders)
    printf '%s\n' "Encoders:" " V....D h264_nvenc" " V....D hevc_nvenc"
    ;;
  -decoders)
    printf '%s\n' "Decoders:" " V..... h264_cuvid" " V..... hevc_cuvid"
    ;;
  *)
    exit 0
    ;;
esac
"#,
        );
        path.to_string_lossy().to_string()
    }

    fn create_mock_ffprobe_binary(
        root: &Path,
        video_codec_name: &str,
        audio_codec_name: Option<&str>,
    ) -> String {
        create_mock_ffprobe_binary_with_format(
            root,
            "mov,mp4,m4a,3gp,3g2,mj2",
            video_codec_name,
            audio_codec_name,
        )
    }

    fn create_mock_ffprobe_binary_with_format(
        root: &Path,
        format_name: &str,
        video_codec_name: &str,
        audio_codec_name: Option<&str>,
    ) -> String {
        let path = root.join("mock-ffprobe.sh");
        let audio_stream = audio_codec_name.map_or_else(String::new, |codec| {
            format!(
                r#",
    {{"codec_type":"audio","codec_name":"{codec}"}}"#
            )
        });
        let body = format!(
            r#"#!/usr/bin/env bash
set -euo pipefail
want_json=0
prev=""
for arg in "$@"; do
  if [ "$prev" = "-of" ] && [ "$arg" = "json" ]; then
    want_json=1
    break
  fi
  prev="$arg"
done
if [ "$want_json" = "1" ]; then
  cat <<'EOF'
{{"streams":[
    {{"codec_type":"video","codec_name":"{video_codec_name}"}}{audio_stream}
],"format":{{"format_name":"{format_name}"}}}}
EOF
else
  echo "{video_codec_name}"
fi
"#
        );
        write_executable(&path, &body);
        path.to_string_lossy().to_string()
    }

    #[test]
    fn registry_tracks_and_filters_snapshots() {
        let registry = LocalRuntimeRegistry::new();
        let handle = RuntimeHandle {
            runtime_id: Uuid::now_v7(),
            task_id: Uuid::now_v7(),
            attempt_no: 1,
            worker_kind: WorkerKind::Ffmpeg,
            pid: Some(1234),
            started_at: Utc::now(),
            last_progress_at: None,
            state: RuntimeState::Running,
            command_line: Some("ffmpeg -i input".to_string()),
            outputs: vec!["rtmp://output".to_string()],
            metadata: json!({ "source": "test" }),
        };
        registry.track(handle.clone());

        let snapshots = registry.snapshots(&AdoptFilter {
            task_ids: vec![handle.task_id],
            worker_kinds: vec![WorkerKind::Ffmpeg],
        });

        assert_eq!(snapshots, vec![handle]);
    }

    #[test]
    fn build_file_transcode_plan_allocates_managed_output_path() {
        let settings = test_settings("/tmp/work");
        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::FileTranscode,
            resolved_spec: json!({
                "type": "file_transcode",
                "name": "test",
                "common": {"created_by": "tester"},
                "input": {"kind": "file", "url": "input.mp4"},
                "process": {"mode": "copy_or_transcode"},
                "record": {},
                "publish": {
                    "kind": "file"
                },
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan =
            build_file_transcode_plan(&settings, &request, &spec).expect("plan should build");
        assert_eq!(plan.executable, "ffmpeg");
        assert!(plan.args.iter().any(|arg| arg == "pipe:1"));
        assert!(plan.output_target.starts_with(TRANSCODE_ARTIFACT_ROOT));
        assert!(plan.output_target.ends_with(".mp4"));
    }

    #[test]
    fn build_file_transcode_plan_copy_or_transcode_copies_hevc_aac_when_mp4_allows_it() {
        let temp_root = std::env::temp_dir().join(format!(
            "streamserver-copy-transcode-hevc-{}",
            Uuid::now_v7()
        ));
        fs::create_dir_all(&temp_root).expect("temp root should exist");

        let mut settings = test_settings("/tmp/work");
        settings.ffprobe_bin = create_mock_ffprobe_binary(&temp_root, "hevc", Some("aac"));

        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::FileTranscode,
            resolved_spec: json!({
                "type": "file_transcode",
                "name": "test-copy-hevc",
                "common": {"created_by": "tester"},
                "input": {"kind": "file", "url": "input.mp4"},
                "process": {"mode": "copy_or_transcode"},
                "record": {},
                "publish": {
                    "kind": "file",
                    "format": "mp4"
                },
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan =
            build_file_transcode_plan(&settings, &request, &spec).expect("plan should build");

        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-c:v", "copy"])
        );
        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-c:a", "copy"])
        );
        assert!(
            !plan
                .args
                .windows(2)
                .any(|window| window == ["-c:v", "libx264"])
        );
        assert!(!plan.args.windows(2).any(|window| window == ["-c:a", "aac"]));

        let _ = fs::remove_dir_all(temp_root);
    }

    #[test]
    fn build_file_transcode_plan_copy_or_transcode_copies_mpegts_aac_for_mp4_with_bsf() {
        let temp_root = std::env::temp_dir().join(format!(
            "streamserver-copy-transcode-mpegts-aac-{}",
            Uuid::now_v7()
        ));
        fs::create_dir_all(&temp_root).expect("temp root should exist");

        let mut settings = test_settings("/tmp/work");
        settings.ffprobe_bin =
            create_mock_ffprobe_binary_with_format(&temp_root, "mpegts", "h264", Some("aac"));

        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::FileTranscode,
            resolved_spec: json!({
                "type": "file_transcode",
                "name": "test-mpegts-aac-to-mp4",
                "common": {"created_by": "tester"},
                "input": {"kind": "file", "url": "input.ts"},
                "process": {"mode": "copy_or_transcode"},
                "record": {},
                "publish": {
                    "kind": "file",
                    "format": "mp4"
                },
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan =
            build_file_transcode_plan(&settings, &request, &spec).expect("plan should build");

        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-c:v", "copy"])
        );
        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-c:a", "copy"])
        );
        assert!(!plan.args.windows(2).any(|window| window == ["-c:a", "aac"]));
        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-bsf:a", "aac_adtstoasc"])
        );

        let _ = fs::remove_dir_all(temp_root);
    }

    #[test]
    fn resolve_video_families_keeps_hevc_input_probe_for_force_h264() {
        let temp_root =
            std::env::temp_dir().join(format!("streamserver-gpu-probe-{}", Uuid::now_v7()));
        fs::create_dir_all(&temp_root).expect("temp root should exist");

        let mut settings = test_settings("/tmp/work");
        settings.ffprobe_bin = create_mock_ffprobe_binary(&temp_root, "hevc", None);

        let (input_family, output_family) =
            resolve_video_families(&settings, "/tmp/input.mp4", VideoOutputPolicy::ForceH264);

        assert_eq!(input_family, VideoCodecFamily::Hevc);
        assert_eq!(output_family, VideoCodecFamily::H264);

        let _ = fs::remove_dir_all(temp_root);
    }

    #[test]
    fn probe_input_media_profile_reads_video_and_audio_codecs() {
        let temp_root =
            std::env::temp_dir().join(format!("streamserver-media-profile-{}", Uuid::now_v7()));
        fs::create_dir_all(&temp_root).expect("temp root should exist");

        let mut settings = test_settings("/tmp/work");
        settings.ffprobe_bin = create_mock_ffprobe_binary(&temp_root, "h264", Some("aac"));
        let spec: TaskSpec = serde_json::from_value(json!({
            "type": "file_transcode",
            "name": "probe-profile",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "input.mp4"},
            "process": {"mode": "copy_or_transcode"},
            "publish": {"kind": "file"},
            "record": {},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }))
        .expect("spec should parse");

        let profile = probe_input_media_profile(&settings, &spec, "/tmp/input.mp4");

        assert!(profile.has_video);
        assert_eq!(profile.video_family, VideoCodecFamily::H264);
        assert!(profile.has_audio);
        assert_eq!(profile.audio_codec_name.as_deref(), Some("aac"));
        assert_eq!(profile.source_family, InputSourceFamily::Mp4Mov);

        let _ = fs::remove_dir_all(temp_root);
    }

    #[test]
    fn maybe_add_cuda_decoder_uses_hevc_decoder_when_available() {
        let temp_root =
            std::env::temp_dir().join(format!("streamserver-gpu-decoder-{}", Uuid::now_v7()));
        fs::create_dir_all(&temp_root).expect("temp root should exist");

        let mut settings = test_settings("/tmp/work");
        settings.ffmpeg_bin = create_mock_ffmpeg_binary(&temp_root);

        let mut input_args = Vec::new();
        maybe_add_cuda_decoder(&mut input_args, &settings, VideoCodecFamily::Hevc);

        assert_eq!(
            input_args,
            vec![
                "-hwaccel".to_string(),
                "cuda".to_string(),
                "-hwaccel_output_format".to_string(),
                "cuda".to_string(),
                "-c:v".to_string(),
                "hevc_cuvid".to_string(),
            ]
        );

        let _ = fs::remove_dir_all(temp_root);
    }

    #[test]
    fn build_file_transcode_plan_rejects_publish_url_override() {
        let settings = test_settings("/tmp/work");
        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::FileTranscode,
            resolved_spec: json!({
                "type": "file_transcode",
                "name": "test",
                "common": {"created_by": "tester"},
                "input": {"kind": "file", "url": "input.mp4"},
                "process": {"mode": "copy_or_transcode"},
                "record": {},
                "publish": {
                    "kind": "file",
                    "url": "/tmp/output.mp4"
                },
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let error = build_file_transcode_plan(&settings, &request, &spec)
            .expect_err("plan should reject publish url override");
        assert!(matches!(
            error,
            ExecutorError::InvalidRequest(message)
                if message.contains("publish.url must not be provided")
        ));
    }

    #[test]
    fn build_multicast_bridge_plan_allocates_managed_file_output_path() {
        let settings = test_settings("/tmp/work");
        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::StreamBridge,
            resolved_spec: json!({
                "type": "stream_bridge",
                "name": "bridge-test",
                "common": {"created_by": "tester"},
                "input": {"kind": "rtsp", "source_mode": "live", "url": "rtsp://example.com/live"},
                "process": {"mode": "passthrough"},
                "publish": {
                    "kind": "file"
                },
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan =
            build_multicast_bridge_plan(&settings, &request, &spec).expect("plan should build");

        assert!(plan.output_target.starts_with(BRIDGE_ARTIFACT_ROOT));
        assert!(plan.output_target.ends_with(".mp4"));
        assert!(plan.args.iter().any(|arg| arg == "mp4"));
    }

    #[test]
    fn start_task_rejects_when_max_runtime_slots_are_exhausted() {
        let temp_root =
            std::env::temp_dir().join(format!("streamserver-runtime-slots-{}", Uuid::now_v7()));
        let registry = LocalRuntimeRegistry::new();
        registry.track(RuntimeHandle {
            runtime_id: Uuid::now_v7(),
            task_id: Uuid::now_v7(),
            attempt_no: 1,
            worker_kind: WorkerKind::Ffmpeg,
            pid: Some(1234),
            started_at: Utc::now(),
            last_progress_at: None,
            state: RuntimeState::Running,
            command_line: Some("ffmpeg -i input".to_string()),
            outputs: vec!["/data/zlm/www/artifacts/transcode/output.mp4".to_string()],
            metadata: json!({"task_type": "file_transcode"}),
        });

        let (priority_tx, _priority_rx) = mpsc::unbounded_channel();
        let (log_tx, _log_rx) = mpsc::channel(8);
        let mut settings = test_settings(temp_root.to_string_lossy().as_ref());
        settings.max_runtime_slots = 1;
        settings.ffmpeg_bin = "/definitely/missing-ffmpeg".to_string();
        let executor = ManagedProcessExecutor::new(
            settings,
            registry,
            RuntimeEventSink::new(priority_tx, log_tx),
        );
        let request = StartTaskRequest {
            task_id: Uuid::now_v7(),
            attempt_no: 1,
            task_type: TaskType::FileTranscode,
            resolved_spec: json!({
                "type": "file_transcode",
                "name": "test",
                "common": {"created_by": "tester"},
                "input": {"kind": "file", "url": "input.mp4"},
                "process": {"mode": "copy_or_transcode"},
                "record": {},
                "publish": {
                    "kind": "file",
                    "url": "/data/zlm/www/artifacts/transcode/output.mp4"
                },
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let error = executor
            .start_task(&request)
            .expect_err("exhausted slots should reject the task before spawn");
        assert!(matches!(
            error,
            ExecutorError::InvalidRequest(message) if message.contains("max_runtime_slots")
        ));

        let _ = fs::remove_dir_all(temp_root);
    }

    #[test]
    fn build_multicast_bridge_plan_renders_multicast_input_and_output() {
        let settings = test_settings("/tmp/work");
        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::StreamBridge,
            resolved_spec: json!({
                "type": "stream_bridge",
                "name": "bridge",
                "common": {"created_by": "tester"},
                "input": {
                    "kind": "udp_mpegts_multicast",
                    "group": "239.10.10.10",
                    "port": 5000,
                    "interface_ip": "192.168.1.10",
                    "ttl": 2,
                    "reuse": true,
                    "pkt_size": 1316
                },
                "process": {"mode": "passthrough"},
                "publish": {
                    "kind": "udp_mpegts_multicast",
                    "group": "239.20.20.20",
                    "port": 6000,
                    "interface_ip": "192.168.1.20",
                    "ttl": 4,
                    "reuse": true,
                    "pkt_size": 1316
                },
                "record": {},
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan =
            build_multicast_bridge_plan(&settings, &request, &spec).expect("plan should build");

        assert_eq!(plan.executable, "ffmpeg");
        assert_eq!(
            plan.output_target,
            "udp://239.20.20.20:6000?localaddr=192.168.1.20&reuse=1&ttl=4&pkt_size=1316"
        );
        assert!(plan.args.iter().any(|arg| arg
            == "udp://239.10.10.10:5000?localaddr=192.168.1.10&reuse=1&ttl=2&pkt_size=1316"));
        assert!(plan.args.iter().any(|arg| arg
            == "udp://239.20.20.20:6000?localaddr=192.168.1.20&reuse=1&ttl=4&pkt_size=1316"));
        let fflags_index = plan
            .args
            .iter()
            .position(|arg| arg == "-fflags")
            .expect("multicast bridge should inject ffmpeg input flags");
        let wallclock_index = plan
            .args
            .iter()
            .position(|arg| arg == "-use_wallclock_as_timestamps")
            .expect("multicast bridge should inject wallclock timestamping");
        let input_index = plan
            .args
            .iter()
            .position(|arg| arg == "-i")
            .expect("ffmpeg args should contain input marker");
        assert!(wallclock_index < input_index);
        assert!(fflags_index < input_index);
        assert_eq!(
            plan.args.get(wallclock_index + 1).map(String::as_str),
            Some("1")
        );
        assert_eq!(
            plan.args.get(fflags_index + 1).map(String::as_str),
            Some("+genpts")
        );
    }

    #[test]
    fn build_multicast_bridge_plan_stabilizes_live_mpegts_multicast_passthrough() {
        let settings = test_settings("/tmp/work");
        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::StreamBridge,
            resolved_spec: json!({
                "type": "stream_bridge",
                "name": "bridge-live-to-mcast",
                "common": {"created_by": "tester"},
                "input": {
                    "kind": "rtsp",
                    "url": "rtsp://camera.example/live"
                },
                "process": {"mode": "passthrough"},
                "publish": {
                    "kind": "udp_mpegts_multicast",
                    "group": "239.20.20.20",
                    "port": 6000,
                    "interface_ip": "192.168.1.20",
                    "ttl": 4,
                    "reuse": true,
                    "pkt_size": 1316
                },
                "record": {},
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan =
            build_multicast_bridge_plan(&settings, &request, &spec).expect("plan should build");

        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-c:v", "libx264"])
        );
        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-c:a", "copy"])
        );
        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-preset", "ultrafast"])
        );
        assert!(plan.args.windows(2).any(|window| window == ["-g", "24"]));
        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-sc_threshold", "0"])
        );
    }

    #[test]
    fn build_multicast_bridge_plan_copy_or_transcode_keeps_video_transcode_for_live_mpegts_multicast()
     {
        let temp_root = std::env::temp_dir().join(format!(
            "streamserver-bridge-mpegts-stable-{}",
            Uuid::now_v7()
        ));
        fs::create_dir_all(&temp_root).expect("temp root should exist");

        let mut settings = test_settings("/tmp/work");
        settings.ffprobe_bin = create_mock_ffprobe_binary(&temp_root, "h264", Some("aac"));

        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::StreamBridge,
            resolved_spec: json!({
                "type": "stream_bridge",
                "name": "bridge-live-to-mcast-copy-or-transcode",
                "common": {"created_by": "tester"},
                "input": {
                    "kind": "rtsp",
                    "source_mode": "live",
                    "url": "rtsp://camera.example/live"
                },
                "process": {"mode": "copy_or_transcode"},
                "publish": {
                    "kind": "udp_mpegts_multicast",
                    "group": "239.20.20.20",
                    "port": 6000,
                    "interface_ip": "192.168.1.20",
                    "ttl": 4,
                    "reuse": true,
                    "pkt_size": 1316
                },
                "record": {},
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan =
            build_multicast_bridge_plan(&settings, &request, &spec).expect("plan should build");

        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-c:v", "libx264"])
        );
        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-c:a", "copy"])
        );

        let _ = fs::remove_dir_all(temp_root);
    }

    #[test]
    fn build_multicast_bridge_plan_pushes_live_input_to_external_rtmp_without_realtime_pacing() {
        let settings = test_settings("/tmp/work");
        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::StreamBridge,
            resolved_spec: json!({
                "type": "stream_bridge",
                "name": "bridge-to-rtmp",
                "common": {"created_by": "tester"},
                "input": {
                    "kind": "rtsp",
                    "source_mode": "live",
                    "url": "rtsp://camera.example/live"
                },
                "process": {"mode": "passthrough"},
                "publish": {
                    "kind": "rtmp_push",
                    "url": "rtmp://push.example.com/live/bridge-ingest"
                },
                "record": {},
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan =
            build_multicast_bridge_plan(&settings, &request, &spec).expect("plan should build");

        assert_eq!(
            plan.output_target,
            "rtmp://push.example.com/live/bridge-ingest"
        );
        assert!(plan.startup_probe.is_none());
        assert_eq!(
            plan.args
                .windows(2)
                .find(|window| *window == ["-f", "flv"])
                .map(|_| "flv"),
            Some("flv")
        );
        assert!(!plan.args.iter().any(|arg| arg == "-re"));
        let wallclock_index = plan
            .args
            .iter()
            .position(|arg| arg == "-use_wallclock_as_timestamps")
            .expect("live bridge should stabilize timestamps");
        assert_eq!(
            plan.args.get(wallclock_index + 1).map(String::as_str),
            Some("1")
        );
        assert!(plan.args.iter().any(|arg| arg == "+genpts"));
    }

    #[test]
    fn build_multicast_bridge_plan_copy_or_transcode_copies_h264_aac_to_external_rtmp() {
        let temp_root =
            std::env::temp_dir().join(format!("streamserver-bridge-copy-rtmp-{}", Uuid::now_v7()));
        fs::create_dir_all(&temp_root).expect("temp root should exist");

        let mut settings = test_settings("/tmp/work");
        settings.ffprobe_bin = create_mock_ffprobe_binary(&temp_root, "h264", Some("aac"));

        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::StreamBridge,
            resolved_spec: json!({
                "type": "stream_bridge",
                "name": "bridge-copy-to-rtmp",
                "common": {"created_by": "tester"},
                "input": {
                    "kind": "rtsp",
                    "source_mode": "live",
                    "url": "rtsp://camera.example/live"
                },
                "process": {"mode": "copy_or_transcode"},
                "publish": {
                    "kind": "rtmp_push",
                    "url": "rtmp://push.example.com/live/bridge-copy"
                },
                "record": {},
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan =
            build_multicast_bridge_plan(&settings, &request, &spec).expect("plan should build");

        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-c:v", "copy"])
        );
        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-c:a", "copy"])
        );
        assert!(
            !plan
                .args
                .windows(2)
                .any(|window| window == ["-c:v", "libx264"])
        );

        let _ = fs::remove_dir_all(temp_root);
    }

    #[test]
    fn build_multicast_bridge_plan_copy_or_transcode_copies_hls_aac_to_external_rtmp_with_bsf() {
        let temp_root =
            std::env::temp_dir().join(format!("streamserver-bridge-hls-aac-{}", Uuid::now_v7()));
        fs::create_dir_all(&temp_root).expect("temp root should exist");

        let mut settings = test_settings("/tmp/work");
        settings.ffprobe_bin =
            create_mock_ffprobe_binary_with_format(&temp_root, "hls", "h264", Some("aac"));

        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::StreamBridge,
            resolved_spec: json!({
                "type": "stream_bridge",
                "name": "bridge-hls-to-rtmp",
                "common": {"created_by": "tester"},
                "input": {
                    "kind": "hls",
                    "source_mode": "live",
                    "url": "http://vod.example.com/archive.m3u8"
                },
                "process": {"mode": "copy_or_transcode"},
                "publish": {
                    "kind": "rtmp_push",
                    "url": "rtmp://push.example.com/live/bridge-hls"
                },
                "record": {},
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan =
            build_multicast_bridge_plan(&settings, &request, &spec).expect("plan should build");

        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-c:v", "copy"])
        );
        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-c:a", "copy"])
        );
        assert!(!plan.args.windows(2).any(|window| window == ["-c:a", "aac"]));
        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-bsf:a", "aac_adtstoasc"])
        );

        let _ = fs::remove_dir_all(temp_root);
    }

    #[test]
    fn build_multicast_bridge_plan_passthrough_copies_hls_aac_to_external_rtmp_with_bsf() {
        let temp_root = std::env::temp_dir().join(format!(
            "streamserver-bridge-passthrough-hls-aac-{}",
            Uuid::now_v7()
        ));
        fs::create_dir_all(&temp_root).expect("temp root should exist");

        let mut settings = test_settings("/tmp/work");
        settings.ffprobe_bin =
            create_mock_ffprobe_binary_with_format(&temp_root, "hls", "h264", Some("aac"));

        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::StreamBridge,
            resolved_spec: json!({
                "type": "stream_bridge",
                "name": "bridge-passthrough-hls-to-rtmp",
                "common": {"created_by": "tester"},
                "input": {
                    "kind": "hls",
                    "source_mode": "live",
                    "url": "http://vod.example.com/archive.m3u8"
                },
                "process": {"mode": "passthrough"},
                "publish": {
                    "kind": "rtmp_push",
                    "url": "rtmp://push.example.com/live/bridge-hls-pass"
                },
                "record": {},
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan =
            build_multicast_bridge_plan(&settings, &request, &spec).expect("plan should build");

        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-c:v", "copy"])
        );
        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-c:a", "copy"])
        );
        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-bsf:a", "aac_adtstoasc"])
        );

        let _ = fs::remove_dir_all(temp_root);
    }

    #[test]
    fn build_multicast_bridge_plan_pushes_vod_input_to_external_rtmp_with_realtime_pacing() {
        let settings = test_settings("/tmp/work");
        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::StreamBridge,
            resolved_spec: json!({
                "type": "stream_bridge",
                "name": "bridge-vod-to-rtmp",
                "common": {"created_by": "tester"},
                "input": {
                    "kind": "http_mp4",
                    "source_mode": "vod",
                    "url": "http://vod.example.com/archive.mp4"
                },
                "process": {"mode": "passthrough"},
                "publish": {
                    "kind": "rtmp_push",
                    "url": "rtmps://push.example.com/live/bridge-default"
                },
                "record": {},
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan =
            build_multicast_bridge_plan(&settings, &request, &spec).expect("plan should build");

        assert_eq!(
            plan.output_target,
            "rtmps://push.example.com/live/bridge-default"
        );
        assert!(plan.args.iter().any(|arg| arg == "-re"));
        assert!(plan.args.windows(2).any(|window| window == ["-f", "flv"]));
        assert!(
            !plan
                .args
                .iter()
                .any(|arg| arg == "-use_wallclock_as_timestamps")
        );
        assert!(!plan.args.iter().any(|arg| arg == "+genpts"));
    }

    #[test]
    fn resolve_interface_binding_ip_resolves_explicit_interface_name() {
        let Some(interface_name) = first_ipv4_interface_name_for_test() else {
            return;
        };

        let resolved = resolve_interface_binding_ip(
            Some(interface_name.as_str()),
            None,
            None,
            None,
            "input",
            true,
        )
        .expect("interface lookup should succeed");

        assert!(resolved.is_some());
    }

    fn first_ipv4_interface_name_for_test() -> Option<String> {
        unsafe {
            let mut addrs: *mut libc::ifaddrs = ptr::null_mut();
            if libc::getifaddrs(&mut addrs) != 0 || addrs.is_null() {
                return None;
            }

            let mut current = addrs;
            let mut resolved = None;
            while !current.is_null() {
                let ifa = &*current;
                if !ifa.ifa_name.is_null()
                    && !ifa.ifa_addr.is_null()
                    && (*ifa.ifa_addr).sa_family as i32 == libc::AF_INET
                {
                    resolved = Some(CStr::from_ptr(ifa.ifa_name).to_string_lossy().to_string());
                    break;
                }
                current = ifa.ifa_next;
            }
            libc::freeifaddrs(addrs);
            resolved
        }
    }

    #[test]
    fn build_input_url_resolves_relative_file_input_under_work_root() {
        let settings = test_settings("/tmp/work");
        let input = InputSpec {
            kind: Some(InputKind::File),
            url: Some("vod/demo.ts".to_string()),
            ..InputSpec::default()
        };

        let input_url = build_input_url(&settings, &input).expect("input url should resolve");

        assert_eq!(input_url, "/tmp/work/vod/demo.ts");
    }

    #[test]
    fn build_input_url_strips_leading_slash_for_file_input() {
        let settings = test_settings("/tmp/work");
        let input = InputSpec {
            kind: Some(InputKind::File),
            url: Some("/demo.mp4".to_string()),
            ..InputSpec::default()
        };

        let input_url = build_input_url(&settings, &input).expect("input url should resolve");

        assert_eq!(input_url, "/tmp/work/demo.mp4");
    }

    #[test]
    fn build_input_url_rejects_parent_dir_in_file_input() {
        let settings = test_settings("/tmp/work");
        let input = InputSpec {
            kind: Some(InputKind::File),
            url: Some("../demo.mp4".to_string()),
            ..InputSpec::default()
        };

        let error = build_input_url(&settings, &input).expect_err("input url should fail");

        assert!(matches!(
            error,
            ExecutorError::InvalidRequest(message)
                if message.contains("must not contain '..' segments")
        ));
    }

    #[test]
    fn build_input_url_keeps_ftp_url_unchanged() {
        let settings = test_settings("/tmp/work");
        let input = InputSpec {
            kind: Some(InputKind::Ftp),
            url: Some("ftp://vod.example.com/archive/demo.mp4".to_string()),
            ..InputSpec::default()
        };

        let input_url = build_input_url(&settings, &input).expect("input url should resolve");

        assert_eq!(input_url, "ftp://vod.example.com/archive/demo.mp4");
    }

    #[test]
    fn build_file_to_live_plan_uses_realtime_tee_output() {
        let settings = test_settings("/tmp/work");
        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::StreamIngest,
            resolved_spec: json!({
                "type": "stream_ingest",
                "name": "file-live",
                "common": {"created_by": "tester"},
                "input": {"kind": "file", "url": "input.mp4"},
                "stream": {"app": "live", "name": "stream"},
                "process": {"mode": "copy_or_transcode"},
                "record": {
                    "enabled": true,
                    "format": "mp4"
                },
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan = build_file_to_live_plan(&settings, &request, &spec).expect("plan should build");

        assert!(plan.args.iter().any(|arg| arg == "-re"));
        assert!(plan.args.iter().any(|arg| arg == "tee"));
        assert_eq!(plan.output_target, "rtmp://127.0.0.1/live/stream");
        assert_eq!(
            plan.outputs,
            vec![
                "rtmp://127.0.0.1/live/stream".to_string(),
                "/tmp/work/00000000-0000-0000-0000-000000000000/attempt-1/record.mp4".to_string()
            ]
        );
    }

    #[test]
    fn build_file_to_live_plan_loops_vod_input_when_enabled() {
        let settings = test_settings("/tmp/work");
        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::StreamIngest,
            resolved_spec: json!({
                "type": "stream_ingest",
                "name": "file-live-loop",
                "common": {"created_by": "tester"},
                "input": {
                    "kind": "file",
                    "source_mode": "vod",
                    "loop_enabled": true,
                    "url": "input.mp4"
                },
                "stream": {"app": "live", "name": "stream"},
                "process": {"mode": "copy_or_transcode"},
                "record": {
                    "enabled": true,
                    "format": "mp4",
                    "duration_sec": 300
                },
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan = build_file_to_live_plan(&settings, &request, &spec).expect("plan should build");

        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-stream_loop", "-1"])
        );
        assert!(plan.args.iter().any(|arg| arg == "-re"));
        assert!(plan.args.windows(2).any(|window| window == ["-t", "300"]));
    }

    #[test]
    fn build_stream_ingest_fast_record_plan_disables_realtime_pacing_and_stream_probe() {
        let settings = test_settings("/tmp/work");
        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::StreamIngest,
            resolved_spec: json!({
                "type": "stream_ingest",
                "name": "vod-fast-record",
                "common": {"created_by": "tester"},
                "input": {
                    "kind": "http_mp4",
                    "source_mode": "vod",
                    "url": "http://vod.example.com/archive.mp4"
                },
                "stream": {"app": "live", "name": "archive-fast"},
                "expose": {
                    "enable_rtsp": false,
                    "enable_rtmp": false,
                    "enable_http_ts": false,
                    "enable_http_fmp4": false,
                    "enable_hls": false
                },
                "process": {"mode": "copy_or_transcode"},
                "record": {
                    "enabled": true,
                    "format": "mp4",
                    "duration_sec": 300
                },
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan = build_stream_ingest_plan(&settings, &request, &spec).expect("plan should build");

        assert!(!plan.args.iter().any(|arg| arg == "-re"));
        assert!(plan.startup_probe.is_none());
        assert_eq!(plan.recording, None);
        assert_eq!(
            plan.managed_file_output_kind,
            Some(ManagedFileOutputKind::StreamIngestRecord)
        );
        assert!(
            plan.output_target
                .starts_with(STREAM_INGEST_RECORD_ARTIFACT_ROOT)
        );
        assert!(plan.output_target.ends_with(".mp4"));
        assert!(plan.args.windows(2).any(|window| window == ["-t", "300"]));
    }

    #[test]
    fn build_stream_ingest_fast_record_plan_copies_mpegts_aac_for_mp4_output_with_bsf() {
        let temp_root = std::env::temp_dir().join(format!(
            "streamserver-fast-record-mpegts-aac-{}",
            Uuid::now_v7()
        ));
        fs::create_dir_all(&temp_root).expect("temp root should exist");

        let mut settings = test_settings("/tmp/work");
        settings.ffprobe_bin =
            create_mock_ffprobe_binary_with_format(&temp_root, "mpegts", "h264", Some("aac"));

        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::StreamIngest,
            resolved_spec: json!({
                "type": "stream_ingest",
                "name": "vod-fast-record-ts",
                "common": {"created_by": "tester"},
                "input": {
                    "kind": "file",
                    "source_mode": "vod",
                    "url": "archive.ts"
                },
                "stream": {"app": "live", "name": "archive-fast-ts"},
                "expose": {
                    "enable_rtsp": false,
                    "enable_rtmp": false,
                    "enable_http_ts": false,
                    "enable_http_fmp4": false,
                    "enable_hls": false
                },
                "process": {"mode": "copy_or_transcode"},
                "record": {
                    "enabled": true,
                    "format": "mp4"
                },
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan = build_stream_ingest_plan(&settings, &request, &spec).expect("plan should build");

        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-c:v", "copy"])
        );
        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-c:a", "copy"])
        );
        assert!(!plan.args.windows(2).any(|window| window == ["-c:a", "aac"]));
        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-bsf:a", "aac_adtstoasc"])
        );

        let _ = fs::remove_dir_all(temp_root);
    }

    #[test]
    fn build_stream_ingest_fast_record_plan_copies_mpegts_h264_aac_for_hls_output() {
        let temp_root = std::env::temp_dir().join(format!(
            "streamserver-fast-record-hls-copy-{}",
            Uuid::now_v7()
        ));
        fs::create_dir_all(&temp_root).expect("temp root should exist");

        let mut settings = test_settings("/tmp/work");
        settings.ffprobe_bin =
            create_mock_ffprobe_binary_with_format(&temp_root, "mpegts", "h264", Some("aac"));

        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::StreamIngest,
            resolved_spec: json!({
                "type": "stream_ingest",
                "name": "vod-fast-record-hls",
                "common": {"created_by": "tester"},
                "input": {
                    "kind": "file",
                    "source_mode": "vod",
                    "url": "archive.ts"
                },
                "stream": {"app": "live", "name": "archive-fast-hls"},
                "expose": {
                    "enable_rtsp": false,
                    "enable_rtmp": false,
                    "enable_http_ts": false,
                    "enable_http_fmp4": false,
                    "enable_hls": false
                },
                "process": {"mode": "copy_or_transcode"},
                "record": {
                    "enabled": true,
                    "format": "hls",
                    "segment_sec": 6
                },
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan = build_stream_ingest_plan(&settings, &request, &spec).expect("plan should build");

        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-c:v", "copy"])
        );
        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-c:a", "copy"])
        );
        assert!(!plan.args.windows(2).any(|window| window == ["-c:a", "aac"]));
        assert!(plan.args.windows(2).any(|window| window == ["-f", "hls"]));

        let _ = fs::remove_dir_all(temp_root);
    }

    #[test]
    fn build_stream_ingest_fast_record_plan_copies_hls_h264_aac_for_hls_output() {
        let temp_root = std::env::temp_dir().join(format!(
            "streamserver-fast-record-hls-source-copy-{}",
            Uuid::now_v7()
        ));
        fs::create_dir_all(&temp_root).expect("temp root should exist");

        let mut settings = test_settings("/tmp/work");
        settings.ffprobe_bin =
            create_mock_ffprobe_binary_with_format(&temp_root, "hls", "h264", Some("aac"));

        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::StreamIngest,
            resolved_spec: json!({
                "type": "stream_ingest",
                "name": "vod-fast-record-hls-source",
                "common": {"created_by": "tester"},
                "input": {
                    "kind": "hls",
                    "source_mode": "vod",
                    "url": "http://vod.example.com/archive.m3u8"
                },
                "stream": {"app": "live", "name": "archive-fast-hls-source"},
                "expose": {
                    "enable_rtsp": false,
                    "enable_rtmp": false,
                    "enable_http_ts": false,
                    "enable_http_fmp4": false,
                    "enable_hls": false
                },
                "process": {"mode": "copy_or_transcode"},
                "record": {
                    "enabled": true,
                    "format": "hls",
                    "segment_sec": 6
                },
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan = build_stream_ingest_plan(&settings, &request, &spec).expect("plan should build");

        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-c:v", "copy"])
        );
        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-c:a", "copy"])
        );
        assert!(!plan.args.windows(2).any(|window| window == ["-c:a", "aac"]));

        let _ = fs::remove_dir_all(temp_root);
    }

    #[test]
    fn build_stream_ingest_fast_record_plan_generates_mp4_and_hls_outputs_for_both_format() {
        let settings = test_settings("/tmp/work");
        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::StreamIngest,
            resolved_spec: json!({
                "type": "stream_ingest",
                "name": "vod-fast-record-both",
                "common": {"created_by": "tester"},
                "input": {
                    "kind": "file",
                    "source_mode": "vod",
                    "url": "archive.mp4"
                },
                "stream": {"app": "live", "name": "archive-both"},
                "expose": {
                    "enable_rtsp": false,
                    "enable_rtmp": false,
                    "enable_http_ts": false,
                    "enable_http_fmp4": false,
                    "enable_hls": false
                },
                "process": {"mode": "copy_or_transcode"},
                "record": {
                    "enabled": true,
                    "format": "both",
                    "segment_sec": 8
                },
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan = build_stream_ingest_plan(&settings, &request, &spec).expect("plan should build");

        assert_eq!(plan.outputs.len(), 2);
        assert!(plan.outputs.iter().any(|output| output.ends_with(".mp4")));
        assert!(plan.outputs.iter().any(|output| output.ends_with(".m3u8")));
        assert!(plan.args.windows(2).any(|window| window == ["-f", "mp4"]));
        assert!(plan.args.windows(2).any(|window| window == ["-f", "hls"]));
        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-hls_time", "8"])
        );
    }

    #[test]
    fn build_stream_ingest_fast_record_plan_copies_mpegts_aac_for_both_output_with_mp4_bsf() {
        let temp_root = std::env::temp_dir().join(format!(
            "streamserver-fast-record-both-mpegts-aac-{}",
            Uuid::now_v7()
        ));
        fs::create_dir_all(&temp_root).expect("temp root should exist");

        let mut settings = test_settings("/tmp/work");
        settings.ffprobe_bin =
            create_mock_ffprobe_binary_with_format(&temp_root, "mpegts", "h264", Some("aac"));

        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::StreamIngest,
            resolved_spec: json!({
                "type": "stream_ingest",
                "name": "vod-fast-record-both-mpegts",
                "common": {"created_by": "tester"},
                "input": {
                    "kind": "file",
                    "source_mode": "vod",
                    "url": "archive.ts"
                },
                "stream": {"app": "live", "name": "archive-both-mpegts"},
                "expose": {
                    "enable_rtsp": false,
                    "enable_rtmp": false,
                    "enable_http_ts": false,
                    "enable_http_fmp4": false,
                    "enable_hls": false
                },
                "process": {"mode": "copy_or_transcode"},
                "record": {
                    "enabled": true,
                    "format": "both",
                    "segment_sec": 8
                },
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan = build_stream_ingest_plan(&settings, &request, &spec).expect("plan should build");

        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-c:v", "copy"])
        );
        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-c:a", "copy"])
        );
        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-bsf:a", "aac_adtstoasc"])
        );
        assert_eq!(
            plan.args
                .windows(2)
                .filter(|window| *window == ["-bsf:a", "aac_adtstoasc"])
                .count(),
            1
        );

        let _ = fs::remove_dir_all(temp_root);
    }

    #[test]
    fn build_file_to_live_plan_copy_or_transcode_copies_mpegts_aac_into_internal_rtmp_with_bsf() {
        let temp_root =
            std::env::temp_dir().join(format!("streamserver-file-live-mpegts-{}", Uuid::now_v7()));
        fs::create_dir_all(&temp_root).expect("temp root should exist");

        let mut settings = test_settings("/tmp/work");
        settings.ffprobe_bin =
            create_mock_ffprobe_binary_with_format(&temp_root, "mpegts", "h264", Some("aac"));

        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::StreamIngest,
            resolved_spec: json!({
                "type": "stream_ingest",
                "name": "file-live-copy",
                "common": {"created_by": "tester"},
                "input": {"kind": "file", "url": "input.ts"},
                "stream": {"app": "live", "name": "stream"},
                "process": {"mode": "copy_or_transcode"},
                "record": {},
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan = build_file_to_live_plan(&settings, &request, &spec).expect("plan should build");

        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-c:v", "copy"])
        );
        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-c:a", "copy"])
        );
        assert!(
            !plan
                .args
                .windows(2)
                .any(|window| window == ["-c:v", "libx264"])
        );
        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-bsf:a", "aac_adtstoasc"])
        );

        let _ = fs::remove_dir_all(temp_root);
    }

    #[test]
    fn build_file_to_live_plan_copy_or_transcode_copies_mp4_h264_aac_into_internal_rtmp() {
        let temp_root =
            std::env::temp_dir().join(format!("streamserver-file-live-mp4-{}", Uuid::now_v7()));
        fs::create_dir_all(&temp_root).expect("temp root should exist");

        let mut settings = test_settings("/tmp/work");
        settings.ffprobe_bin = create_mock_ffprobe_binary(&temp_root, "h264", Some("aac"));

        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::StreamIngest,
            resolved_spec: json!({
                "type": "stream_ingest",
                "name": "file-live-copy-safe",
                "common": {"created_by": "tester"},
                "input": {"kind": "file", "url": "input.mp4"},
                "stream": {"app": "live", "name": "stream"},
                "process": {"mode": "copy_or_transcode"},
                "record": {},
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan = build_file_to_live_plan(&settings, &request, &spec).expect("plan should build");

        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-c:v", "copy"])
        );
        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-c:a", "copy"])
        );
        assert!(!plan.args.windows(2).any(|window| window == ["-c:a", "aac"]));

        let _ = fs::remove_dir_all(temp_root);
    }

    #[test]
    fn build_file_to_live_plan_copy_or_transcode_only_transcodes_non_aac_audio() {
        let temp_root = std::env::temp_dir().join(format!(
            "streamserver-file-live-audio-transcode-{}",
            Uuid::now_v7()
        ));
        fs::create_dir_all(&temp_root).expect("temp root should exist");

        let mut settings = test_settings("/tmp/work");
        settings.ffprobe_bin =
            create_mock_ffprobe_binary_with_format(&temp_root, "mpegts", "h264", Some("mp3"));

        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::StreamIngest,
            resolved_spec: json!({
                "type": "stream_ingest",
                "name": "file-live-audio-transcode",
                "common": {"created_by": "tester"},
                "input": {"kind": "file", "url": "input.ts"},
                "stream": {"app": "live", "name": "stream"},
                "process": {"mode": "copy_or_transcode"},
                "record": {},
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan = build_file_to_live_plan(&settings, &request, &spec).expect("plan should build");

        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-c:v", "copy"])
        );
        assert!(plan.args.windows(2).any(|window| window == ["-c:a", "aac"]));

        let _ = fs::remove_dir_all(temp_root);
    }

    #[test]
    fn build_file_to_live_plan_uses_bsf_when_recording_mp4_from_mpegts_aac() {
        let temp_root = std::env::temp_dir().join(format!(
            "streamserver-file-live-recording-mpegts-{}",
            Uuid::now_v7()
        ));
        fs::create_dir_all(&temp_root).expect("temp root should exist");

        let mut settings = test_settings("/tmp/work");
        settings.ffprobe_bin =
            create_mock_ffprobe_binary_with_format(&temp_root, "mpegts", "h264", Some("aac"));

        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::StreamIngest,
            resolved_spec: json!({
                "type": "stream_ingest",
                "name": "file-live-recording",
                "common": {"created_by": "tester"},
                "input": {"kind": "file", "url": "input.ts", "source_mode": "vod", "loop_enabled": true},
                "stream": {"app": "live", "name": "stream"},
                "process": {"mode": "copy_or_transcode", "bitrate": 8000},
                "record": {"enabled": true, "format": "mp4"},
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan = build_file_to_live_plan(&settings, &request, &spec).expect("plan should build");

        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-c:v", "libx264"])
        );
        assert!(
            plan.args
                .windows(2)
                .any(|window| window == ["-c:a", "copy"])
        );
        assert!(plan.args.iter().any(|arg| arg == "-f"));
        assert!(plan.args.iter().any(|arg| arg == "tee"));
        assert!(
            plan.args
                .iter()
                .any(|arg| arg.contains("bsfs/a=aac_adtstoasc"))
        );

        let _ = fs::remove_dir_all(temp_root);
    }

    #[test]
    fn build_file_to_live_plan_accepts_http_mp4_and_duration_limit() {
        let settings = test_settings("/tmp/work");
        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::StreamIngest,
            resolved_spec: json!({
                "type": "stream_ingest",
                "name": "file-live-http",
                "common": {"created_by": "tester"},
                "input": {"kind": "http_mp4", "url": "http://vod.example.com/archive.mp4"},
                "stream": {"app": "live", "name": "stream"},
                "process": {"mode": "copy_or_transcode"},
                "record": {
                    "enabled": true,
                    "format": "mp4",
                    "duration_sec": 300
                },
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan = build_file_to_live_plan(&settings, &request, &spec).expect("plan should build");

        assert!(plan.args.iter().any(|arg| arg == "-re"));
        assert!(plan.args.windows(2).any(|window| window == ["-t", "300"]));
        assert!(
            plan.args
                .iter()
                .any(|arg| arg == "http://vod.example.com/archive.mp4")
        );
    }

    #[test]
    fn build_file_to_live_plan_uses_flv_for_internal_rtmp_publish() {
        let settings = test_settings("/tmp/work");
        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::StreamIngest,
            resolved_spec: json!({
                "type": "stream_ingest",
                "name": "file-live-flv",
                "common": {"created_by": "tester"},
                "input": {
                    "kind": "http_mp4",
                    "source_mode": "vod",
                    "url": "http://vod.example.com/archive.mp4"
                },
                "stream": {
                    "app": "live",
                    "name": "internal-flv-check"
                },
                "expose": {
                    "enable_rtmp": true
                },
                "record": {},
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan = build_file_to_live_plan(&settings, &request, &spec).expect("plan should build");

        assert_eq!(
            plan.output_target,
            "rtmp://127.0.0.1/live/internal-flv-check"
        );
        assert!(plan.args.windows(2).any(|window| window == ["-f", "flv"]));
        assert!(!plan.args.windows(2).any(|window| window == ["-f", "rtmp"]));
    }

    #[test]
    fn build_live_relay_plan_allocates_stable_stream_binding() {
        let settings = test_settings("/tmp/work");
        let task_id = Uuid::now_v7();
        let request = StartTaskRequest {
            task_id,
            attempt_no: 1,
            task_type: TaskType::StreamIngest,
            resolved_spec: json!({
                "type": "stream_ingest",
                "name": "relay",
                "common": {"created_by": "tester"},
                "input": {"kind": "rtsp", "url": "rtsp://camera.example/live"},
                "expose": {
                    "enable_rtsp": true,
                    "enable_rtmp": false,
                    "enable_http_ts": false,
                    "enable_http_fmp4": false,
                    "enable_hls": false
                },
                "record": {},
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan = build_live_relay_plan(&settings, &request, &spec).expect("plan should build");

        assert_eq!(plan.startup_probe.schema.as_deref(), Some("rtsp"));
        assert_eq!(plan.startup_probe.vhost, "__defaultVhost__");
        assert_eq!(plan.startup_probe.app, "live");
        assert_eq!(plan.startup_probe.stream, task_id.to_string());
        assert!(
            plan.command_line
                .contains("zlm addStreamProxy --url rtsp://camera.example/live")
        );
    }

    #[test]
    fn build_live_relay_api_params_map_publish_flags() {
        let mut settings = test_settings("/tmp/work");
        settings.zlm_auto_close_on_no_reader_enabled = true;
        let spec = serde_json::from_value::<TaskSpec>(json!({
            "type": "stream_ingest",
            "name": "relay",
            "common": {"created_by": "tester"},
            "input": {"kind": "rtsp", "url": "rtsp://camera.example/live", "probe_timeout_ms": 7000},
            "expose": {
                "enable_rtsp": false,
                "enable_rtmp": true,
                "enable_http_ts": false,
                "enable_http_fmp4": true,
                "enable_hls": false,
                "stop_on_no_reader": true
            },
            "record": {"enabled": true, "format": "both"},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }))
        .expect("task spec should parse");
        let startup_probe = StartupProbe {
            schema: Some("rtmp".to_string()),
            vhost: "__defaultVhost__".to_string(),
            app: "relay".to_string(),
            stream: "stream-1".to_string(),
        };

        let params = build_live_relay_api_params(
            &settings,
            &spec,
            &startup_probe,
            "rtsp://camera.example/live",
        )
        .into_iter()
        .collect::<HashMap<_, _>>();

        assert_eq!(params.get("enable_rtsp").map(String::as_str), Some("0"));
        assert_eq!(params.get("enable_rtmp").map(String::as_str), Some("1"));
        assert_eq!(params.get("enable_ts").map(String::as_str), Some("0"));
        assert_eq!(params.get("enable_fmp4").map(String::as_str), Some("1"));
        assert_eq!(params.get("enable_hls").map(String::as_str), Some("1"));
        assert_eq!(params.get("enable_mp4").map(String::as_str), Some("1"));
        assert_eq!(params.get("auto_close").map(String::as_str), Some("1"));
        assert_eq!(params.get("timeout_sec").map(String::as_str), Some("7"));
    }

    #[test]
    fn build_live_relay_plan_uses_managed_recording_root_when_enabled() {
        let settings = test_settings("/tmp/work");
        let request = StartTaskRequest {
            task_id: Uuid::now_v7(),
            attempt_no: 1,
            task_type: TaskType::StreamIngest,
            resolved_spec: json!({
                "type": "stream_ingest",
                "name": "relay-record",
                "common": {"created_by": "tester"},
                "input": {"kind": "rtsp", "url": "rtsp://camera.example/live"},
                "publish": {},
                "record": {
                    "enabled": true,
                    "format": "mp4",
                    "segment_sec": 120
                },
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan = build_live_relay_plan(&settings, &request, &spec).expect("plan should build");
        let recording = plan.recording.expect("recording should be present");

        assert_eq!(recording.formats, vec![ZlmRecordKind::Mp4]);
        assert_eq!(recording.root_path, "/data/zlm/www/record");
        assert_eq!(recording.duration_sec, None);
        assert_eq!(recording.segment_sec, Some(120));
        assert!(
            plan.outputs
                .iter()
                .any(|output| output == "/data/zlm/www/record")
        );
    }

    #[test]
    fn recording_duration_reached_uses_recording_start_time() {
        let started_at = Utc::now();
        let recording = LiveRelayRecording {
            formats: vec![ZlmRecordKind::Mp4],
            root_path: "/var/media/archive".to_string(),
            duration_sec: Some(300),
            segment_sec: None,
            as_player: false,
            recording_started_at: Some(started_at),
            auto_stop_requested: false,
            completion_reason: None,
            started: true,
            failed: false,
        };

        assert!(!recording_duration_reached(
            &recording,
            started_at + chrono::Duration::seconds(299)
        ));
        assert!(recording_duration_reached(
            &recording,
            started_at + chrono::Duration::seconds(300)
        ));
    }

    #[test]
    fn classify_adopted_exit_treats_record_duration_reached_as_success() {
        let handle = RuntimeHandle {
            runtime_id: Uuid::now_v7(),
            task_id: Uuid::now_v7(),
            attempt_no: 1,
            worker_kind: WorkerKind::Ffmpeg,
            pid: Some(1234),
            started_at: Utc::now(),
            last_progress_at: None,
            state: RuntimeState::Exited,
            command_line: Some("ffmpeg -re -i input".to_string()),
            outputs: vec!["rtmp://127.0.0.1/live/stream".to_string()],
            metadata: json!({
                "task_type": "file_to_live",
                "completion_reason": "record_duration_reached",
            }),
        };

        let (event_type, _, _, payload) =
            classify_adopted_exit(&handle, &SuccessCheck::ProcessExit, true);
        assert_eq!(event_type, "succeeded");
        assert_eq!(payload["reason"], json!("record_duration_reached"));
    }

    #[test]
    fn build_live_relay_plan_ignores_record_save_path_override() {
        let settings = test_settings("/tmp/work");
        let request = StartTaskRequest {
            task_id: Uuid::now_v7(),
            attempt_no: 1,
            task_type: TaskType::StreamIngest,
            resolved_spec: json!({
                "type": "stream_ingest",
                "name": "relay-record-custom-path",
                "common": {"created_by": "tester"},
                "input": {"kind": "rtsp", "url": "rtsp://camera.example/live"},
                "publish": {},
                "record": {
                    "enabled": true,
                    "format": "hls",
                    "save_path": "/var/media/archive/custom"
                },
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan = build_live_relay_plan(&settings, &request, &spec).expect("plan should build");
        let recording = plan.recording.expect("recording should be present");

        assert_eq!(recording.root_path, "/data/zlm/www/record");
    }

    #[test]
    fn build_record_api_params_uses_expected_zlm_shape() {
        let binding = StreamBinding {
            schema: Some("rtmp".to_string()),
            vhost: "__defaultVhost__".to_string(),
            app: "relay".to_string(),
            stream: "stream-1".to_string(),
        };
        let recording = LiveRelayRecording {
            formats: vec![ZlmRecordKind::Mp4],
            root_path: "/var/media/archive".to_string(),
            duration_sec: None,
            segment_sec: Some(90),
            as_player: false,
            recording_started_at: None,
            auto_stop_requested: false,
            completion_reason: None,
            started: false,
            failed: false,
        };

        let params = build_record_api_params(&binding, &recording, &ZlmRecordKind::Mp4)
            .into_iter()
            .collect::<HashMap<_, _>>();

        assert_eq!(params.get("type").map(String::as_str), Some("1"));
        assert_eq!(
            params.get("customized_path").map(String::as_str),
            Some("/var/media/archive")
        );
        assert_eq!(params.get("max_second").map(String::as_str), Some("90"));
        assert_eq!(params.get("schema").map(String::as_str), Some("rtmp"));
    }

    #[test]
    fn build_rtp_receive_plan_uses_attempt_scoped_stream_id() {
        let settings = test_settings("/tmp/work");
        let task_id = Uuid::now_v7();
        let request = StartTaskRequest {
            task_id,
            attempt_no: 3,
            task_type: TaskType::StreamIngest,
            resolved_spec: json!({
                "type": "stream_ingest",
                "name": "gb28181",
                "common": {"created_by": "tester"},
                "input": {"kind": "gb_rtp", "port": 0},
                "publish": {"enable_rtsp": true, "enable_rtmp": false},
                "record": {},
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan = build_rtp_receive_plan(&settings, &request, &spec).expect("plan should build");
        let params = build_open_rtp_server_params(&plan)
            .into_iter()
            .collect::<HashMap<_, _>>();
        let expected_stream_id = format!("{task_id}-3");

        assert_eq!(
            plan.command_line,
            format!(
                "zlm openRtpServer --port 0 --tcp_mode 0 --stream_id {}-3",
                task_id
            )
        );
        assert_eq!(params.get("port").map(String::as_str), Some("0"));
        assert_eq!(params.get("tcp_mode").map(String::as_str), Some("0"));
        assert_eq!(
            params.get("stream_id").map(String::as_str),
            Some(expected_stream_id.as_str())
        );
    }

    #[test]
    fn build_rtp_receive_plan_maps_reuse_port_and_ssrc() {
        let settings = test_settings("/tmp/work");
        let request = StartTaskRequest {
            task_id: Uuid::now_v7(),
            attempt_no: 1,
            task_type: TaskType::StreamIngest,
            resolved_spec: json!({
                "type": "stream_ingest",
                "name": "gb28181",
                "common": {"created_by": "tester"},
                "input": {
                    "kind": "gb_rtp",
                    "port": 30000,
                    "tcp_mode": 1,
                    "reuse": true,
                    "ssrc": 123456
                },
                "publish": {"enable_rtsp": true},
                "record": {},
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease".to_string(),
            trace_context: None,
        };

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan = build_rtp_receive_plan(&settings, &request, &spec).expect("plan should build");
        let params = build_open_rtp_server_params(&plan)
            .into_iter()
            .collect::<HashMap<_, _>>();

        assert!(plan.command_line.contains("--re_use_port 1"));
        assert!(plan.command_line.contains("--ssrc 123456"));
        assert_eq!(params.get("re_use_port").map(String::as_str), Some("1"));
        assert_eq!(params.get("ssrc").map(String::as_str), Some("123456"));
    }

    #[test]
    fn zlm_stream_online_in_body_matches_vhost_and_schema() {
        let body = json!({
            "code": 0,
            "data": [
                {
                    "schema": "rtmp",
                    "vhost": "__defaultVhost__",
                    "app": "relay",
                    "stream": "stream-1"
                }
            ]
        });
        let target = StartupProbe {
            schema: Some("rtmp".to_string()),
            vhost: "__defaultVhost__".to_string(),
            app: "relay".to_string(),
            stream: "stream-1".to_string(),
        };

        assert!(zlm_stream_online_in_body(&body, &target));
        assert!(!zlm_stream_online_in_body(
            &body,
            &StartupProbe {
                schema: Some("rtsp".to_string()),
                ..target
            }
        ));
    }

    #[test]
    fn failed_live_relay_recording_is_not_retried() {
        assert!(!should_start_live_relay_recording(&LiveRelayRecording {
            formats: vec![ZlmRecordKind::Mp4],
            root_path: "/var/media/archive".to_string(),
            duration_sec: None,
            segment_sec: None,
            as_player: false,
            recording_started_at: None,
            auto_stop_requested: false,
            completion_reason: None,
            started: false,
            failed: true,
        }));
    }

    #[test]
    fn scan_persisted_runtimes_reads_runtime_state_files() {
        let temp_root =
            std::env::temp_dir().join(format!("streamserver-runtime-{}", Uuid::now_v7()));
        let work_dir = temp_root.join("task").join("attempt-1");
        let handle = RuntimeHandle {
            runtime_id: Uuid::now_v7(),
            task_id: Uuid::now_v7(),
            attempt_no: 1,
            worker_kind: WorkerKind::Ffmpeg,
            pid: Some(std::process::id() as i32),
            started_at: Utc::now(),
            last_progress_at: None,
            state: RuntimeState::Running,
            command_line: Some("ffmpeg -re -i input".to_string()),
            outputs: vec!["rtmp://127.0.0.1/live/stream".to_string()],
            metadata: json!({"task_type": "file_to_live"}),
        };

        persist_runtime_state(&work_dir, &handle, &SuccessCheck::ProcessExit)
            .expect("runtime state should persist");
        let scanned = scan_persisted_runtimes(temp_root.to_string_lossy().as_ref());

        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0].handle.task_id, handle.task_id);
        assert_eq!(scanned[0].success_check, SuccessCheck::ProcessExit);

        let _ = fs::remove_dir_all(temp_root);
    }

    #[tokio::test]
    async fn adopt_orphans_tracks_persisted_runtime() {
        let temp_root =
            std::env::temp_dir().join(format!("streamserver-adopt-runtime-{}", Uuid::now_v7()));
        let work_dir = temp_root.join("task").join("attempt-1");
        let handle = RuntimeHandle {
            runtime_id: Uuid::now_v7(),
            task_id: Uuid::now_v7(),
            attempt_no: 1,
            worker_kind: WorkerKind::Ffmpeg,
            pid: Some(std::process::id() as i32),
            started_at: Utc::now(),
            last_progress_at: None,
            state: RuntimeState::Running,
            command_line: Some("ffmpeg -re -i input".to_string()),
            outputs: vec!["rtmp://127.0.0.1/live/stream".to_string()],
            metadata: json!({"task_type": "file_to_live"}),
        };

        persist_runtime_state(&work_dir, &handle, &SuccessCheck::ProcessExit)
            .expect("runtime state should persist");

        let registry = LocalRuntimeRegistry::new();
        let (priority_tx, _priority_rx) = mpsc::unbounded_channel();
        let (log_tx, _log_rx) = mpsc::channel(8);
        let executor = ManagedProcessExecutor::new(
            test_settings(temp_root.to_string_lossy().as_ref()),
            registry.clone(),
            RuntimeEventSink::new(priority_tx, log_tx),
        );

        let adopted = executor.adopt_orphans(&AdoptFilter {
            task_ids: vec![handle.task_id],
            worker_kinds: vec![WorkerKind::Ffmpeg],
        });

        assert_eq!(adopted.len(), 1);
        assert_eq!(adopted[0].state, RuntimeState::Orphaned);
        assert!(
            registry
                .find_by_task_attempt(handle.task_id, handle.attempt_no)
                .is_some()
        );

        let _ = fs::remove_dir_all(temp_root);
    }

    #[tokio::test]
    async fn runtime_event_sink_summarizes_dropped_log_lines() {
        let (priority_tx, _priority_rx) = mpsc::unbounded_channel();
        let (log_tx, mut log_rx) = mpsc::channel(1);
        let sink = RuntimeEventSink::new(priority_tx, log_tx);
        let task_id = Uuid::now_v7();

        assert!(
            sink.send(RuntimeNotification::TaskLogBatch(RuntimeTaskLogBatch {
                task_id,
                attempt_no: 1,
                stream: "stderr".to_string(),
                lines: vec!["first".to_string()],
                source_line_count: 1,
            }))
            .is_ok()
        );
        assert!(
            sink.send(RuntimeNotification::TaskLogBatch(RuntimeTaskLogBatch {
                task_id,
                attempt_no: 1,
                stream: "stderr".to_string(),
                lines: vec!["dropped".to_string()],
                source_line_count: 3,
            }))
            .is_ok()
        );

        let first = log_rx.recv().await.expect("first batch should be queued");
        assert_eq!(first.lines, vec!["first".to_string()]);

        assert!(
            sink.send(RuntimeNotification::TaskLogBatch(RuntimeTaskLogBatch {
                task_id,
                attempt_no: 1,
                stream: "stderr".to_string(),
                lines: vec!["after".to_string()],
                source_line_count: 1,
            }))
            .is_ok()
        );

        let second = log_rx.recv().await.expect("second batch should be queued");
        assert_eq!(
            second.lines,
            vec![
                "suppressed 3 stderr log lines".to_string(),
                "after".to_string()
            ]
        );
    }

    #[test]
    fn collect_terminal_runtime_replays_only_replays_stopped_exited_runtimes() {
        let temp_root =
            std::env::temp_dir().join(format!("streamserver-terminal-replay-{}", Uuid::now_v7()));
        let stopped_dir = temp_root.join("stopped").join("attempt-1");
        let completed_dir = temp_root.join("completed").join("attempt-1");

        let stopped_handle = RuntimeHandle {
            runtime_id: Uuid::now_v7(),
            task_id: Uuid::now_v7(),
            attempt_no: 1,
            worker_kind: WorkerKind::ZlmProxy,
            pid: Some(1234),
            started_at: Utc::now(),
            last_progress_at: Some(Utc::now()),
            state: RuntimeState::Exited,
            command_line: Some("ffmpeg -i input".to_string()),
            outputs: vec!["rtmp://127.0.0.1/live/test".to_string()],
            metadata: json!({
                "task_type": "stream_ingest",
                "stop": {
                    "reason": "user_requested"
                }
            }),
        };
        let completed_handle = RuntimeHandle {
            runtime_id: Uuid::now_v7(),
            task_id: Uuid::now_v7(),
            attempt_no: 1,
            worker_kind: WorkerKind::ZlmProxy,
            pid: Some(5678),
            started_at: Utc::now(),
            last_progress_at: Some(Utc::now()),
            state: RuntimeState::Exited,
            command_line: Some("ffmpeg -i input".to_string()),
            outputs: vec!["rtmp://127.0.0.1/live/test".to_string()],
            metadata: json!({
                "task_type": "stream_ingest"
            }),
        };

        persist_runtime_state(&stopped_dir, &stopped_handle, &SuccessCheck::ProcessExit)
            .expect("stopped runtime should persist");
        persist_runtime_state(
            &completed_dir,
            &completed_handle,
            &SuccessCheck::ProcessExit,
        )
        .expect("completed runtime should persist");

        let replays = collect_terminal_runtime_replays(
            temp_root.to_string_lossy().as_ref(),
            &LocalRuntimeRegistry::new(),
        );

        assert_eq!(replays.len(), 1);
        assert_eq!(replays[0].handle.task_id, stopped_handle.task_id);
        assert_eq!(replays[0].event.event_type, "canceled");

        let _ = fs::remove_dir_all(temp_root);
    }

    #[test]
    fn cleanup_persisted_runtime_state_removes_runtime_files() {
        let temp_root =
            std::env::temp_dir().join(format!("streamserver-runtime-cleanup-{}", Uuid::now_v7()));
        let task_id = Uuid::parse_str("019d8631-7061-71b3-a9ca-95874bddeb55").unwrap();
        let work_dir = temp_root.join(task_id.to_string()).join("attempt-2");
        let handle = RuntimeHandle {
            runtime_id: Uuid::now_v7(),
            task_id,
            attempt_no: 2,
            worker_kind: WorkerKind::ZlmProxy,
            pid: Some(4321),
            started_at: Utc::now(),
            last_progress_at: None,
            state: RuntimeState::Exited,
            command_line: Some("ffmpeg -i input".to_string()),
            outputs: vec!["rtmp://127.0.0.1/live/test".to_string()],
            metadata: json!({"task_type": "stream_ingest"}),
        };

        persist_runtime_state(&work_dir, &handle, &SuccessCheck::ProcessExit)
            .expect("runtime should persist");
        cleanup_persisted_runtime_state(
            temp_root.to_string_lossy().as_ref(),
            handle.task_id,
            handle.attempt_no,
        );

        assert!(!work_dir.join(RUNTIME_STATE_FILE).exists());
        assert!(!work_dir.join(RUNTIME_PID_FILE).exists());
        assert!(!work_dir.join(RUNTIME_COMMAND_FILE).exists());

        let _ = fs::remove_dir_all(temp_root);
    }
}
