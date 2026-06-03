//! Runtime ZLM 启动：创建 ZLM proxy/RTP server runtime 并登记本地状态。
//!
//! 这个模块只承接 ZLM 托管 runtime 的启动流程，包括调用 ZLM API、构造 runtime
//! metadata、写入持久化状态、占用 runtime slot，以及启动后续在线/存活监控。

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, RwLock, atomic::AtomicBool},
};

use chrono::Utc;
use media_domain::{RuntimeHandle, RuntimeState, WorkerKind};
use reqwest::Client;
use serde_json::json;
use tracing::warn;
use uuid::Uuid;

use crate::{
    config::AgentSettings,
    runtime::{ExecutorError, StartTaskRequest, SuccessCheck},
    runtime_events::{
        RuntimeEventSink, RuntimeNotification, RuntimeTaskEvent, runtime_session_epoch,
    },
    runtime_metadata::{
        RtpServerMetadata, StreamBinding, attach_zlm_server_id, runtime_lease_token,
    },
    runtime_monitors::{spawn_live_relay_monitor, spawn_rtp_receive_monitor},
    runtime_persistence::{
        RUNTIME_COMMAND_FILE, RUNTIME_PID_FILE, RUNTIME_STATE_FILE,
        persist_runtime_state as persist_runtime_state_to_disk,
    },
    runtime_plan::{
        build_live_relay_api_params, build_live_relay_plan, build_open_rtp_server_params,
        build_rtp_receive_plan, parse_task_spec, prepare_work_dir,
    },
    runtime_process::{ManagedRuntime, RuntimeSlotPermit, remove_managed_runtime},
    runtime_registry::LocalRuntimeRegistry,
    runtime_zlm::{
        build_close_stream_params, call_zlm_api, close_zlm_rtp_server, extract_zlm_local_port,
        extract_zlm_proxy_key,
    },
};

type PersistRuntimeStateHook =
    dyn Fn(&Path, &RuntimeHandle, &SuccessCheck) -> Result<(), ExecutorError> + Send + Sync;
type AfterRuntimeInsertHook = dyn Fn(&RuntimeHandle) -> Result<(), ExecutorError> + Send + Sync;
type AfterRegistryTrackHook = dyn Fn(&RuntimeHandle) -> Result<(), ExecutorError> + Send + Sync;

#[derive(Clone)]
pub(crate) struct RuntimeZlmStartHooks {
    persist_runtime_state: Arc<PersistRuntimeStateHook>,
    after_runtime_insert: Arc<AfterRuntimeInsertHook>,
    after_registry_track: Arc<AfterRegistryTrackHook>,
}

impl Default for RuntimeZlmStartHooks {
    fn default() -> Self {
        Self {
            persist_runtime_state: Arc::new(persist_runtime_state_to_disk),
            after_runtime_insert: Arc::new(|_| Ok(())),
            after_registry_track: Arc::new(|_| Ok(())),
        }
    }
}

impl RuntimeZlmStartHooks {
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

    fn after_registry_track(&self, handle: &RuntimeHandle) -> Result<(), ExecutorError> {
        (self.after_registry_track)(handle)
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
    pub(crate) fn with_after_registry_track(
        after_registry_track: impl Fn(&RuntimeHandle) -> Result<(), ExecutorError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            after_registry_track: Arc::new(after_registry_track),
            ..Self::default()
        }
    }
}

enum ZlmStartResource {
    LiveRelay {
        proxy_key: Option<String>,
        binding: StreamBinding,
    },
    RtpServer {
        stream_id: String,
    },
}

struct ZlmStartRollback {
    runtime_id: Uuid,
    work_dir: PathBuf,
    registry: LocalRuntimeRegistry,
    runtimes: Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    resource: ZlmStartResource,
    armed: bool,
}

impl ZlmStartRollback {
    fn live_relay(
        runtime_id: Uuid,
        work_dir: PathBuf,
        registry: LocalRuntimeRegistry,
        runtimes: Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
        proxy_key: Option<String>,
        binding: StreamBinding,
    ) -> Self {
        Self {
            runtime_id,
            work_dir,
            registry,
            runtimes,
            resource: ZlmStartResource::LiveRelay { proxy_key, binding },
            armed: true,
        }
    }

    fn rtp_server(
        runtime_id: Uuid,
        work_dir: PathBuf,
        registry: LocalRuntimeRegistry,
        runtimes: Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
        stream_id: String,
    ) -> Self {
        Self {
            runtime_id,
            work_dir,
            registry,
            runtimes,
            resource: ZlmStartResource::RtpServer { stream_id },
            armed: true,
        }
    }

    async fn cleanup(&mut self, client: &Client, settings: &AgentSettings) {
        if !self.armed {
            return;
        }
        self.armed = false;
        let _ = remove_managed_runtime(&self.runtimes, self.runtime_id);
        let _ = self.registry.remove(self.runtime_id);
        self.cleanup_persisted_runtime_files();

        match &self.resource {
            ZlmStartResource::LiveRelay { proxy_key, binding } => {
                if let Some(proxy_key) = proxy_key {
                    if let Err(error) = call_zlm_api(
                        client,
                        settings,
                        "/index/api/delStreamProxy",
                        &[("key".to_string(), proxy_key.clone())],
                    )
                    .await
                    {
                        warn!(
                            runtime_id = %self.runtime_id,
                            error = %error,
                            "failed to rollback ZLM stream proxy"
                        );
                    }
                }
                if let Err(error) = call_zlm_api(
                    client,
                    settings,
                    "/index/api/close_streams",
                    &build_close_stream_params(binding, true),
                )
                .await
                {
                    warn!(
                        runtime_id = %self.runtime_id,
                        error = %error,
                        "failed to rollback ZLM live relay stream"
                    );
                }
            }
            ZlmStartResource::RtpServer { stream_id } => {
                if let Err(error) = close_zlm_rtp_server(client, settings, stream_id).await {
                    warn!(
                        runtime_id = %self.runtime_id,
                        stream_id = stream_id.as_str(),
                        error = %error,
                        "failed to rollback ZLM RTP server"
                    );
                }
            }
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }

    fn cleanup_persisted_runtime_files(&self) {
        for file_name in [RUNTIME_STATE_FILE, RUNTIME_PID_FILE, RUNTIME_COMMAND_FILE] {
            let _ = fs::remove_file(self.work_dir.join(file_name));
        }
    }
}

async fn rollback_zlm_start_error<T>(
    rollback: &mut ZlmStartRollback,
    client: &Client,
    settings: &AgentSettings,
    error: ExecutorError,
) -> Result<T, ExecutorError> {
    rollback.cleanup(client, settings).await;
    Err(error)
}

pub(crate) struct RuntimeZlmStartContext<'a> {
    pub(crate) settings: &'a AgentSettings,
    pub(crate) http_client: &'a Client,
    pub(crate) registry: &'a LocalRuntimeRegistry,
    pub(crate) runtimes: &'a Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    pub(crate) events: &'a RuntimeEventSink,
    pub(crate) zlm_server_id: Option<String>,
    pub(crate) hooks: RuntimeZlmStartHooks,
}

pub(crate) async fn start_live_relay_task(
    ctx: RuntimeZlmStartContext<'_>,
    request: &StartTaskRequest,
    slot_permit: Arc<RuntimeSlotPermit>,
) -> Result<RuntimeHandle, ExecutorError> {
    let spec = parse_task_spec(request)?;
    let plan = build_live_relay_plan(ctx.settings, request, &spec)?;
    prepare_work_dir(&plan.work_dir)?;

    let response = call_zlm_api(
        ctx.http_client,
        ctx.settings,
        "/index/api/addStreamProxy",
        &build_live_relay_api_params(ctx.settings, &spec, &plan.startup_probe, &plan.input_url),
    )
    .await?;
    let proxy_key = extract_zlm_proxy_key(&response);
    let runtime_id = Uuid::now_v7();
    let startup_probe = plan.startup_probe.clone();
    let stream_binding = StreamBinding {
        schema: startup_probe.schema.clone(),
        vhost: startup_probe.vhost.clone(),
        app: startup_probe.app.clone(),
        stream: startup_probe.stream.clone(),
    };
    let mut rollback = ZlmStartRollback::live_relay(
        runtime_id,
        plan.work_dir.clone(),
        ctx.registry.clone(),
        ctx.runtimes.clone(),
        proxy_key.clone(),
        stream_binding.clone(),
    );
    let stop_requested = Arc::new(AtomicBool::new(false));
    let mut metadata = json!({
        "task_type": request.task_type,
        "execution_mode": request.execution_mode,
        "lease_token": request.lease_token,
        "session_epoch": request.session_epoch,
        "trace_context": request.trace_context,
        "resolved_spec": request.resolved_spec,
        "work_dir": plan.work_dir,
        "output_target": plan.outputs.first(),
        "outputs": plan.outputs,
        "startup_probe": startup_probe,
        "stream_online": false,
        "stream_binding": {
            "schema": stream_binding.schema,
            "vhost": stream_binding.vhost,
            "app": stream_binding.app,
            "stream": stream_binding.stream,
        },
        "recording": plan.recording,
        "zlm_proxy_key": proxy_key,
        "source_url": plan.input_url,
    });
    attach_zlm_server_id(&mut metadata, ctx.zlm_server_id.as_deref());
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
        metadata,
    };
    if let Err(error) =
        ctx.hooks
            .persist_runtime_state(&plan.work_dir, &handle, &SuccessCheck::ProcessExit)
    {
        return rollback_zlm_start_error(&mut rollback, ctx.http_client, ctx.settings, error).await;
    }
    {
        let mut runtimes = ctx.runtimes.write().expect("runtime map lock poisoned");
        runtimes.insert(
            runtime_id,
            ManagedRuntime {
                process: None,
                companion_processes: Vec::new(),
                _slot_permit: slot_permit,
                stop_requested,
                suppress_companion_events: Arc::new(AtomicBool::new(false)),
            },
        );
    }
    if let Err(error) = ctx.hooks.after_runtime_insert(&handle) {
        return rollback_zlm_start_error(&mut rollback, ctx.http_client, ctx.settings, error).await;
    }
    ctx.registry.track(handle.clone());
    if let Err(error) = ctx.hooks.after_registry_track(&handle) {
        return rollback_zlm_start_error(&mut rollback, ctx.http_client, ctx.settings, error).await;
    }
    let _ = ctx
        .events
        .send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
            task_id: handle.task_id,
            attempt_no: handle.attempt_no,
            lease_token: runtime_lease_token(&handle).unwrap_or_default(),
            session_epoch: runtime_session_epoch(&handle),
            event_type: "zlm_proxy_created".to_string(),
            event_level: "info".to_string(),
            message: "stream_ingest proxy created in ZLM".to_string(),
            payload: json!({
                "runtime_id": handle.runtime_id,
                "vhost": stream_binding.vhost,
                "app": stream_binding.app,
                "stream": stream_binding.stream,
                "zlm_proxy_key": extract_zlm_proxy_key(&response),
            }),
        }));
    spawn_live_relay_monitor(
        runtime_id,
        plan.work_dir,
        plan.startup_probe,
        ctx.settings.clone(),
        ctx.http_client.clone(),
        ctx.registry.clone(),
        ctx.runtimes.clone(),
        ctx.events.clone(),
    );
    rollback.disarm();
    Ok(handle)
}

pub(crate) async fn start_rtp_receive_task(
    ctx: RuntimeZlmStartContext<'_>,
    request: &StartTaskRequest,
    slot_permit: Arc<RuntimeSlotPermit>,
) -> Result<RuntimeHandle, ExecutorError> {
    let spec = parse_task_spec(request)?;
    let plan = build_rtp_receive_plan(ctx.settings, request, &spec)?;
    prepare_work_dir(&plan.work_dir)?;

    let response = call_zlm_api(
        ctx.http_client,
        ctx.settings,
        "/index/api/openRtpServer",
        &build_open_rtp_server_params(&plan),
    )
    .await?;
    let local_port = extract_zlm_local_port(&response).unwrap_or(plan.requested_port);
    let runtime_id = Uuid::now_v7();
    let mut rollback = ZlmStartRollback::rtp_server(
        runtime_id,
        plan.work_dir.clone(),
        ctx.registry.clone(),
        ctx.runtimes.clone(),
        plan.stream_id.clone(),
    );
    let rtp_server = RtpServerMetadata {
        stream_id: plan.stream_id.clone(),
        local_port,
        requested_port: plan.requested_port,
        tcp_mode: plan.tcp_mode,
        reuse_port: plan.reuse_port,
        ssrc: plan.ssrc,
    };
    let stop_requested = Arc::new(AtomicBool::new(false));
    let mut metadata = json!({
        "task_type": request.task_type,
        "execution_mode": request.execution_mode,
        "lease_token": request.lease_token,
        "session_epoch": request.session_epoch,
        "trace_context": request.trace_context,
        "resolved_spec": request.resolved_spec,
        "work_dir": plan.work_dir,
        "output_target": plan.outputs.first(),
        "outputs": plan.outputs,
        "stream_online": false,
        "rtp_stream_id": rtp_server.stream_id,
        "rtp_server": rtp_server,
    });
    attach_zlm_server_id(&mut metadata, ctx.zlm_server_id.as_deref());
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
        metadata,
    };
    if let Err(error) =
        ctx.hooks
            .persist_runtime_state(&plan.work_dir, &handle, &SuccessCheck::ProcessExit)
    {
        return rollback_zlm_start_error(&mut rollback, ctx.http_client, ctx.settings, error).await;
    }
    {
        let mut runtimes = ctx.runtimes.write().expect("runtime map lock poisoned");
        runtimes.insert(
            runtime_id,
            ManagedRuntime {
                process: None,
                companion_processes: Vec::new(),
                _slot_permit: slot_permit,
                stop_requested,
                suppress_companion_events: Arc::new(AtomicBool::new(false)),
            },
        );
    }
    if let Err(error) = ctx.hooks.after_runtime_insert(&handle) {
        return rollback_zlm_start_error(&mut rollback, ctx.http_client, ctx.settings, error).await;
    }
    ctx.registry.track(handle.clone());
    if let Err(error) = ctx.hooks.after_registry_track(&handle) {
        return rollback_zlm_start_error(&mut rollback, ctx.http_client, ctx.settings, error).await;
    }
    let _ = ctx
        .events
        .send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
            task_id: handle.task_id,
            attempt_no: handle.attempt_no,
            lease_token: runtime_lease_token(&handle).unwrap_or_default(),
            session_epoch: runtime_session_epoch(&handle),
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
        ctx.settings.clone(),
        ctx.http_client.clone(),
        ctx.registry.clone(),
        ctx.runtimes.clone(),
        ctx.events.clone(),
    );
    rollback.disarm();
    Ok(handle)
}
