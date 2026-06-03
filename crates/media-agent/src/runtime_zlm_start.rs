//! Runtime ZLM 启动：创建 ZLM proxy/RTP server runtime 并登记本地状态。
//!
//! 这个模块只承接 ZLM 托管 runtime 的启动流程，包括调用 ZLM API、构造 runtime
//! metadata、写入持久化状态、占用 runtime slot，以及启动后续在线/存活监控。

use std::{
    collections::HashMap,
    sync::{Arc, RwLock, atomic::AtomicBool},
};

use chrono::Utc;
use media_domain::{RuntimeHandle, RuntimeState, WorkerKind};
use reqwest::Client;
use serde_json::json;
use uuid::Uuid;

use crate::{
    config::AgentSettings,
    runtime::{ExecutorError, StartTaskRequest, SuccessCheck},
    runtime_events::{
        RuntimeEventSink, RuntimeNotification, RuntimeTaskEvent, runtime_session_epoch,
    },
    runtime_metadata::{RtpServerMetadata, attach_zlm_server_id, runtime_lease_token},
    runtime_monitors::{spawn_live_relay_monitor, spawn_rtp_receive_monitor},
    runtime_persistence::persist_runtime_state,
    runtime_plan::{
        build_live_relay_api_params, build_live_relay_plan, build_open_rtp_server_params,
        build_rtp_receive_plan, parse_task_spec, prepare_work_dir,
    },
    runtime_process::{ManagedRuntime, RuntimeSlotPermit},
    runtime_registry::LocalRuntimeRegistry,
    runtime_zlm::{call_zlm_api, extract_zlm_local_port, extract_zlm_proxy_key},
};

pub(crate) struct RuntimeZlmStartContext<'a> {
    pub(crate) settings: &'a AgentSettings,
    pub(crate) http_client: &'a Client,
    pub(crate) registry: &'a LocalRuntimeRegistry,
    pub(crate) runtimes: &'a Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    pub(crate) events: &'a RuntimeEventSink,
    pub(crate) zlm_server_id: Option<String>,
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
    ctx.registry.track(handle.clone());
    persist_runtime_state(&plan.work_dir, &handle, &SuccessCheck::ProcessExit)?;
    ctx.runtimes
        .write()
        .expect("runtime map lock poisoned")
        .insert(
            runtime_id,
            ManagedRuntime {
                pid: None,
                companion_pids: Vec::new(),
                _slot_permit: slot_permit,
                stop_requested,
                suppress_companion_events: Arc::new(AtomicBool::new(false)),
            },
        );
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
        ctx.settings.clone(),
        ctx.http_client.clone(),
        ctx.registry.clone(),
        ctx.runtimes.clone(),
        ctx.events.clone(),
    );
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
    ctx.registry.track(handle.clone());
    persist_runtime_state(&plan.work_dir, &handle, &SuccessCheck::ProcessExit)?;
    ctx.runtimes
        .write()
        .expect("runtime map lock poisoned")
        .insert(
            runtime_id,
            ManagedRuntime {
                pid: None,
                companion_pids: Vec::new(),
                _slot_permit: slot_permit,
                stop_requested,
                suppress_companion_events: Arc::new(AtomicBool::new(false)),
            },
        );
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
    Ok(handle)
}
