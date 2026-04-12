use std::{
    collections::{HashMap, HashSet},
    ffi::CStr,
    fs,
    future::Future,
    net::Ipv4Addr,
    path::{Component, Path, PathBuf},
    process::Stdio,
    ptr,
    str::FromStr,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use chrono::{DateTime, Utc};
use media_domain::{
    InputKind, InputSpec, PublishSpec, PublishTargetKind, RecoveryPolicy, RuntimeHandle,
    RuntimeState, TaskSpec, TaskType, WorkerKind,
};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::mpsc,
    time::sleep,
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
    events: mpsc::UnboundedSender<RuntimeNotification>,
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
const ZLM_RUNTIME_VHOST: &str = "__defaultVhost__";
const LIVE_RELAY_APP: &str = "relay";

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

impl ManagedProcessExecutor {
    pub fn new(
        settings: AgentSettings,
        registry: LocalRuntimeRegistry,
        events: mpsc::UnboundedSender<RuntimeNotification>,
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

        match request.task_type {
            TaskType::LiveRelay => self.start_live_relay_task(request),
            TaskType::RtpReceive => self.start_rtp_receive_task(request),
            _ => self.start_process_task(request),
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

        let task_type = task_type_from_handle(&handle);
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
        } else if matches!(task_type, Some(TaskType::LiveRelay)) {
            self.stop_live_relay_recording(&handle)?;
            self.close_live_relay(&handle, true)?;
        } else if matches!(task_type, Some(TaskType::RtpReceive)) {
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
                    message: "rtp_receive server stopped".to_string(),
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

            match task_type_from_handle(&persisted.handle) {
                Some(TaskType::RtpReceive) => {
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
                                    message: "reattached persisted rtp_receive runtime".to_string(),
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
                Some(TaskType::LiveRelay) => {}
                _ => continue,
            }

            if !matches!(
                task_type_from_handle(&persisted.handle),
                Some(TaskType::LiveRelay)
            ) {
                continue;
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
                        message: "reattached persisted live_relay runtime".to_string(),
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
            worker_kind: request.task_type.default_worker_kind(),
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

            attach_transcode_artifact_metadata(&mut exited_handle, &success_check);

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
                message: "live_relay proxy created in ZLM".to_string(),
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
                message: "rtp_receive server opened in ZLM".to_string(),
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
    events: mpsc::UnboundedSender<RuntimeNotification>,
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

async fn read_log_stream(
    stderr: tokio::process::ChildStderr,
    task_id: Uuid,
    attempt_no: i32,
    stream: String,
    events: mpsc::UnboundedSender<RuntimeNotification>,
) {
    let mut reader = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = reader.next_line().await {
        let line = line.trim_end().to_string();
        if line.is_empty() {
            continue;
        }
        let _ = events.send(RuntimeNotification::TaskLogBatch(RuntimeTaskLogBatch {
            task_id,
            attempt_no,
            stream: stream.clone(),
            lines: vec![line],
        }));
    }
}

fn build_process_plan(
    settings: &AgentSettings,
    request: &StartTaskRequest,
) -> Result<ProcessPlan, ExecutorError> {
    let spec = parse_task_spec(request)?;

    match request.task_type {
        TaskType::FileTranscode => build_file_transcode_plan(settings, request, &spec),
        TaskType::FileToLive => build_file_to_live_plan(settings, request, &spec),
        TaskType::MulticastBridge => build_multicast_bridge_plan(settings, request, &spec),
        other => Err(ExecutorError::InvalidRequest(format!(
            "task type {other} is not yet supported by the managed executor"
        ))),
    }
}

fn parse_task_spec(request: &StartTaskRequest) -> Result<TaskSpec, ExecutorError> {
    serde_json::from_value(request.resolved_spec.clone()).map_err(|error| {
        ExecutorError::InvalidRequest(format!("invalid resolved_spec for task execution: {error}"))
    })
}

const ZLM_RECORD_HTTP_ROOT: &str = "/data/zlm/www/record";
const LEGACY_ZLM_RECORD_ROOT: &str = "/data/zlm/record";
const TRANSCODE_ARTIFACT_ROOT: &str = "/data/zlm/www/artifacts/transcode";

fn path_is_under_root(path: &str, root: &str) -> bool {
    path != root
        && path
            .strip_prefix(root)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn normalized_posix_path(path: &str) -> Result<String, ExecutorError> {
    let path = Path::new(path.trim());
    if !path.is_absolute() {
        return Err(ExecutorError::InvalidRequest(
            "file_transcode publish.url must be an absolute path".to_string(),
        ));
    }

    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(value) => parts.push(value.to_string_lossy().to_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(ExecutorError::InvalidRequest(
                    "file_transcode publish.url must not contain parent segments".to_string(),
                ));
            }
            Component::Prefix(_) => {
                return Err(ExecutorError::InvalidRequest(
                    "file_transcode publish.url must be a POSIX path".to_string(),
                ));
            }
        }
    }

    Ok(format!("/{}", parts.join("/")))
}

fn validate_transcode_output_path(path: &str) -> Result<String, ExecutorError> {
    let normalized = normalized_posix_path(path)?;
    if !path_is_under_root(&normalized, TRANSCODE_ARTIFACT_ROOT) {
        return Err(ExecutorError::InvalidRequest(format!(
            "file_transcode publish.url must stay under {TRANSCODE_ARTIFACT_ROOT}"
        )));
    }
    Ok(normalized)
}

fn build_file_transcode_plan(
    settings: &AgentSettings,
    request: &StartTaskRequest,
    spec: &TaskSpec,
) -> Result<ProcessPlan, ExecutorError> {
    let input_url = build_input_url(settings, &spec.input)?;

    let work_dir = attempt_work_dir(settings, request.task_id, request.attempt_no);
    let output_path = match spec.publish.kind {
        Some(PublishTargetKind::File) => {
            required_nonempty("publish.url", spec.publish.url.as_deref())?
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
    let output_path = validate_transcode_output_path(&output_path)?;
    let output_format = spec
        .publish
        .format
        .clone()
        .unwrap_or_else(|| infer_file_output_format(&output_path, "mp4"));
    let mut args = ffmpeg_base_args(input_url.clone(), false);
    append_process_args(
        &mut args,
        settings,
        spec,
        "copy_or_transcode",
        input_url.as_str(),
        VideoOutputPolicy::KeepSourceFamily,
        AudioOutputPolicy::Aac,
    )?;

    args.extend([
        "-threads".to_string(),
        "0".to_string(),
        "-f".to_string(),
        output_format,
        output_path.clone(),
    ]);

    Ok(ProcessPlan {
        executable: settings.ffmpeg_bin.clone(),
        args,
        work_dir,
        output_target: output_path.clone(),
        outputs: vec![output_path.clone()],
        success_check: SuccessCheck::FileExists(PathBuf::from(output_path)),
        startup_probe: None,
        recording: None,
    })
}

fn build_file_to_live_plan(
    settings: &AgentSettings,
    request: &StartTaskRequest,
    spec: &TaskSpec,
) -> Result<ProcessPlan, ExecutorError> {
    let input_url = match spec.input.kind {
        Some(InputKind::File | InputKind::HttpMp4 | InputKind::Hls | InputKind::HttpTs) => {
            build_input_url(settings, &spec.input)?
        }
        _ => {
            return Err(ExecutorError::InvalidRequest(
                "file_to_live requires input.kind=file|http_mp4|hls|http_ts".to_string(),
            ));
        }
    };
    let work_dir = attempt_work_dir(settings, request.task_id, request.attempt_no);
    let publish_output = build_publish_output(settings, &spec.publish)?;
    let startup_probe = build_startup_probe(&spec.publish)?;
    let mut outputs = vec![publish_output.target.clone()];
    let mut success_check = publish_output.success_check.clone();
    let mut recording = None;

    let mut args = ffmpeg_base_args(input_url.clone(), true);
    append_process_args(
        &mut args,
        settings,
        spec,
        "copy_or_transcode",
        input_url.as_str(),
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
                let record_path =
                    spec.record.save_path.clone().unwrap_or_else(|| {
                        work_dir.join("record.mp4").to_string_lossy().to_string()
                    });
                let tee_target = format!(
                    "[f={}:onfail=ignore]{}|[f={}:onfail=ignore]{}",
                    publish_output.format,
                    escape_tee_target(&publish_output.target),
                    record_format,
                    escape_tee_target(&record_path),
                );
                args.extend(["-f".to_string(), "tee".to_string(), tee_target]);
                outputs.push(record_path.clone());
                success_check = SuccessCheck::FileExists(PathBuf::from(record_path));
            }
            media_domain::RecordFormat::Hls | media_domain::RecordFormat::Both => {
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
    })
}

fn build_multicast_bridge_plan(
    settings: &AgentSettings,
    request: &StartTaskRequest,
    spec: &TaskSpec,
) -> Result<ProcessPlan, ExecutorError> {
    let input_url = build_input_url(settings, &spec.input)?;
    let work_dir = attempt_work_dir(settings, request.task_id, request.attempt_no);
    let output = build_publish_output(settings, &spec.publish)?;
    let startup_probe = matches!(spec.publish.kind, Some(PublishTargetKind::ZlmIngest))
        .then(|| build_startup_probe(&spec.publish))
        .transpose()?;
    let mut args = ffmpeg_base_args(input_url.clone(), false);
    insert_ffmpeg_input_args(
        &mut args,
        vec![
            "-use_wallclock_as_timestamps".to_string(),
            "1".to_string(),
            "-fflags".to_string(),
            "+genpts".to_string(),
        ],
    );
    if should_stabilize_live_mpegts_multicast_bridge(spec, &output) {
        // ZLM-published live inputs can surface unset/non-monotonic DTS when copied
        // directly into MPEG-TS. Re-encode video to regenerate timestamps while
        // keeping audio copy so the bridge stays close to passthrough semantics.
        append_live_mpegts_multicast_bridge_args(&mut args, settings, spec, input_url.as_str());
    } else {
        append_process_args(
            &mut args,
            settings,
            spec,
            "passthrough",
            input_url.as_str(),
            VideoOutputPolicy::ForceH264,
            AudioOutputPolicy::Aac,
        )?;
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
    })
}

fn should_stabilize_live_mpegts_multicast_bridge(spec: &TaskSpec, output: &PublishOutput) -> bool {
    output.format == "mpegts"
        && spec.process.mode.as_deref().unwrap_or("passthrough") == "passthrough"
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
    let input_url = match spec.input.kind {
        Some(
            InputKind::Rtsp
            | InputKind::Rtmp
            | InputKind::Hls
            | InputKind::HttpFlv
            | InputKind::HttpTs,
        ) => required_nonempty("input.url", spec.input.url.as_deref())?,
        Some(_) => {
            return Err(ExecutorError::InvalidRequest(
                "live_relay requires a network input kind".to_string(),
            ));
        }
        None => {
            return Err(ExecutorError::InvalidRequest(
                "input.kind must be provided for live_relay".to_string(),
            ));
        }
    };

    let startup_probe = StartupProbe {
        schema: Some(preferred_publish_schema(&spec.publish)),
        vhost: ZLM_RUNTIME_VHOST.to_string(),
        app: LIVE_RELAY_APP.to_string(),
        stream: request.task_id.to_string(),
    };
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
    if spec.input.kind != Some(InputKind::GbRtp) {
        return Err(ExecutorError::InvalidRequest(
            "rtp_receive requires input.kind=gb_rtp".to_string(),
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
enum VideoCodecFamily {
    H264,
    Hevc,
    Unknown,
}

#[derive(Debug, Clone)]
struct TranscodeSelection {
    input_args: Vec<String>,
    video_encoder: String,
    audio_encoder: String,
}

fn build_input_url(settings: &AgentSettings, input: &InputSpec) -> Result<String, ExecutorError> {
    match input.kind {
        Some(
            InputKind::Rtsp
            | InputKind::Rtmp
            | InputKind::Hls
            | InputKind::HttpMp4
            | InputKind::HttpFlv
            | InputKind::HttpTs
            | InputKind::File,
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

fn ffmpeg_base_args(input_url: String, realtime: bool) -> Vec<String> {
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
    args.extend([
        "-i".to_string(),
        input_url,
        "-map".to_string(),
        "0:v?".to_string(),
        "-map".to_string(),
        "0:a?".to_string(),
    ]);
    args
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
    video_policy: VideoOutputPolicy,
    audio_policy: AudioOutputPolicy,
) -> Result<(), ExecutorError> {
    let mode = normalized_process_mode(spec, default_mode);
    match mode {
        "passthrough" => {
            args.extend([
                "-c:v".to_string(),
                "copy".to_string(),
                "-c:a".to_string(),
                "copy".to_string(),
            ]);
        }
        "copy_or_transcode" | "force_transcode" => {
            let selection =
                resolve_transcode_selection(settings, input_url, video_policy, audio_policy);
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
        }
        other => {
            return Err(ExecutorError::InvalidRequest(format!(
                "unsupported process.mode: {other}"
            )));
        }
    }

    Ok(())
}

fn resolve_transcode_selection(
    settings: &AgentSettings,
    input_url: &str,
    video_policy: VideoOutputPolicy,
    audio_policy: AudioOutputPolicy,
) -> TranscodeSelection {
    let (input_family, output_family) = resolve_video_families(settings, input_url, video_policy);
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
    }
}

fn resolve_video_families(
    settings: &AgentSettings,
    input_url: &str,
    video_policy: VideoOutputPolicy,
) -> (VideoCodecFamily, VideoCodecFamily) {
    let input_family = probe_primary_video_codec_family(settings, input_url);
    let output_family = match video_policy {
        VideoOutputPolicy::KeepSourceFamily => match input_family {
            VideoCodecFamily::Hevc => VideoCodecFamily::Hevc,
            _ => VideoCodecFamily::H264,
        },
        VideoOutputPolicy::ForceH264 => VideoCodecFamily::H264,
    };
    (input_family, output_family)
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

fn build_publish_output(
    settings: &AgentSettings,
    publish: &PublishSpec,
) -> Result<PublishOutput, ExecutorError> {
    match publish.kind {
        Some(PublishTargetKind::File) => {
            let target = required_nonempty("publish.url", publish.url.as_deref())?;
            let format = publish
                .format
                .clone()
                .unwrap_or_else(|| infer_file_output_format(&target, "mpegts"));
            Ok(PublishOutput {
                success_check: SuccessCheck::FileExists(PathBuf::from(&target)),
                target,
                format,
            })
        }
        Some(PublishTargetKind::ZlmIngest) => {
            let target = required_nonempty("publish.url", publish.url.as_deref())?;
            let format = publish
                .format
                .clone()
                .unwrap_or_else(|| infer_url_output_format(&target));
            Ok(PublishOutput {
                success_check: SuccessCheck::ProcessExit,
                target,
                format,
            })
        }
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
            bool_as_flag(spec.publish.enable_rtsp.unwrap_or(true)),
        ),
        (
            "enable_rtmp".to_string(),
            bool_as_flag(spec.publish.enable_rtmp.unwrap_or(true)),
        ),
        (
            "enable_hls".to_string(),
            bool_as_flag(spec.publish.enable_hls.unwrap_or(false) || spec.record.wants_hls()),
        ),
        (
            "enable_ts".to_string(),
            bool_as_flag(spec.publish.enable_http_ts.unwrap_or(true)),
        ),
        (
            "enable_fmp4".to_string(),
            bool_as_flag(spec.publish.enable_http_fmp4.unwrap_or(true)),
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
    work_dir: &Path,
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
    let root_path = normalize_record_root(spec.record.save_path.as_deref(), work_dir);

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

fn normalize_record_root(save_path: Option<&str>, work_dir: &Path) -> String {
    let Some(save_path) = save_path.map(str::trim).filter(|value| !value.is_empty()) else {
        return ZLM_RECORD_HTTP_ROOT.to_string();
    };
    let path = PathBuf::from(save_path);
    let root = if path.extension().is_some() {
        path.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(work_dir)
            .to_path_buf()
    } else {
        path
    };
    normalize_record_root_path(&root)
}

fn normalize_record_root_path(root: &Path) -> String {
    if root.as_os_str().is_empty() {
        return ZLM_RECORD_HTTP_ROOT.to_string();
    }

    if root.is_relative() {
        return Path::new(ZLM_RECORD_HTTP_ROOT)
            .join(root)
            .to_string_lossy()
            .to_string();
    }

    match root.strip_prefix(LEGACY_ZLM_RECORD_ROOT) {
        Ok(suffix) => Path::new(ZLM_RECORD_HTTP_ROOT)
            .join(suffix)
            .to_string_lossy()
            .to_string(),
        Err(_) => root.to_string_lossy().to_string(),
    }
}

fn build_startup_probe(publish: &PublishSpec) -> Result<StartupProbe, ExecutorError> {
    let url = required_nonempty("publish.url", publish.url.as_deref())?;
    let url = Url::parse(&url).map_err(|error| {
        ExecutorError::InvalidRequest(format!("publish.url must be a valid URL: {error}"))
    })?;
    let mut segments = url
        .path_segments()
        .ok_or_else(|| {
            ExecutorError::InvalidRequest(
                "publish.url must include /<app>/<stream> path segments".to_string(),
            )
        })?
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if segments.len() < 2 {
        return Err(ExecutorError::InvalidRequest(
            "publish.url must include /<app>/<stream> path segments".to_string(),
        ));
    }
    let stream = segments.pop().expect("checked len").to_string();
    let app = segments.pop().expect("checked len").to_string();
    Ok(StartupProbe {
        schema: Some(url.scheme().to_ascii_lowercase()),
        vhost: ZLM_RUNTIME_VHOST.to_string(),
        app,
        stream,
    })
}

fn preferred_publish_schema(publish: &PublishSpec) -> String {
    if publish.enable_rtmp.unwrap_or(true) {
        "rtmp".to_string()
    } else if publish.enable_rtsp.unwrap_or(true) {
        "rtsp".to_string()
    } else if publish.enable_http_ts.unwrap_or(true) {
        "ts".to_string()
    } else if publish.enable_http_fmp4.unwrap_or(true) {
        "fmp4".to_string()
    } else if publish.enable_hls.unwrap_or(false) {
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

fn infer_file_output_format(path: &str, default_format: &str) -> String {
    match Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .as_deref()
    {
        Some("mp4") => "mp4".to_string(),
        Some("mkv") => "matroska".to_string(),
        Some("ts") | Some("mpegts") => "mpegts".to_string(),
        Some("flv") => "flv".to_string(),
        Some("mov") => "mov".to_string(),
        _ => default_format.to_string(),
    }
}

fn infer_url_output_format(url: &str) -> String {
    match url
        .split_once("://")
        .map(|(scheme, _)| scheme.to_ascii_lowercase())
        .as_deref()
    {
        Some("rtmp") | Some("rtmps") => "flv".to_string(),
        Some("rtsp") => "rtsp".to_string(),
        Some("udp") => "mpegts".to_string(),
        Some("rtp") => "rtp_mpegts".to_string(),
        _ => "flv".to_string(),
    }
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
    settings.zlm_auto_close_on_no_reader_enabled && spec.publish.stop_on_no_reader.unwrap_or(false)
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
        || task_type_from_handle(handle) != Some(TaskType::FileToLive)
        || !stream_online(handle)
        || fatal_recording_error_from_handle(handle).is_some()
    {
        return false;
    }

    if !matches!(
        recovery_policy_from_handle(handle),
        Some(RecoveryPolicy::Always | RecoveryPolicy::OnFailure)
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
    events: mpsc::UnboundedSender<RuntimeNotification>,
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
            attach_transcode_artifact_metadata(&mut exited_handle, &success_check);

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
    events: mpsc::UnboundedSender<RuntimeNotification>,
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
    events: mpsc::UnboundedSender<RuntimeNotification>,
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
    events: mpsc::UnboundedSender<RuntimeNotification>,
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

fn attach_transcode_artifact_metadata(handle: &mut RuntimeHandle, success_check: &SuccessCheck) {
    if task_type_from_handle(handle) != Some(TaskType::FileTranscode) {
        return;
    }
    let SuccessCheck::FileExists(path) = success_check else {
        return;
    };
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    if !metadata.is_file() {
        return;
    }

    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_string();
    let file_path = path.to_string_lossy().to_string();

    let Some(object) = handle.metadata.as_object_mut() else {
        return;
    };
    object.insert(
        "transcode_artifact".to_string(),
        json!({
            "file_name": file_name,
            "file_path": file_path,
            "file_size": i64::try_from(metadata.len()).unwrap_or(i64::MAX),
        }),
    );
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
            Some(TaskType::FileToLive) => (
                "succeeded",
                "info",
                "adopted file_to_live process exited; treating as completed".to_string(),
                json!({
                    "output_target": output_target,
                    "orphaned": true,
                }),
            ),
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

    fn create_mock_ffprobe_binary(root: &Path, codec_name: &str) -> String {
        let path = root.join("mock-ffprobe.sh");
        let body = format!(
            r#"#!/usr/bin/env bash
set -euo pipefail
echo "{codec_name}"
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
    fn build_file_transcode_plan_uses_publish_file_target() {
        let settings = test_settings("/tmp/work");
        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::FileTranscode,
            resolved_spec: json!({
                "type": "file_transcode",
                "name": "test",
                "common": {"created_by": "tester"},
                "input": {"kind": "file", "url": "/tmp/input.mp4"},
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

        let spec = parse_task_spec(&request).expect("spec should parse");
        let plan =
            build_file_transcode_plan(&settings, &request, &spec).expect("plan should build");
        assert_eq!(plan.executable, "ffmpeg");
        assert!(plan.args.iter().any(|arg| arg == "pipe:1"));
        assert_eq!(
            plan.output_target,
            "/data/zlm/www/artifacts/transcode/output.mp4"
        );
    }

    #[test]
    fn resolve_video_families_keeps_hevc_input_probe_for_force_h264() {
        let temp_root =
            std::env::temp_dir().join(format!("streamserver-gpu-probe-{}", Uuid::now_v7()));
        fs::create_dir_all(&temp_root).expect("temp root should exist");

        let mut settings = test_settings("/tmp/work");
        settings.ffprobe_bin = create_mock_ffprobe_binary(&temp_root, "hevc");

        let (input_family, output_family) =
            resolve_video_families(&settings, "/tmp/input.mp4", VideoOutputPolicy::ForceH264);

        assert_eq!(input_family, VideoCodecFamily::Hevc);
        assert_eq!(output_family, VideoCodecFamily::H264);

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
    fn build_file_transcode_plan_rejects_output_outside_http_root() {
        let settings = test_settings("/tmp/work");
        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::FileTranscode,
            resolved_spec: json!({
                "type": "file_transcode",
                "name": "test",
                "common": {"created_by": "tester"},
                "input": {"kind": "file", "url": "/tmp/input.mp4"},
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
            .expect_err("plan should reject output outside web root");
        assert!(matches!(
            error,
            ExecutorError::InvalidRequest(message)
                if message.contains(TRANSCODE_ARTIFACT_ROOT)
        ));
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

        let (events_tx, _events_rx) = mpsc::unbounded_channel();
        let mut settings = test_settings(temp_root.to_string_lossy().as_ref());
        settings.max_runtime_slots = 1;
        settings.ffmpeg_bin = "/definitely/missing-ffmpeg".to_string();
        let executor = ManagedProcessExecutor::new(settings, registry, events_tx);
        let request = StartTaskRequest {
            task_id: Uuid::now_v7(),
            attempt_no: 1,
            task_type: TaskType::FileTranscode,
            resolved_spec: json!({
                "type": "file_transcode",
                "name": "test",
                "common": {"created_by": "tester"},
                "input": {"kind": "file", "url": "/tmp/input.mp4"},
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
            task_type: TaskType::MulticastBridge,
            resolved_spec: json!({
                "type": "multicast_bridge",
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
            task_type: TaskType::MulticastBridge,
            resolved_spec: json!({
                "type": "multicast_bridge",
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
    fn build_multicast_bridge_plan_waits_for_zlm_stream_when_ingesting() {
        let settings = test_settings("/tmp/work");
        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::MulticastBridge,
            resolved_spec: json!({
                "type": "multicast_bridge",
                "name": "bridge-to-zlm",
                "common": {"created_by": "tester"},
                "input": {
                    "kind": "udp_mpegts_multicast",
                    "group": "239.10.10.10",
                    "port": 5000,
                    "interface_ip": "192.168.1.10"
                },
                "process": {"mode": "passthrough"},
                "publish": {
                    "kind": "zlm_ingest",
                    "url": "rtmp://zlmediakit/live/bridge-ingest"
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

        assert_eq!(plan.output_target, "rtmp://zlmediakit/live/bridge-ingest");
        assert_eq!(
            plan.startup_probe
                .as_ref()
                .and_then(|probe| probe.schema.as_deref()),
            Some("rtmp")
        );
        assert_eq!(
            plan.startup_probe.as_ref().map(|probe| probe.app.as_str()),
            Some("live")
        );
        assert_eq!(
            plan.startup_probe
                .as_ref()
                .map(|probe| probe.stream.as_str()),
            Some("bridge-ingest")
        );
    }

    #[test]
    fn build_multicast_bridge_plan_uses_agent_default_multicast_interface_ip() {
        let mut settings = test_settings("/tmp/work");
        settings.multicast_interface_ip = "192.168.50.20".to_string();
        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::MulticastBridge,
            resolved_spec: json!({
                "type": "multicast_bridge",
                "name": "bridge-default-multicast",
                "common": {"created_by": "tester"},
                "input": {
                    "kind": "udp_mpegts_multicast",
                    "group": "239.10.10.10",
                    "port": 5000
                },
                "process": {"mode": "passthrough"},
                "publish": {
                    "kind": "zlm_ingest",
                    "url": "rtmp://zlmediakit/live/bridge-default"
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
                .iter()
                .any(|arg| arg == "udp://239.10.10.10:5000?localaddr=192.168.50.20")
        );
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
    fn build_file_to_live_plan_uses_realtime_tee_output() {
        let settings = test_settings("/tmp/work");
        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::FileToLive,
            resolved_spec: json!({
                "type": "file_to_live",
                "name": "file-live",
                "common": {"created_by": "tester"},
                "input": {"kind": "file", "url": "/tmp/input.mp4"},
                "process": {"mode": "copy_or_transcode"},
                "publish": {
                    "kind": "zlm_ingest",
                    "url": "rtmp://127.0.0.1/live/stream"
                },
                "record": {
                    "enabled": true,
                    "format": "mp4",
                    "save_path": "/tmp/archive.mp4"
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
                "/tmp/archive.mp4".to_string()
            ]
        );
    }

    #[test]
    fn build_file_to_live_plan_accepts_http_mp4_and_duration_limit() {
        let settings = test_settings("/tmp/work");
        let request = StartTaskRequest {
            task_id: Uuid::nil(),
            attempt_no: 1,
            task_type: TaskType::FileToLive,
            resolved_spec: json!({
                "type": "file_to_live",
                "name": "file-live-http",
                "common": {"created_by": "tester"},
                "input": {"kind": "http_mp4", "url": "http://vod.example.com/archive.mp4"},
                "process": {"mode": "copy_or_transcode"},
                "publish": {
                    "kind": "zlm_ingest",
                    "url": "rtmp://127.0.0.1/live/stream"
                },
                "record": {
                    "enabled": true,
                    "format": "mp4",
                    "duration_sec": 300,
                    "save_path": "/tmp/archive.mp4"
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
    fn build_live_relay_plan_allocates_stable_stream_binding() {
        let settings = test_settings("/tmp/work");
        let task_id = Uuid::now_v7();
        let request = StartTaskRequest {
            task_id,
            attempt_no: 1,
            task_type: TaskType::LiveRelay,
            resolved_spec: json!({
                "type": "live_relay",
                "name": "relay",
                "common": {"created_by": "tester"},
                "input": {"kind": "rtsp", "url": "rtsp://camera.example/live"},
                "publish": {
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
        assert_eq!(plan.startup_probe.app, "relay");
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
            "type": "live_relay",
            "name": "relay",
            "common": {"created_by": "tester"},
            "input": {"kind": "rtsp", "url": "rtsp://camera.example/live", "probe_timeout_ms": 7000},
            "publish": {
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
    fn build_live_relay_plan_includes_recording_root_when_enabled() {
        let settings = test_settings("/tmp/work");
        let request = StartTaskRequest {
            task_id: Uuid::now_v7(),
            attempt_no: 1,
            task_type: TaskType::LiveRelay,
            resolved_spec: json!({
                "type": "live_relay",
                "name": "relay-record",
                "common": {"created_by": "tester"},
                "input": {"kind": "rtsp", "url": "rtsp://camera.example/live"},
                "publish": {},
                "record": {
                    "enabled": true,
                    "format": "mp4",
                    "save_path": "/var/media/archive/session.mp4",
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
        assert_eq!(recording.root_path, "/var/media/archive");
        assert_eq!(recording.duration_sec, None);
        assert_eq!(recording.segment_sec, Some(120));
        assert!(
            plan.outputs
                .iter()
                .any(|output| output == "/var/media/archive")
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
    fn normalize_record_root_defaults_to_http_record_root() {
        assert_eq!(
            normalize_record_root(None, Path::new("/tmp/work")),
            "/data/zlm/www/record"
        );
    }

    #[test]
    fn normalize_record_root_maps_legacy_record_root_into_http_root() {
        assert_eq!(
            normalize_record_root(
                Some("/data/zlm/record/live/archive.mp4"),
                Path::new("/tmp/work")
            ),
            "/data/zlm/www/record/live"
        );
        assert_eq!(
            normalize_record_root(Some("/data/zlm/record/live"), Path::new("/tmp/work")),
            "/data/zlm/www/record/live"
        );
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
            task_type: TaskType::RtpReceive,
            resolved_spec: json!({
                "type": "rtp_receive",
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
            task_type: TaskType::RtpReceive,
            resolved_spec: json!({
                "type": "rtp_receive",
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
        let (events_tx, _events_rx) = mpsc::unbounded_channel();
        let executor = ManagedProcessExecutor::new(
            test_settings(temp_root.to_string_lossy().as_ref()),
            registry.clone(),
            events_tx,
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
}
