//! Runtime 执行器门面：负责把启停、录制控制、ZLM 启动和进程恢复串接起来。
//!
//! 具体的 FFmpeg 参数、ZLM API、持久化和监控逻辑分散在相邻 runtime_* 模块中，这里只保留
//! `LocalExecutor` 对外契约和 `ManagedProcessExecutor` 的运行上下文组装。

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex as StdMutex, RwLock},
    time::Duration,
};

use media_domain::RuntimeHandle;
use reqwest::Client;
use tonic::async_trait;
use uuid::Uuid;

use crate::{
    config::AgentSettings,
    runtime_adoption::{RuntimeAdoptionContext, adopt_orphan_runtimes},
    runtime_controls::{
        RuntimeControlContext, RuntimeRecordingControlContext,
        set_task_recording as set_runtime_task_recording,
    },
    runtime_events::RuntimeEventSink,
    runtime_plan::TaskRuntimeMode,
    runtime_process::{ManagedRuntime, RuntimeSlotLimiter, RuntimeSlotPermit},
    runtime_process_start::{
        ManagedProcessStartContext, ManagedProcessStartHooks,
        start_process_task as start_managed_process_task,
    },
    runtime_recovery::{
        ProcessRecoveryContext,
        cleanup_managed_stream_before_restart as cleanup_managed_stream_before_restart_impl,
        restart_process_task_after_failure as restart_process_task_after_failure_impl,
    },
    runtime_registry::{AdoptFilter, LocalRuntimeRegistry},
    runtime_start::{RuntimeStartContext, RuntimeStartDecision, prepare_start_task},
    runtime_stop::{RuntimeStopContext, stop_runtime_task},
    runtime_types::{
        ExecutorError, RuntimeCapabilityHints, StartTaskRequest, StopTaskRequest,
        TaskRecordingControlRequest,
    },
    runtime_zlm::{zlm_rtp_server_port, zlm_stream_online},
    runtime_zlm_start::{
        RuntimeZlmStartContext, RuntimeZlmStartHooks,
        start_live_relay_task as start_zlm_live_relay_task,
        start_rtp_receive_task as start_zlm_rtp_receive_task,
    },
};

#[async_trait]
pub trait LocalExecutor: Send + Sync {
    async fn start_task(&self, request: StartTaskRequest) -> Result<RuntimeHandle, ExecutorError>;
    async fn stop_task(&self, request: StopTaskRequest) -> Result<(), ExecutorError>;
    async fn set_task_recording(
        &self,
        request: TaskRecordingControlRequest,
    ) -> Result<RuntimeHandle, ExecutorError>;
    async fn adopt_orphans(&self, filter: AdoptFilter) -> Vec<RuntimeHandle>;
    fn set_zlm_server_id(&self, _server_id: String) {}
    fn set_zlm_rtmp_enhanced_enabled(&self, _enabled: Option<bool>) {}
}

#[derive(Debug, Clone)]
pub struct ManagedProcessExecutor {
    pub(crate) settings: AgentSettings,
    pub(crate) registry: LocalRuntimeRegistry,
    events: RuntimeEventSink,
    pub(crate) runtimes: Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    pub(crate) slot_limiter: Arc<RuntimeSlotLimiter>,
    stop_intents: Arc<RwLock<HashMap<(Uuid, i32), StopTaskRequest>>>,
    recording_controls: Arc<StdMutex<HashSet<Uuid>>>,
    http_client: Client,
    zlm_server_id: Arc<RwLock<Option<String>>>,
    zlm_rtmp_enhanced_enabled: Arc<RwLock<Option<bool>>>,
}

impl ManagedProcessExecutor {
    pub fn new(
        settings: AgentSettings,
        registry: LocalRuntimeRegistry,
        events: RuntimeEventSink,
    ) -> Self {
        let max_runtime_slots = settings.max_runtime_slots;
        Self {
            settings,
            registry,
            events,
            runtimes: Arc::new(RwLock::new(HashMap::new())),
            slot_limiter: Arc::new(RuntimeSlotLimiter::new(max_runtime_slots)),
            stop_intents: Arc::new(RwLock::new(HashMap::new())),
            recording_controls: Arc::new(StdMutex::new(HashSet::new())),
            http_client: Client::builder()
                .timeout(Duration::from_secs(3))
                .build()
                .expect("failed to build runtime HTTP client"),
            zlm_server_id: Arc::new(RwLock::new(None)),
            zlm_rtmp_enhanced_enabled: Arc::new(RwLock::new(None)),
        }
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

    fn control_context(&self) -> RuntimeControlContext<'_> {
        RuntimeControlContext {
            settings: &self.settings,
            http_client: &self.http_client,
            registry: &self.registry,
            runtimes: &self.runtimes,
            events: &self.events,
        }
    }

    fn zlm_start_context(&self) -> RuntimeZlmStartContext<'_> {
        RuntimeZlmStartContext {
            settings: &self.settings,
            http_client: &self.http_client,
            registry: &self.registry,
            runtimes: &self.runtimes,
            events: &self.events,
            zlm_server_id: self.current_zlm_server_id(),
            hooks: RuntimeZlmStartHooks::default(),
        }
    }

    fn process_recovery_context(&self) -> ProcessRecoveryContext<'_> {
        ProcessRecoveryContext {
            settings: &self.settings,
            http_client: &self.http_client,
            registry: &self.registry,
            runtimes: &self.runtimes,
            events: &self.events,
            slot_limiter: &self.slot_limiter,
            zlm_server_id: self.current_zlm_server_id(),
            capability_hints: RuntimeCapabilityHints {
                zlm_rtmp_enhanced_enabled: self.current_zlm_rtmp_enhanced_enabled(),
            },
            restart_executor: self.clone(),
        }
    }
}

#[async_trait]
impl LocalExecutor for ManagedProcessExecutor {
    async fn start_task(&self, request: StartTaskRequest) -> Result<RuntimeHandle, ExecutorError> {
        match prepare_start_task(
            RuntimeStartContext {
                settings: &self.settings,
                registry: &self.registry,
                runtimes: &self.runtimes,
                stop_intents: &self.stop_intents,
                slot_limiter: &self.slot_limiter,
            },
            &request,
        )? {
            RuntimeStartDecision::Existing(handle) => Ok(handle),
            RuntimeStartDecision::Start { mode, slot_permit } => match mode {
                TaskRuntimeMode::ZlmProxy => {
                    self.start_live_relay_task(&request, slot_permit).await
                }
                TaskRuntimeMode::ZlmRtpServer => {
                    self.start_rtp_receive_task(&request, slot_permit).await
                }
                TaskRuntimeMode::ManagedProcess => self.start_process_task(&request, slot_permit),
            },
        }
    }

    async fn stop_task(&self, request: StopTaskRequest) -> Result<(), ExecutorError> {
        let controls = self.control_context();
        stop_runtime_task(
            RuntimeStopContext {
                settings: &self.settings,
                registry: &self.registry,
                runtimes: &self.runtimes,
                events: &self.events,
                stop_intents: &self.stop_intents,
                controls,
            },
            &request,
        )
        .await
    }

    async fn set_task_recording(
        &self,
        request: TaskRecordingControlRequest,
    ) -> Result<RuntimeHandle, ExecutorError> {
        let controls = self.control_context();
        set_runtime_task_recording(
            RuntimeRecordingControlContext {
                controls,
                recording_controls: self.recording_controls.clone(),
            },
            &request,
        )
        .await
    }

    async fn adopt_orphans(&self, filter: AdoptFilter) -> Vec<RuntimeHandle> {
        let stream_probe_client = self.http_client.clone();
        let stream_probe_settings = self.settings.clone();
        let rtp_probe_client = self.http_client.clone();
        let rtp_probe_settings = self.settings.clone();

        adopt_orphan_runtimes(
            RuntimeAdoptionContext {
                filter,
                zlm_server_id: self.current_zlm_server_id(),
                settings: self.settings.clone(),
                http_client: self.http_client.clone(),
                registry: self.registry.clone(),
                runtimes: self.runtimes.clone(),
                slot_limiter: self.slot_limiter.clone(),
                events: self.events.clone(),
            },
            |request| async move { self.start_task(request).await.ok() },
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

    fn set_zlm_server_id(&self, server_id: String) {
        let server_id = server_id.trim().to_string();
        {
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
    }

    fn set_zlm_rtmp_enhanced_enabled(&self, enabled: Option<bool>) {
        {
            let mut guard = self
                .zlm_rtmp_enhanced_enabled
                .write()
                .expect("zlm_rtmp_enhanced_enabled lock poisoned");
            *guard = enabled;
        }
    }
}

impl ManagedProcessExecutor {
    fn start_process_task(
        &self,
        request: &StartTaskRequest,
        slot_permit: Arc<RuntimeSlotPermit>,
    ) -> Result<RuntimeHandle, ExecutorError> {
        start_managed_process_task(
            ManagedProcessStartContext {
                settings: &self.settings,
                http_client: &self.http_client,
                registry: &self.registry,
                runtimes: &self.runtimes,
                events: &self.events,
                zlm_server_id: self.current_zlm_server_id(),
                capability_hints: RuntimeCapabilityHints {
                    zlm_rtmp_enhanced_enabled: self.current_zlm_rtmp_enhanced_enabled(),
                },
                restart_executor: self.clone(),
                hooks: ManagedProcessStartHooks::default(),
            },
            request,
            slot_permit,
        )
    }

    pub(crate) async fn restart_process_task_after_failure(
        &self,
        exited_handle: &RuntimeHandle,
        emit_starting_event: bool,
    ) -> Result<RuntimeHandle, ExecutorError> {
        restart_process_task_after_failure_impl(
            self.process_recovery_context(),
            exited_handle,
            emit_starting_event,
        )
        .await
    }

    pub(crate) async fn cleanup_managed_stream_before_restart(&self, handle: &RuntimeHandle) {
        cleanup_managed_stream_before_restart_impl(self.process_recovery_context(), handle).await;
    }

    async fn start_live_relay_task(
        &self,
        request: &StartTaskRequest,
        slot_permit: Arc<RuntimeSlotPermit>,
    ) -> Result<RuntimeHandle, ExecutorError> {
        start_zlm_live_relay_task(self.zlm_start_context(), request, slot_permit).await
    }

    async fn start_rtp_receive_task(
        &self,
        request: &StartTaskRequest,
        slot_permit: Arc<RuntimeSlotPermit>,
    ) -> Result<RuntimeHandle, ExecutorError> {
        start_zlm_rtp_receive_task(self.zlm_start_context(), request, slot_permit).await
    }
}
