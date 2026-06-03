//! Runtime 受管进程启动：构造本地进程计划、spawn 主进程并注册启动后的监控与伴随进程。
//!
//! 这里集中维护 FFmpeg/本地进程 runtime 的启动执行路径，包括计划路径准备、metadata
//! 登记、stdout/stderr 事件读取、startup probe、退出监控和伴随录制 sidecar 启动。

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, RwLock, atomic::AtomicBool},
    time::Duration,
};

use chrono::Utc;
use media_domain::{RuntimeHandle, RuntimeState};
use reqwest::Client;
use serde_json::{Value, json};
use tokio::process::{ChildStderr, ChildStdout, Command};
use uuid::Uuid;

use crate::{
    config::AgentSettings,
    runtime::{
        ExecutorError, ManagedProcessExecutor, RuntimeCapabilityHints, StartTaskRequest,
        SuccessCheck,
    },
    runtime_events::{
        RuntimeEventSink, RuntimeNotification, RuntimeTaskEvent, read_log_stream,
        read_progress_stream, runtime_session_epoch,
    },
    runtime_io::render_command_line,
    runtime_metadata::{
        CompanionProcessMetadata, CompanionProcessState, attach_zlm_server_id, runtime_lease_token,
        update_companion_recording_metadata,
    },
    runtime_monitors::spawn_startup_probe_monitor,
    runtime_persistence::{
        RUNTIME_COMMAND_FILE, RUNTIME_PID_FILE, RUNTIME_STATE_FILE,
        persist_runtime_state as persist_runtime_state_to_disk,
    },
    runtime_plan::{CompanionProcessPlan, build_process_plan, prepare_plan_paths},
    runtime_process::{
        ManagedRuntime, ProcessIdentity, RuntimeSlotPermit, configure_new_process_group,
        remove_managed_runtime, schedule_force_kill_processes_if_running, signal_process,
    },
    runtime_process_exit::{ProcessExitMonitorContext, spawn_process_exit_monitor},
    runtime_process_monitors::spawn_companion_process_monitor,
    runtime_registry::LocalRuntimeRegistry,
};

const START_ROLLBACK_FORCE_KILL_DELAY: Duration = Duration::from_millis(250);

type PersistRuntimeStateHook =
    dyn Fn(&Path, &RuntimeHandle, &SuccessCheck) -> Result<(), ExecutorError> + Send + Sync;
type AfterRuntimeInsertHook = dyn Fn(&RuntimeHandle) -> Result<(), ExecutorError> + Send + Sync;
type AfterCompanionSpawnHook =
    dyn Fn(&RuntimeHandle, i32) -> Result<(), ExecutorError> + Send + Sync;

#[derive(Clone)]
pub(crate) struct ManagedProcessStartHooks {
    persist_runtime_state: Arc<PersistRuntimeStateHook>,
    after_runtime_insert: Arc<AfterRuntimeInsertHook>,
    after_companion_spawn: Arc<AfterCompanionSpawnHook>,
}

impl Default for ManagedProcessStartHooks {
    fn default() -> Self {
        Self {
            persist_runtime_state: Arc::new(persist_runtime_state_to_disk),
            after_runtime_insert: Arc::new(|_| Ok(())),
            after_companion_spawn: Arc::new(|_, _| Ok(())),
        }
    }
}

impl ManagedProcessStartHooks {
    fn persist_runtime_state(
        &self,
        work_dir: &Path,
        handle: &RuntimeHandle,
        success_check: &SuccessCheck,
    ) -> Result<(), ExecutorError> {
        (self.persist_runtime_state)(work_dir, handle, success_check)
    }

    fn after_runtime_insert(&self, handle: &RuntimeHandle) -> Result<(), ExecutorError> {
        (self.after_runtime_insert)(handle)
    }

    fn after_companion_spawn(
        &self,
        handle: &RuntimeHandle,
        companion_pid: i32,
    ) -> Result<(), ExecutorError> {
        (self.after_companion_spawn)(handle, companion_pid)
    }

    #[cfg(test)]
    pub(crate) fn with_persist_runtime_state(
        persist_runtime_state: impl Fn(
            &Path,
            &RuntimeHandle,
            &SuccessCheck,
        ) -> Result<(), ExecutorError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            persist_runtime_state: Arc::new(persist_runtime_state),
            ..Self::default()
        }
    }

    #[cfg(test)]
    pub(crate) fn with_after_runtime_insert(
        after_runtime_insert: impl Fn(&RuntimeHandle) -> Result<(), ExecutorError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            after_runtime_insert: Arc::new(after_runtime_insert),
            ..Self::default()
        }
    }

    #[cfg(test)]
    pub(crate) fn with_after_companion_spawn(
        after_companion_spawn: impl Fn(&RuntimeHandle, i32) -> Result<(), ExecutorError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            after_companion_spawn: Arc::new(after_companion_spawn),
            ..Self::default()
        }
    }
}

pub(crate) struct RuntimeStartRollback {
    runtime_id: Uuid,
    work_dir: PathBuf,
    registry: LocalRuntimeRegistry,
    runtimes: Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    processes: Vec<ProcessIdentity>,
    armed: bool,
}

impl RuntimeStartRollback {
    pub(crate) fn new(
        runtime_id: Uuid,
        work_dir: PathBuf,
        process: ProcessIdentity,
        registry: LocalRuntimeRegistry,
        runtimes: Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    ) -> Self {
        Self {
            runtime_id,
            work_dir,
            registry,
            runtimes,
            processes: vec![process],
            armed: true,
        }
    }

    pub(crate) fn add_companion_process(&mut self, companion_process: ProcessIdentity) {
        if !self
            .processes
            .iter()
            .any(|process| process.pid == companion_process.pid)
        {
            self.processes.push(companion_process);
        }
    }

    fn disarm(mut self) {
        self.armed = false;
    }

    fn cleanup_persisted_runtime_files(&self) {
        for file_name in [RUNTIME_STATE_FILE, RUNTIME_PID_FILE, RUNTIME_COMMAND_FILE] {
            let _ = fs::remove_file(self.work_dir.join(file_name));
        }
    }
}

impl Drop for RuntimeStartRollback {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }

        let mut processes = self.processes.clone();
        if let Some(runtime) = remove_managed_runtime(&self.runtimes, self.runtime_id) {
            if let Some(process) = runtime
                .process
                .filter(|process| !processes.iter().any(|known| known.pid == process.pid))
            {
                processes.push(process);
            }
            for companion_process in runtime.companion_processes {
                if !processes
                    .iter()
                    .any(|known| known.pid == companion_process.pid)
                {
                    processes.push(companion_process);
                }
            }
        }
        let _ = self.registry.remove(self.runtime_id);
        self.cleanup_persisted_runtime_files();

        for process in &processes {
            let _ = signal_process(process, libc::SIGTERM);
        }
        schedule_force_kill_processes_if_running(
            processes,
            START_ROLLBACK_FORCE_KILL_DELAY,
            "runtime_start_rollback",
        );
    }
}

pub(crate) struct ManagedProcessStartContext<'a> {
    pub(crate) settings: &'a AgentSettings,
    pub(crate) http_client: &'a Client,
    pub(crate) registry: &'a LocalRuntimeRegistry,
    pub(crate) runtimes: &'a Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    pub(crate) events: &'a RuntimeEventSink,
    pub(crate) zlm_server_id: Option<String>,
    pub(crate) capability_hints: RuntimeCapabilityHints,
    pub(crate) restart_executor: ManagedProcessExecutor,
    pub(crate) hooks: ManagedProcessStartHooks,
}

pub(crate) fn start_process_task(
    context: ManagedProcessStartContext<'_>,
    request: &StartTaskRequest,
    slot_permit: Arc<RuntimeSlotPermit>,
) -> Result<RuntimeHandle, ExecutorError> {
    let ManagedProcessStartContext {
        settings,
        http_client,
        registry,
        runtimes,
        events,
        zlm_server_id,
        capability_hints,
        restart_executor,
        hooks,
    } = context;

    let plan = build_process_plan(settings, request, capability_hints)?;
    prepare_plan_paths(&plan)?;

    let command_line = render_command_line(&plan.executable, &plan.args);
    let mut child = Command::new(&plan.executable);
    child
        .args(&plan.args)
        .current_dir(&plan.work_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_new_process_group(&mut child);

    let mut child = child
        .spawn()
        .map_err(|error| ExecutorError::ProcessSpawn(error.to_string()))?;
    let pid = child
        .id()
        .map(|pid| pid as i32)
        .ok_or_else(|| ExecutorError::ProcessSpawn("spawned child has no pid".to_string()))?;
    let process = ProcessIdentity::spawned_process_group(pid);
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let runtime_id = Uuid::now_v7();
    let mut rollback = RuntimeStartRollback::new(
        runtime_id,
        plan.work_dir.clone(),
        process,
        registry.clone(),
        runtimes.clone(),
    );
    let stop_requested = Arc::new(AtomicBool::new(false));
    let require_stream_online = plan.startup_probe.is_some();
    let companion_recording_metadata = plan
        .companion_recording
        .as_ref()
        .map(initial_companion_recording_metadata);

    let mut metadata = json!({
        "task_type": request.task_type,
        "execution_mode": request.execution_mode,
        "lease_token": request.lease_token,
        "session_epoch": request.session_epoch,
        "trace_context": request.trace_context,
        "resolved_spec": request.resolved_spec,
        "work_dir": plan.work_dir,
        "output_target": plan.output_target,
        "outputs": plan.outputs,
        "startup_probe": plan.startup_probe,
        "stream_online": plan.startup_probe.is_none(),
        "recording": plan.recording,
        "managed_file_output_kind": plan.managed_file_output_kind,
        "companion_recording": companion_recording_metadata,
        "process": process,
    });
    if let Some(protocol) = plan.internal_ingress_protocol.as_deref() {
        metadata["internal_ingress_protocol"] = json!(protocol);
    }
    attach_zlm_server_id(&mut metadata, zlm_server_id.as_deref());
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
        metadata,
    };
    hooks.persist_runtime_state(&plan.work_dir, &handle, &plan.success_check)?;

    {
        let mut runtimes = runtimes.write().expect("runtime map lock poisoned");
        runtimes.insert(
            runtime_id,
            ManagedRuntime {
                process: Some(process),
                companion_processes: Vec::new(),
                _slot_permit: slot_permit,
                stop_requested: stop_requested.clone(),
                suppress_companion_events: Arc::new(AtomicBool::new(false)),
            },
        );
    }
    hooks.after_runtime_insert(&handle)?;

    registry.track(handle.clone());
    spawn_process_stream_readers(
        ProcessStreamReaderContext {
            runtime_id,
            handle: handle.clone(),
            require_stream_online,
            registry: registry.clone(),
            events: events.clone(),
        },
        stdout,
        stderr,
    );

    if let Some(companion_plan) = plan.companion_recording.clone() {
        start_companion_recording_process(
            CompanionRecordingStartContext {
                runtime_id,
                handle: handle.clone(),
                work_dir: plan.work_dir.clone(),
                success_check: plan.success_check.clone(),
                registry: registry.clone(),
                runtimes: runtimes.clone(),
                events: events.clone(),
                rollback: &mut rollback,
                hooks: hooks.clone(),
            },
            companion_plan,
        )?;
    }

    if let Some(startup_probe) = plan.startup_probe.clone() {
        spawn_startup_probe_monitor(
            runtime_id,
            plan.work_dir.clone(),
            plan.success_check.clone(),
            startup_probe,
            settings.clone(),
            http_client.clone(),
            registry.clone(),
            runtimes.clone(),
            events.clone(),
        );
    } else {
        let running_handle = registry
            .update(runtime_id, |runtime| {
                runtime.state = RuntimeState::Running;
            })
            .unwrap_or_else(|| handle.clone());
        hooks.persist_runtime_state(&plan.work_dir, &running_handle, &plan.success_check)?;
        let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
            task_id: running_handle.task_id,
            attempt_no: running_handle.attempt_no,
            lease_token: runtime_lease_token(&running_handle).unwrap_or_default(),
            session_epoch: runtime_session_epoch(&running_handle),
            event_type: "running".to_string(),
            event_level: "info".to_string(),
            message: "child process started".to_string(),
            payload: json!({
                "runtime_id": running_handle.runtime_id,
                "pid": running_handle.pid,
            }),
        }));
        let _ = events.send(RuntimeNotification::TaskSnapshot(running_handle.clone()));
    }

    spawn_process_exit_monitor(
        ProcessExitMonitorContext {
            runtime_id,
            wait_handle: handle.clone(),
            work_dir: plan.work_dir.clone(),
            output_target: plan.output_target.clone(),
            success_check: plan.success_check.clone(),
            stop_requested,
            registry: registry.clone(),
            runtimes: runtimes.clone(),
            events: events.clone(),
            restart_executor,
        },
        child,
    );

    rollback.disarm();
    Ok(handle)
}

pub(crate) struct ProcessStreamReaderContext {
    pub(crate) runtime_id: Uuid,
    pub(crate) handle: RuntimeHandle,
    pub(crate) require_stream_online: bool,
    pub(crate) registry: LocalRuntimeRegistry,
    pub(crate) events: RuntimeEventSink,
}

pub(crate) fn initial_companion_recording_metadata(companion: &CompanionProcessPlan) -> Value {
    json!(CompanionProcessMetadata {
        kind: companion.kind,
        pid: None,
        pgid: None,
        pid_start_time: None,
        output_target: companion.output_target.clone(),
        outputs: companion.outputs.clone(),
        command_line: Some(render_command_line(&companion.executable, &companion.args,)),
        state: CompanionProcessState::Starting,
        error: None,
    })
}

pub(crate) fn spawn_process_stream_readers(
    context: ProcessStreamReaderContext,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
) {
    let ProcessStreamReaderContext {
        runtime_id,
        handle,
        require_stream_online,
        registry,
        events,
    } = context;

    if let Some(stdout) = stdout {
        let events = events.clone();
        let registry = registry.clone();
        let progress_handle = handle.clone();
        tokio::spawn(async move {
            read_progress_stream(
                stdout,
                runtime_id,
                progress_handle.task_id,
                progress_handle.attempt_no,
                runtime_lease_token(&progress_handle).unwrap_or_default(),
                registry,
                events,
                require_stream_online,
            )
            .await;
        });
    }
    if let Some(stderr) = stderr {
        let log_handle = handle;
        tokio::spawn(async move {
            read_log_stream(
                stderr,
                runtime_id,
                log_handle.task_id,
                log_handle.attempt_no,
                runtime_lease_token(&log_handle).unwrap_or_default(),
                "stderr".to_string(),
                registry,
                events,
            )
            .await;
        });
    }
}

pub(crate) struct CompanionRecordingStartContext<'a> {
    pub(crate) runtime_id: Uuid,
    pub(crate) handle: RuntimeHandle,
    pub(crate) work_dir: PathBuf,
    pub(crate) success_check: SuccessCheck,
    pub(crate) registry: LocalRuntimeRegistry,
    pub(crate) runtimes: Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    pub(crate) events: RuntimeEventSink,
    pub(crate) rollback: &'a mut RuntimeStartRollback,
    pub(crate) hooks: ManagedProcessStartHooks,
}

pub(crate) fn start_companion_recording_process(
    context: CompanionRecordingStartContext,
    companion_plan: CompanionProcessPlan,
) -> Result<(), ExecutorError> {
    let CompanionRecordingStartContext {
        runtime_id,
        handle,
        work_dir,
        success_check,
        registry,
        runtimes,
        events,
        rollback,
        hooks,
    } = context;

    let companion_command_line =
        render_command_line(&companion_plan.executable, &companion_plan.args);
    let mut companion_child = Command::new(&companion_plan.executable);
    companion_child
        .args(&companion_plan.args)
        .current_dir(&companion_plan.work_dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    configure_new_process_group(&mut companion_child);

    match companion_child.spawn() {
        Ok(mut companion_child) => {
            let companion_pid =
                companion_child
                    .id()
                    .map(|value| value as i32)
                    .ok_or_else(|| {
                        ExecutorError::ProcessSpawn(
                            "spawned companion child has no pid".to_string(),
                        )
                    })?;
            let companion_process = ProcessIdentity::spawned_process_group(companion_pid);
            rollback.add_companion_process(companion_process);
            hooks.after_companion_spawn(&handle, companion_pid)?;
            let updated_handle = registry
                .update(runtime_id, |runtime| {
                    update_companion_recording_metadata(runtime, |companion| {
                        companion.pid = Some(companion_pid);
                        companion.pgid = companion_process.pgid;
                        companion.pid_start_time = companion_process.pid_start_time;
                        companion.command_line = Some(companion_command_line.clone());
                        companion.state = CompanionProcessState::Running;
                        companion.error = None;
                    });
                })
                .unwrap_or_else(|| handle.clone());
            hooks.persist_runtime_state(&work_dir, &updated_handle, &success_check)?;
            {
                let mut runtimes = runtimes.write().expect("runtime map lock poisoned");
                runtimes
                    .entry(runtime_id)
                    .and_modify(|runtime| runtime.companion_processes.push(companion_process));
            }

            if let Some(stderr) = companion_child.stderr.take() {
                let events = events.clone();
                let recording_log_handle = handle.clone();
                let registry = registry.clone();
                tokio::spawn(async move {
                    read_log_stream(
                        stderr,
                        runtime_id,
                        recording_log_handle.task_id,
                        recording_log_handle.attempt_no,
                        runtime_lease_token(&recording_log_handle).unwrap_or_default(),
                        "recording_stderr".to_string(),
                        registry,
                        events,
                    )
                    .await;
                });
            }

            spawn_companion_process_monitor(
                runtime_id,
                handle.task_id,
                handle.attempt_no,
                companion_pid,
                companion_plan,
                work_dir.clone(),
                success_check.clone(),
                registry.clone(),
                runtimes.clone(),
                events.clone(),
                companion_child,
            );
        }
        Err(error) => {
            let message = format!("failed to start stream_ingest mp4 recording sidecar: {error}");
            let updated_handle = registry
                .update(runtime_id, |runtime| {
                    update_companion_recording_metadata(runtime, |companion| {
                        companion.pid = None;
                        companion.state = CompanionProcessState::Failed;
                        companion.error = Some(message.clone());
                    });
                })
                .unwrap_or_else(|| handle.clone());
            let _ = hooks.persist_runtime_state(&work_dir, &updated_handle, &success_check);
            let _ = events.send(RuntimeNotification::TaskSnapshot(updated_handle.clone()));
            let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                task_id: updated_handle.task_id,
                attempt_no: updated_handle.attempt_no,
                lease_token: runtime_lease_token(&updated_handle).unwrap_or_default(),
                session_epoch: runtime_session_epoch(&updated_handle),
                event_type: "recording_degraded".to_string(),
                event_level: "warn".to_string(),
                message: "mp4 recording sidecar failed to start; continuing without recording"
                    .to_string(),
                payload: json!({
                    "output_target": companion_plan.output_target,
                    "reason": "recording_sidecar_start_failed",
                    "error": error.to_string(),
                }),
            }));
        }
    }

    Ok(())
}
