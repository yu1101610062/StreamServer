//! Runtime manager worker factory：负责把启停、录制控制、ZLM 启动和进程恢复串接起来。
//!
//! 生产控制入口只通过 `RuntimeManagerHandle` 提交状态；这里的 executor 只作为
//! RuntimeManager actor 派发慢副作用 worker 的工厂。

use std::{
    collections::HashMap,
    io,
    sync::{Arc, RwLock},
    time::Duration,
};

use chrono::Utc;
use media_domain::{RuntimeHandle, RuntimeState, TaskType};
use reqwest::Client;
use serde_json::json;
use tracing::warn;
use uuid::Uuid;

use crate::{
    config::AgentSettings,
    runtime_adoption::{
        RuntimeAdoptionCommit, RuntimeAdoptionMonitor, RuntimeAdoptionOutcome,
        RuntimeAdoptionWorkerContext, adopted_event_notification,
        prepare_adopt_orphan_runtimes_for_manager,
    },
    runtime_artifacts::attach_file_artifact_metadata,
    runtime_controls::{
        RuntimeControlContext, RuntimeRecordingOutcome, RuntimeRecordingPreparation,
        RuntimeRecordingWorkerRequest, prepare_runtime_recording_for_manager,
        run_runtime_recording_worker,
    },
    runtime_events::{
        RuntimeEventSink, RuntimeNotification, RuntimeTaskEvent, runtime_session_epoch,
    },
    runtime_manager::{
        ProcessExitedEvent, RuntimeBackendStore, RuntimeMonitorCommit, RuntimeMonitorHandle,
    },
    runtime_metadata::{
        attach_zlm_server_id, completion_reason_from_handle, continuous_stream_ingest_from_handle,
        fatal_recording_error_from_handle, mark_source_reconnecting, requires_stream_online,
        runtime_lease_token, should_emit_recording_gap_started,
        sticky_reconnect_stream_ingest_from_handle, stop_reason_from_handle, stream_online,
        task_runtime_mode_from_handle, task_type_from_handle,
    },
    runtime_monitors::{
        spawn_live_relay_monitor, spawn_rtp_receive_monitor, spawn_startup_probe_monitor,
    },
    runtime_persistence::persist_runtime_state,
    runtime_plan::TaskRuntimeMode,
    runtime_process::{ManagedRuntime, RuntimeSlotPermit},
    runtime_process_monitors::{
        spawn_adopted_companion_process_monitor, spawn_adopted_runtime_monitor,
    },
    runtime_process_start::{
        ManagedProcessStartContext, ManagedProcessStartHooks,
        RuntimeStartOutcome as ManagedProcessStartOutcome, prepare_process_start_task,
    },
    runtime_recovery::{
        ProcessRecoveryContext, cleanup_managed_stream_before_restart_notifications,
        should_auto_restart_process,
    },
    runtime_registry::AdoptFilter,
    runtime_start::{RuntimeStartContext, RuntimeStartDecision, prepare_start_task},
    runtime_stop::{
        RuntimeStopOutcome, RuntimeStopPreparation, RuntimeStopWorkerRequest,
        prepare_runtime_stop_for_manager, run_runtime_stop_worker,
    },
    runtime_types::{
        ExecutorError, RuntimeCapabilityHints, StartTaskRequest, StopTaskRequest, SuccessCheck,
        TaskRecordingControlRequest,
    },
    runtime_zlm::{zlm_rtp_server_port, zlm_stream_online},
    runtime_zlm_start::{
        RuntimeZlmStartContext, RuntimeZlmStartHooks, RuntimeZlmStartOutcome,
        start_live_relay_task as prepare_zlm_live_relay_start_task,
        start_rtp_receive_task as prepare_zlm_rtp_receive_start_task,
    },
};

pub(crate) enum RuntimeStartWorkerResult {
    PendingCommit(RuntimeStartOutcome),
}

pub(crate) enum RuntimeProcessExitOutcome {
    Terminal(RuntimeMonitorCommit),
    Restarted {
        exit_commit: RuntimeMonitorCommit,
        restart: RuntimeStartWorkerResult,
        emit_starting_event: bool,
    },
}

pub(crate) enum RuntimeStartOutcome {
    ManagedProcess(ManagedProcessStartOutcome),
    Zlm(RuntimeZlmStartOutcome),
}

impl RuntimeStartWorkerResult {
    pub(crate) fn runtime_id(&self) -> Option<Uuid> {
        match self {
            Self::PendingCommit(outcome) => Some(outcome.runtime_id()),
        }
    }

    pub(crate) fn backend(&self) -> Option<ManagedRuntime> {
        match self {
            Self::PendingCommit(outcome) => Some(outcome.backend()),
        }
    }

    pub(crate) async fn commit(
        self,
        monitor_handle: RuntimeMonitorHandle,
    ) -> Result<RuntimeHandle, ExecutorError> {
        match self {
            Self::PendingCommit(outcome) => outcome.commit(monitor_handle).await,
        }
    }

    pub(crate) fn carry_reconnect_metadata_from(&mut self, exited_handle: &RuntimeHandle) {
        let Self::PendingCommit(outcome) = self;
        outcome.carry_reconnect_metadata_from(exited_handle);
    }
}

impl RuntimeStartOutcome {
    fn runtime_id(&self) -> Uuid {
        match self {
            Self::ManagedProcess(outcome) => outcome.runtime_id(),
            Self::Zlm(outcome) => outcome.runtime_id(),
        }
    }

    fn backend(&self) -> ManagedRuntime {
        match self {
            Self::ManagedProcess(outcome) => outcome.backend(),
            Self::Zlm(outcome) => outcome.backend(),
        }
    }

    async fn commit(
        self,
        monitor_handle: RuntimeMonitorHandle,
    ) -> Result<RuntimeHandle, ExecutorError> {
        match self {
            Self::ManagedProcess(outcome) => outcome.commit(monitor_handle),
            Self::Zlm(outcome) => outcome.commit(monitor_handle).await,
        }
    }

    fn carry_reconnect_metadata_from(&mut self, exited_handle: &RuntimeHandle) {
        if let Self::ManagedProcess(outcome) = self {
            outcome.carry_reconnect_metadata_from(exited_handle);
        }
    }
}

#[derive(Clone)]
pub(crate) struct ManagedProcessExecutor {
    pub(crate) settings: AgentSettings,
    events: RuntimeEventSink,
    backend_store: RuntimeBackendStore,
    stop_intents: Arc<RwLock<HashMap<(Uuid, i32), StopTaskRequest>>>,
    http_client: Client,
    zlm_server_id: Arc<RwLock<Option<String>>>,
    zlm_rtmp_enhanced_enabled: Arc<RwLock<Option<bool>>>,
    process_start_hooks: ManagedProcessStartHooks,
    zlm_start_hooks: RuntimeZlmStartHooks,
}

impl ManagedProcessExecutor {
    pub(crate) fn new(settings: AgentSettings, events: RuntimeEventSink) -> Self {
        let backend_store = RuntimeBackendStore::new(&settings);
        Self {
            settings,
            events,
            backend_store,
            stop_intents: Arc::new(RwLock::new(HashMap::new())),
            http_client: Client::builder()
                .timeout(Duration::from_secs(3))
                .build()
                .expect("failed to build runtime HTTP client"),
            zlm_server_id: Arc::new(RwLock::new(None)),
            zlm_rtmp_enhanced_enabled: Arc::new(RwLock::new(None)),
            process_start_hooks: ManagedProcessStartHooks::default(),
            zlm_start_hooks: RuntimeZlmStartHooks::default(),
        }
    }

    pub(crate) fn new_for_manager(settings: AgentSettings, events: RuntimeEventSink) -> Self {
        Self::new(settings, events)
    }

    pub(crate) fn backend_store(&self) -> RuntimeBackendStore {
        self.backend_store.clone()
    }

    pub(crate) fn prepare_start_mode_for_manager(
        &self,
        request: &StartTaskRequest,
    ) -> Result<TaskRuntimeMode, ExecutorError> {
        match prepare_start_task(
            RuntimeStartContext {
                _settings: &self.settings,
                stop_intents: &self.stop_intents,
            },
            request,
        )? {
            RuntimeStartDecision::Start { mode } => Ok(mode),
        }
    }

    pub(crate) fn acquire_runtime_slot_for_manager(
        &self,
    ) -> Result<Arc<RuntimeSlotPermit>, ExecutorError> {
        self.backend_store.try_acquire_slot()
    }

    fn current_zlm_server_id(&self) -> Option<String> {
        {
            let guard = self
                .zlm_server_id
                .read()
                .expect("zlm_server_id lock poisoned");
            guard.clone()
        }
    }

    fn current_zlm_rtmp_enhanced_enabled(&self) -> Option<bool> {
        {
            let guard = self
                .zlm_rtmp_enhanced_enabled
                .read()
                .expect("zlm_rtmp_enhanced_enabled lock poisoned");
            *guard
        }
    }

    pub(crate) fn set_zlm_server_id(&self, server_id: String) {
        let server_id = server_id.trim().to_string();
        let mut guard = self
            .zlm_server_id
            .write()
            .expect("zlm_server_id lock poisoned");
        if server_id.is_empty() {
            *guard = None;
        } else {
            *guard = Some(server_id);
        }
    }

    pub(crate) fn set_zlm_rtmp_enhanced_enabled(&self, enabled: Option<bool>) {
        let mut guard = self
            .zlm_rtmp_enhanced_enabled
            .write()
            .expect("zlm_rtmp_enhanced_enabled lock poisoned");
        *guard = enabled;
    }

    fn control_context(&self) -> RuntimeControlContext<'_> {
        RuntimeControlContext {
            settings: &self.settings,
            http_client: &self.http_client,
        }
    }

    fn zlm_start_context(&self) -> RuntimeZlmStartContext<'_> {
        RuntimeZlmStartContext {
            settings: &self.settings,
            http_client: &self.http_client,
            events: &self.events,
            zlm_server_id: self.current_zlm_server_id(),
            hooks: self.zlm_start_hooks.clone(),
        }
    }

    fn process_recovery_context(&self) -> ProcessRecoveryContext<'_> {
        ProcessRecoveryContext {
            settings: &self.settings,
            http_client: &self.http_client,
        }
    }

    pub(crate) fn apply_monitor_commit(&self, commit: RuntimeMonitorCommit) {
        if let Some(persist) = &commit.persist {
            if let Err(error) =
                persist_runtime_state(&persist.work_dir, &commit.handle, &persist.success_check)
            {
                warn!(
                    runtime_id = %commit.runtime_id,
                    error = %error,
                    "failed to persist runtime monitor commit"
                );
            }
        }
        for notification in commit.notifications {
            let _ = self.events.send(notification);
        }
    }

    pub(crate) fn prepare_stop_for_manager(
        &self,
        request: &StopTaskRequest,
        handle: &RuntimeHandle,
        runtime: Option<ManagedRuntime>,
        generation: crate::runtime_manager::RuntimeGeneration,
        monitor_handle: RuntimeMonitorHandle,
    ) -> Result<RuntimeStopPreparation, ExecutorError> {
        prepare_runtime_stop_for_manager(
            &self.settings,
            &self.stop_intents,
            runtime,
            request,
            handle,
            generation,
            monitor_handle,
        )
    }

    pub(crate) async fn run_stop_worker_for_manager(
        &self,
        worker: RuntimeStopWorkerRequest,
    ) -> Result<RuntimeStopOutcome, ExecutorError> {
        let controls = self.control_context();
        run_runtime_stop_worker(controls, worker).await
    }

    pub(crate) fn prepare_recording_for_manager(
        &self,
        request: &TaskRecordingControlRequest,
        handle: &RuntimeHandle,
        generation: crate::runtime_manager::RuntimeGeneration,
        monitor_handle: RuntimeMonitorHandle,
    ) -> Result<RuntimeRecordingPreparation, ExecutorError> {
        prepare_runtime_recording_for_manager(
            &self.control_context(),
            request,
            handle,
            generation,
            monitor_handle,
        )
    }

    pub(crate) async fn run_recording_worker_for_manager(
        &self,
        worker: RuntimeRecordingWorkerRequest,
    ) -> Result<RuntimeRecordingOutcome, ExecutorError> {
        let controls = self.control_context();
        run_runtime_recording_worker(controls, worker).await
    }

    pub(crate) async fn handle_process_exited(
        &self,
        event: ProcessExitedEvent,
        current_handle: RuntimeHandle,
        restart_slot_permit: Option<Arc<RuntimeSlotPermit>>,
    ) -> RuntimeProcessExitOutcome {
        let mut exited_handle = current_handle;
        exited_handle.state = RuntimeState::Exited;
        exited_handle.last_progress_at = Some(Utc::now());
        attach_file_artifact_metadata(&mut exited_handle, &event.success_check);
        let mut pre_terminal_notifications = Vec::new();

        let restart_status = event
            .status
            .as_ref()
            .map(|status| *status)
            .map_err(|error| io::Error::new(io::ErrorKind::Other, error.clone()));
        // 进程退出后先判断是否能本地重启。可恢复路径会先写一次退出 commit，
        // 再把新的 start outcome 交回 actor；真正终态只在恢复条件不满足时生成。
        if should_auto_restart_process(&exited_handle, event.was_stopped, &restart_status) {
            let sticky_reconnect = sticky_reconnect_stream_ingest_from_handle(&exited_handle);
            let restart_reason = if stream_online(&exited_handle) {
                "source_disconnected"
            } else {
                "source_unavailable"
            };
            let emit_gap_started = should_emit_recording_gap_started(&exited_handle);
            if sticky_reconnect {
                mark_source_reconnecting(&mut exited_handle, restart_reason);
            }
            let mut recovery_notifications = Vec::new();
            if sticky_reconnect {
                recovery_notifications.push(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                    task_id: exited_handle.task_id,
                    attempt_no: exited_handle.attempt_no,
                    lease_token: runtime_lease_token(&exited_handle).unwrap_or_default(),
                    session_epoch: runtime_session_epoch(&exited_handle),
                    event_type: "source_reconnecting".to_string(),
                    event_level: "warn".to_string(),
                    message: "managed stream_ingest process exited; restarting locally".to_string(),
                    payload: json!({
                        "runtime_id": exited_handle.runtime_id,
                        "exit_code": event.status.as_ref().ok().and_then(|value| value.code()),
                        "output_target": event.output_target,
                        "task_type": task_type_from_handle(&exited_handle),
                        "reason": restart_reason,
                    }),
                }));
                if emit_gap_started {
                    recovery_notifications.push(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                        task_id: exited_handle.task_id,
                        attempt_no: exited_handle.attempt_no,
                        lease_token: runtime_lease_token(&exited_handle).unwrap_or_default(),
                        session_epoch: runtime_session_epoch(&exited_handle),
                        event_type: "recording_gap_started".to_string(),
                        event_level: "warn".to_string(),
                        message: "stream recording gap started while source reconnects".to_string(),
                        payload: json!({
                            "runtime_id": exited_handle.runtime_id,
                            "exit_code": event.status.as_ref().ok().and_then(|value| value.code()),
                            "output_target": event.output_target,
                            "task_type": task_type_from_handle(&exited_handle),
                            "reason": restart_reason,
                            "recording_gap_started_at": exited_handle
                                .metadata
                                .get("recording_gap_started_at")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null),
                        }),
                    }));
                }
            } else {
                recovery_notifications.push(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
                    task_id: exited_handle.task_id,
                    attempt_no: exited_handle.attempt_no,
                    lease_token: runtime_lease_token(&exited_handle).unwrap_or_default(),
                    session_epoch: runtime_session_epoch(&exited_handle),
                    event_type: "recovering".to_string(),
                    event_level: "warn".to_string(),
                    message:
                        "managed process exited after stream was online; attempting local recovery"
                            .to_string(),
                    payload: json!({
                        "exit_code": event.status.as_ref().ok().and_then(|value| value.code()),
                        "output_target": event.output_target,
                        "task_type": task_type_from_handle(&exited_handle),
                    }),
                }));
            }
            recovery_notifications.extend(
                cleanup_managed_stream_before_restart_notifications(
                    self.process_recovery_context(),
                    &exited_handle,
                )
                .await,
            );

            if let Some(slot_permit) = restart_slot_permit {
                if let Ok(restart) = self
                    .restart_process_task_after_failure_for_manager(&exited_handle, slot_permit)
                    .await
                {
                    let exit_commit =
                        RuntimeMonitorCommit::new(exited_handle.clone(), event.generation)
                            .with_persist(event.work_dir, event.success_check)
                            .with_notifications(recovery_notifications)
                            .terminal();
                    return RuntimeProcessExitOutcome::Restarted {
                        exit_commit,
                        restart,
                        emit_starting_event: !sticky_reconnect,
                    };
                }
            }
            pre_terminal_notifications = recovery_notifications;
        }

        let completion_reason = completion_reason_from_handle(&exited_handle);
        let stop_reason = stop_reason_from_handle(&exited_handle);
        let fatal_recording_error = fatal_recording_error_from_handle(&exited_handle);
        // 不能重启时才把进程退出折算成 task event。这里按“用户主动停止、
        // 录制时长到达、磁盘保护、产物校验、异常退出”顺序裁决，避免成功 exit
        // 被误报为任务成功。
        let (event_type, event_level, message, payload) = match &event.status {
            Ok(status)
                if event.was_stopped
                    && completion_reason.as_deref() == Some("record_duration_reached") =>
            {
                (
                    "succeeded",
                    "info",
                    "child process completed after recording duration reached".to_string(),
                    json!({
                        "exit_code": status.code(),
                        "output_target": event.output_target,
                        "reason": "record_duration_reached",
                    }),
                )
            }
            Ok(status)
                if event.was_stopped
                    && stop_reason.as_deref() == Some("disk_threshold_exceeded") =>
            {
                (
                    "failed",
                    "error",
                    "child process stopped after disk threshold was exceeded".to_string(),
                    json!({
                        "exit_code": status.code(),
                        "output_target": event.output_target,
                        "reason": "disk_threshold_exceeded",
                    }),
                )
            }
            Ok(status) if event.was_stopped => (
                "canceled",
                "info",
                "child process stopped".to_string(),
                json!({
                    "exit_code": status.code(),
                    "output_target": event.output_target,
                    "reason": stop_reason,
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
                    "output_target": event.output_target,
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
                        "output_target": event.output_target,
                    }),
                )
            }
            Ok(status)
                if status.success()
                    && task_type_from_handle(&exited_handle) == Some(TaskType::StreamIngest)
                    && task_runtime_mode_from_handle(&exited_handle)
                        == Some(TaskRuntimeMode::ManagedProcess)
                    && continuous_stream_ingest_from_handle(&exited_handle) =>
            {
                (
                    "failed",
                    "error",
                    "continuous stream_ingest process exited unexpectedly".to_string(),
                    json!({
                        "exit_code": status.code(),
                        "output_target": event.output_target,
                        "reason": "unexpected_stream_exit",
                    }),
                )
            }
            Ok(status) if status.success() => match &event.success_check {
                SuccessCheck::FileExists(path) if path.exists() => (
                    "succeeded",
                    "info",
                    "child process completed".to_string(),
                    json!({
                        "exit_code": status.code(),
                        "output_target": event.output_target,
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
                        "output_target": event.output_target,
                    }),
                ),
                SuccessCheck::FilesExist(paths) if paths.iter().all(|path| path.exists()) => (
                    "succeeded",
                    "info",
                    "child process completed".to_string(),
                    json!({
                        "exit_code": status.code(),
                        "output_target": event.output_target,
                    }),
                ),
                SuccessCheck::FilesExist(paths) => {
                    let missing = paths
                        .iter()
                        .filter(|path| !path.exists())
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>();
                    (
                        "failed",
                        "error",
                        format!(
                            "child process finished without artifacts: {}",
                            missing.join(", ")
                        ),
                        json!({
                            "exit_code": status.code(),
                            "output_target": event.output_target,
                            "missing_outputs": missing,
                        }),
                    )
                }
                SuccessCheck::ProcessExit => (
                    "succeeded",
                    "info",
                    "child process completed".to_string(),
                    json!({
                        "exit_code": status.code(),
                        "output_target": event.output_target,
                    }),
                ),
            },
            Ok(status) => (
                "failed",
                "error",
                format!("child process exited unsuccessfully: {:?}", status.code()),
                json!({
                    "exit_code": status.code(),
                    "output_target": event.output_target,
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
                    "output_target": event.output_target,
                    "recording_error": fatal_recording_error,
                    "wait_error": error,
                }),
            ),
            Err(error)
                if event.was_stopped
                    && stop_reason.as_deref() == Some("disk_threshold_exceeded") =>
            {
                (
                    "failed",
                    "error",
                    format!("failed to wait child process after disk threshold stop: {error}"),
                    json!({
                        "output_target": event.output_target,
                        "reason": "disk_threshold_exceeded",
                        "wait_error": error,
                    }),
                )
            }
            Err(error) => (
                "failed",
                "error",
                format!("failed to wait child process: {error}"),
                json!({
                    "output_target": event.output_target,
                }),
            ),
        };

        pre_terminal_notifications.push(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
            task_id: exited_handle.task_id,
            attempt_no: exited_handle.attempt_no,
            lease_token: runtime_lease_token(&exited_handle).unwrap_or_default(),
            session_epoch: runtime_session_epoch(&exited_handle),
            event_type: event_type.to_string(),
            event_level: event_level.to_string(),
            message,
            payload,
        }));
        pre_terminal_notifications.push(RuntimeNotification::TaskSnapshot(exited_handle.clone()));
        // 终态 commit 是进程监控的最后一次写入，持久化、通知和 manager 状态收口
        // 必须在同一个 RuntimeMonitorCommit 中完成。
        RuntimeProcessExitOutcome::Terminal(
            RuntimeMonitorCommit::new(exited_handle, event.generation)
                .with_persist(event.work_dir, event.success_check)
                .with_notifications(pre_terminal_notifications)
                .terminal(),
        )
    }
}

impl ManagedProcessExecutor {
    pub(crate) async fn start_task_for_manager(
        &self,
        request: StartTaskRequest,
        mode: TaskRuntimeMode,
        slot_permit: Arc<RuntimeSlotPermit>,
    ) -> Result<RuntimeStartWorkerResult, ExecutorError> {
        match mode {
            TaskRuntimeMode::ZlmProxy => self
                .start_live_relay_task(&request, slot_permit)
                .await
                .map(RuntimeStartOutcome::Zlm)
                .map(RuntimeStartWorkerResult::PendingCommit),
            TaskRuntimeMode::ZlmRtpServer => self
                .start_rtp_receive_task(&request, slot_permit)
                .await
                .map(RuntimeStartOutcome::Zlm)
                .map(RuntimeStartWorkerResult::PendingCommit),
            TaskRuntimeMode::ManagedProcess => self
                .prepare_process_task_for_manager(&request, slot_permit)
                .map(RuntimeStartOutcome::ManagedProcess)
                .map(RuntimeStartWorkerResult::PendingCommit),
        }
    }

    pub(crate) async fn prepare_adopt_orphans_for_manager(
        &self,
        filter: AdoptFilter,
    ) -> Vec<RuntimeAdoptionOutcome<RuntimeStartWorkerResult>> {
        let stream_probe_client = self.http_client.clone();
        let stream_probe_settings = self.settings.clone();
        let rtp_probe_client = self.http_client.clone();
        let rtp_probe_settings = self.settings.clone();

        prepare_adopt_orphan_runtimes_for_manager(
            RuntimeAdoptionWorkerContext {
                filter,
                zlm_server_id: self.current_zlm_server_id(),
                settings: self.settings.clone(),
            },
            |request| async move {
                let mode = self.prepare_start_mode_for_manager(&request).ok()?;
                let slot_permit = self.acquire_runtime_slot_for_manager().ok()?;
                self.start_task_for_manager(request, mode, slot_permit)
                    .await
                    .ok()
            },
            move |startup_probe| {
                let client = stream_probe_client.clone();
                let settings = stream_probe_settings.clone();
                async move {
                    zlm_stream_online(&client, &settings, &startup_probe)
                        .await
                        .map_err(|error| ExecutorError::ApiCall(error.to_string()))
                        .unwrap_or(false)
                }
            },
            move |stream_id| {
                let client = rtp_probe_client.clone();
                let settings = rtp_probe_settings.clone();
                async move {
                    zlm_rtp_server_port(&client, &settings, &stream_id)
                        .await
                        .ok()
                        .flatten()
                }
            },
        )
        .await
    }

    pub(crate) fn prepare_existing_adoption_commit(
        &self,
        handle: &RuntimeHandle,
        session_epoch: u64,
        generation: crate::runtime_manager::RuntimeGeneration,
    ) -> RuntimeMonitorCommit {
        let mut updated = handle.clone();
        updated.metadata["session_epoch"] = json!(session_epoch);
        attach_zlm_server_id(
            &mut updated.metadata,
            self.current_zlm_server_id().as_deref(),
        );
        RuntimeMonitorCommit::new(updated.clone(), generation).with_notifications(vec![
            adopted_event_notification(
                &updated,
                "reattached active runtime after control-plane reconnect",
                json!({
                    "runtime_id": updated.runtime_id,
                    "orphaned": false,
                }),
            ),
        ])
    }

    pub(crate) fn apply_adoption_commit(
        &self,
        commit: RuntimeAdoptionCommit,
        _generation: crate::runtime_manager::RuntimeGeneration,
        monitor_handle: RuntimeMonitorHandle,
    ) -> RuntimeHandle {
        let handle = commit.handle.clone();
        let runtime_id = handle.runtime_id;
        let process = commit.backend.process;
        if let Err(error) = persist_runtime_state(&commit.work_dir, &handle, &commit.success_check)
        {
            warn!(
                runtime_id = %runtime_id,
                error = %error,
                "failed to persist adopted runtime state"
            );
        }
        for notification in commit.notifications {
            let _ = self.events.send(notification);
        }

        for monitor in commit.monitors {
            match monitor {
                RuntimeAdoptionMonitor::StartupProbe { startup_probe } => {
                    spawn_startup_probe_monitor(
                        commit.work_dir.clone(),
                        commit.success_check.clone(),
                        startup_probe,
                        self.settings.clone(),
                        self.http_client.clone(),
                        self.events.clone(),
                        monitor_handle.clone(),
                    );
                }
                RuntimeAdoptionMonitor::AdoptedRuntime => {
                    spawn_adopted_runtime_monitor(
                        process,
                        commit.work_dir.clone(),
                        commit.success_check.clone(),
                        monitor_handle.clone(),
                    );
                }
                RuntimeAdoptionMonitor::AdoptedCompanion { process, companion } => {
                    spawn_adopted_companion_process_monitor(
                        runtime_id,
                        process,
                        companion,
                        commit.work_dir.clone(),
                        commit.success_check.clone(),
                        monitor_handle.clone(),
                    );
                }
                RuntimeAdoptionMonitor::LiveRelay { startup_probe } => {
                    spawn_live_relay_monitor(
                        commit.work_dir.clone(),
                        startup_probe,
                        self.settings.clone(),
                        self.http_client.clone(),
                        self.events.clone(),
                        monitor_handle.clone(),
                    );
                }
                RuntimeAdoptionMonitor::RtpReceive { stream_id } => {
                    spawn_rtp_receive_monitor(
                        commit.work_dir.clone(),
                        stream_id,
                        self.settings.clone(),
                        self.http_client.clone(),
                        self.events.clone(),
                        monitor_handle.clone(),
                    );
                }
            }
        }
        handle
    }
}

impl ManagedProcessExecutor {
    fn prepare_process_task_for_manager(
        &self,
        request: &StartTaskRequest,
        slot_permit: Arc<RuntimeSlotPermit>,
    ) -> Result<ManagedProcessStartOutcome, ExecutorError> {
        prepare_process_start_task(
            ManagedProcessStartContext {
                settings: &self.settings,
                http_client: &self.http_client,
                events: &self.events,
                zlm_server_id: self.current_zlm_server_id(),
                capability_hints: RuntimeCapabilityHints {
                    zlm_rtmp_enhanced_enabled: self.current_zlm_rtmp_enhanced_enabled(),
                },
                hooks: self.process_start_hooks.clone(),
            },
            request,
            slot_permit,
        )
    }

    pub(crate) async fn restart_process_task_after_failure_for_manager(
        &self,
        exited_handle: &RuntimeHandle,
        slot_permit: Arc<RuntimeSlotPermit>,
    ) -> Result<RuntimeStartWorkerResult, ExecutorError> {
        crate::runtime_zlm::wait_for_zlm_api_ready(
            &self.http_client,
            &self.settings,
            Duration::from_secs(15),
        )
        .await;

        let request = crate::runtime_metadata::restart_request_from_handle(exited_handle)?;
        self.prepare_process_task_for_manager(&request, slot_permit)
            .map(RuntimeStartOutcome::ManagedProcess)
            .map(RuntimeStartWorkerResult::PendingCommit)
    }

    async fn start_live_relay_task(
        &self,
        request: &StartTaskRequest,
        slot_permit: Arc<RuntimeSlotPermit>,
    ) -> Result<RuntimeZlmStartOutcome, ExecutorError> {
        prepare_zlm_live_relay_start_task(self.zlm_start_context(), request, slot_permit).await
    }

    async fn start_rtp_receive_task(
        &self,
        request: &StartTaskRequest,
        slot_permit: Arc<RuntimeSlotPermit>,
    ) -> Result<RuntimeZlmStartOutcome, ExecutorError> {
        prepare_zlm_rtp_receive_start_task(self.zlm_start_context(), request, slot_permit).await
    }
}
