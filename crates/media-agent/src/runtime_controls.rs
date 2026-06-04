//! Runtime 控制入口与控制动作：封装录制控制校验、ZLM 关闭、RTP 关闭以及手动录制启停。
//!
//! 这一层负责“已经启动的 runtime 如何被控制”，包括 lease 校验、防并发 guard、
//! resolved spec/binding 解析，以及真正调用 ZLM 或本地进程控制的动作。

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex, RwLock},
    time::Duration,
};

use chrono::{DateTime, Utc};
use media_domain::{RuntimeHandle, RuntimeState, TaskSpec};
use reqwest::Client;
use serde_json::{Value, json};
use tokio::time::sleep;
use uuid::Uuid;

use crate::{
    config::AgentSettings,
    recording_control::RecordingControlGuard,
    runtime::{
        ExecutorError, RECORD_DURATION_FORCE_KILL_DELAY, RecordingControlAction, SuccessCheck,
        TaskRecordingControlRequest,
    },
    runtime_events::{RuntimeEventSink, RuntimeNotification},
    runtime_io::attempt_work_dir,
    runtime_manager::{
        RuntimeGeneration, RuntimeInternalEvent, RuntimeMonitorCommit, RuntimeMonitorHandle,
    },
    runtime_metadata::{
        StreamBinding, emit_recording_control_event, live_relay_recording_from_handle,
        process_identity_from_handle, recording_control_notification, resolved_spec_from_handle,
        runtime_lease_token, stream_binding_from_handle, stream_online,
    },
    runtime_persistence::{persist_runtime_state, success_check_from_handle},
    runtime_process::{ManagedRuntime, schedule_force_kill_if_running, signal_process},
    runtime_recording::{
        LiveRelayRecording, build_manual_live_relay_recording, mark_recording_completion,
        mark_recording_started, recording_config_matches,
    },
    runtime_registry::LocalRuntimeRegistry,
    runtime_zlm::{
        build_close_stream_params, call_zlm_api, close_zlm_rtp_server,
        start_live_relay_recording as zlm_start_live_relay_recording,
        stop_live_relay_recording as zlm_stop_live_relay_recording,
    },
};

pub(crate) struct RuntimeControlContext<'a> {
    pub(crate) settings: &'a AgentSettings,
    pub(crate) http_client: &'a Client,
    pub(crate) registry: &'a LocalRuntimeRegistry,
    pub(crate) runtimes: &'a Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    pub(crate) events: &'a RuntimeEventSink,
}

pub(crate) struct RuntimeRecordingControlContext<'a> {
    pub(crate) controls: RuntimeControlContext<'a>,
    pub(crate) recording_controls: Arc<StdMutex<HashSet<Uuid>>>,
}

#[derive(Clone)]
pub(crate) enum RuntimeRecordingPreparation {
    Unchanged(RuntimeHandle),
    Immediate(RuntimeMonitorCommit),
    Worker {
        initial_commit: RuntimeMonitorCommit,
        worker: RuntimeRecordingWorkerRequest,
    },
}

#[derive(Clone)]
pub(crate) struct RuntimeRecordingWorkerRequest {
    pub(crate) handle: RuntimeHandle,
    pub(crate) generation: RuntimeGeneration,
    pub(crate) request: TaskRecordingControlRequest,
    pub(crate) binding: StreamBinding,
    pub(crate) action: RecordingControlAction,
    pub(crate) recording: LiveRelayRecording,
    pub(crate) monitor_handle: RuntimeMonitorHandle,
}

#[derive(Debug)]
pub(crate) enum RuntimeRecordingOutcome {
    Updated(RuntimeMonitorCommit),
    #[allow(dead_code)]
    Unchanged(RuntimeHandle),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeRecordingCommandIdStatus {
    New,
    Duplicate,
    Conflict,
}

pub(crate) async fn set_task_recording(
    ctx: RuntimeRecordingControlContext<'_>,
    request: &TaskRecordingControlRequest,
) -> Result<RuntimeHandle, ExecutorError> {
    if request.lease_token.trim().is_empty() {
        return Err(ExecutorError::InvalidRequest(
            "lease_token must not be empty".to_string(),
        ));
    }

    let handle = ctx
        .controls
        .registry
        .find_by_task_attempt(request.task_id, request.attempt_no)
        .ok_or(ExecutorError::RuntimeNotFound {
            task_id: request.task_id,
            attempt_no: request.attempt_no,
        })?;
    let handle_lease_token = runtime_lease_token(&handle).unwrap_or_default();
    if handle_lease_token != request.lease_token {
        return Err(ExecutorError::InvalidRequest(format!(
            "stale recording control for {}/{}: lease_token mismatch",
            request.task_id, request.attempt_no
        )));
    }
    if handle.state != RuntimeState::Running && handle.state != RuntimeState::Starting {
        return Err(ExecutorError::InvalidRequest(format!(
            "recording control requires an active runtime, current state is {:?}",
            handle.state
        )));
    }

    let _guard = RecordingControlGuard::acquire(ctx.recording_controls.clone(), handle.runtime_id)?;
    let spec = resolved_spec_from_handle(&handle).ok_or_else(|| {
        ExecutorError::InvalidRequest("runtime is missing resolved stream_ingest spec".to_string())
    })?;
    if !spec.supports_runtime_recording_control() {
        return Err(ExecutorError::InvalidRequest(
            "recording control only supports realtime stream_ingest runtimes".to_string(),
        ));
    }
    let binding = stream_binding_from_handle(&handle).ok_or_else(|| {
        ExecutorError::InvalidRequest("recording control requires a ZLM stream binding".to_string())
    })?;

    match request.action {
        RecordingControlAction::Start => {
            let requested = build_manual_live_relay_recording(
                ctx.controls.settings,
                request.task_id,
                &spec,
                request.record.as_ref(),
                &request.command_id,
            );
            start_manual_recording(&ctx.controls, request, &handle, &binding, requested).await
        }
        RecordingControlAction::Stop => {
            stop_manual_recording(&ctx.controls, request, &handle, &binding, &spec).await
        }
    }
}

pub(crate) fn recording_control_request_fingerprint(
    request: &TaskRecordingControlRequest,
) -> String {
    serde_json::to_string(&json!({
        "action": recording_control_action_name(request.action),
        "record": request.record,
        "reason": request.reason,
    }))
    .unwrap_or_else(|_| recording_control_action_name(request.action).to_string())
}

pub(crate) fn recording_control_action_name(action: RecordingControlAction) -> &'static str {
    match action {
        RecordingControlAction::Start => "start",
        RecordingControlAction::Stop => "stop",
    }
}

pub(crate) fn recording_command_id_status(
    handle: &RuntimeHandle,
    request: &TaskRecordingControlRequest,
    request_fingerprint: &str,
) -> RuntimeRecordingCommandIdStatus {
    let Some(metadata) = handle.metadata.get("recording_control") else {
        return RuntimeRecordingCommandIdStatus::New;
    };
    let Some(last_command_id) = metadata.get("last_command_id").and_then(Value::as_str) else {
        return RuntimeRecordingCommandIdStatus::New;
    };
    if last_command_id != request.command_id {
        return RuntimeRecordingCommandIdStatus::New;
    }

    let last_action = metadata.get("last_action").and_then(Value::as_str);
    let last_fingerprint = metadata.get("request_fingerprint").and_then(Value::as_str);
    if last_action == Some(recording_control_action_name(request.action))
        && last_fingerprint == Some(request_fingerprint)
    {
        RuntimeRecordingCommandIdStatus::Duplicate
    } else {
        RuntimeRecordingCommandIdStatus::Conflict
    }
}

pub(crate) fn prepare_runtime_recording_for_manager(
    ctx: &RuntimeControlContext<'_>,
    request: &TaskRecordingControlRequest,
    handle: &RuntimeHandle,
    generation: RuntimeGeneration,
    monitor_handle: RuntimeMonitorHandle,
) -> Result<RuntimeRecordingPreparation, ExecutorError> {
    validate_recording_request_for_handle(request, handle)?;
    let spec = resolved_spec_from_handle(handle).ok_or_else(|| {
        ExecutorError::InvalidRequest("runtime is missing resolved stream_ingest spec".to_string())
    })?;
    if !spec.supports_runtime_recording_control() {
        return Err(ExecutorError::InvalidRequest(
            "recording control only supports realtime stream_ingest runtimes".to_string(),
        ));
    }
    let binding = stream_binding_from_handle(handle).ok_or_else(|| {
        ExecutorError::InvalidRequest("recording control requires a ZLM stream binding".to_string())
    })?;

    match request.action {
        RecordingControlAction::Start => {
            let requested = build_manual_live_relay_recording(
                ctx.settings,
                request.task_id,
                &spec,
                request.record.as_ref(),
                &request.command_id,
            );
            prepare_start_recording_for_manager(
                ctx,
                request,
                handle,
                generation,
                monitor_handle,
                binding,
                requested,
            )
        }
        RecordingControlAction::Stop => prepare_stop_recording_for_manager(
            ctx,
            request,
            handle,
            generation,
            monitor_handle,
            binding,
            &spec,
        ),
    }
}

pub(crate) async fn run_runtime_recording_worker(
    ctx: RuntimeControlContext<'_>,
    worker: RuntimeRecordingWorkerRequest,
) -> Result<RuntimeRecordingOutcome, ExecutorError> {
    match worker.action {
        RecordingControlAction::Start => {
            let updated_recording = start_stream_recording(
                ctx.http_client,
                ctx.settings,
                &worker.binding,
                &worker.recording,
                Utc::now(),
            )
            .await?;
            maybe_spawn_manual_recording_duration_timer_with_monitor(
                worker.monitor_handle.clone(),
                worker.binding.clone(),
                ctx.settings.clone(),
                ctx.http_client.clone(),
                updated_recording.clone(),
            );
            Ok(RuntimeRecordingOutcome::Updated(recording_commit(
                ctx.settings,
                &worker.request,
                &worker.handle,
                worker.generation,
                &worker.binding,
                &updated_recording,
                vec![recording_control_notification(
                    &apply_recording_metadata_to_handle(
                        &worker.handle,
                        &worker.request,
                        &updated_recording,
                    ),
                    "recording_started",
                    "info",
                    "manual stream recording started",
                    &updated_recording,
                    &worker.request,
                    stream_payload(&worker.binding),
                )],
            )))
        }
        RecordingControlAction::Stop => {
            if worker.recording.started && stream_online(&worker.handle) {
                zlm_stop_live_relay_recording(
                    ctx.http_client,
                    ctx.settings,
                    &worker.binding,
                    &worker.recording,
                )
                .await?;
            }
            let stopped =
                mark_recording_completion(&worker.recording, worker.request.reason.clone());
            Ok(RuntimeRecordingOutcome::Updated(recording_commit(
                ctx.settings,
                &worker.request,
                &worker.handle,
                worker.generation,
                &worker.binding,
                &stopped,
                vec![recording_control_notification(
                    &apply_recording_metadata_to_handle(&worker.handle, &worker.request, &stopped),
                    "recording_stopped",
                    "info",
                    "manual stream recording stopped",
                    &stopped,
                    &worker.request,
                    stream_payload(&worker.binding),
                )],
            )))
        }
    }
}

fn validate_recording_request_for_handle(
    request: &TaskRecordingControlRequest,
    handle: &RuntimeHandle,
) -> Result<(), ExecutorError> {
    if request.lease_token.trim().is_empty() {
        return Err(ExecutorError::InvalidRequest(
            "lease_token must not be empty".to_string(),
        ));
    }
    let handle_lease_token = runtime_lease_token(handle).unwrap_or_default();
    if handle_lease_token != request.lease_token {
        return Err(ExecutorError::InvalidRequest(format!(
            "stale recording control for {}/{}: lease_token mismatch",
            request.task_id, request.attempt_no
        )));
    }
    if handle.state != RuntimeState::Running && handle.state != RuntimeState::Starting {
        return Err(ExecutorError::InvalidRequest(format!(
            "recording control requires an active runtime, current state is {:?}",
            handle.state
        )));
    }
    Ok(())
}

fn prepare_start_recording_for_manager(
    ctx: &RuntimeControlContext<'_>,
    request: &TaskRecordingControlRequest,
    handle: &RuntimeHandle,
    generation: RuntimeGeneration,
    monitor_handle: RuntimeMonitorHandle,
    binding: StreamBinding,
    recording: LiveRelayRecording,
) -> Result<RuntimeRecordingPreparation, ExecutorError> {
    if let Some(existing) = live_relay_recording_from_handle(handle) {
        if existing.started && !recording_config_matches(&existing, &recording) {
            return Err(ExecutorError::InvalidRequest(
                "recording is already running with different parameters; stop it first".to_string(),
            ));
        }
        if existing.started {
            return Ok(RuntimeRecordingPreparation::Unchanged(handle.clone()));
        }
    }

    let requested = recording_control_notification(
        handle,
        "recording_start_requested",
        "info",
        "manual stream recording start requested",
        &recording,
        request,
        stream_payload(&binding),
    );

    if !stream_online(handle) {
        let updated = apply_recording_metadata_to_handle(handle, request, &recording);
        let pending = recording_control_notification(
            &updated,
            "recording_start_pending",
            "info",
            "manual stream recording will start after source reconnects",
            &recording,
            request,
            stream_payload(&binding),
        );
        return Ok(RuntimeRecordingPreparation::Immediate(recording_commit(
            ctx.settings,
            request,
            handle,
            generation,
            &binding,
            &recording,
            vec![requested, pending],
        )));
    }

    let initial_commit =
        RuntimeMonitorCommit::new(handle.clone(), generation).with_notifications(vec![requested]);
    Ok(RuntimeRecordingPreparation::Worker {
        initial_commit,
        worker: RuntimeRecordingWorkerRequest {
            handle: handle.clone(),
            generation,
            request: request.clone(),
            binding,
            action: RecordingControlAction::Start,
            recording,
            monitor_handle,
        },
    })
}

fn prepare_stop_recording_for_manager(
    ctx: &RuntimeControlContext<'_>,
    request: &TaskRecordingControlRequest,
    handle: &RuntimeHandle,
    generation: RuntimeGeneration,
    monitor_handle: RuntimeMonitorHandle,
    binding: StreamBinding,
    spec: &TaskSpec,
) -> Result<RuntimeRecordingPreparation, ExecutorError> {
    let mut recording = live_relay_recording_from_handle(handle).unwrap_or_else(|| {
        build_manual_live_relay_recording(
            ctx.settings,
            request.task_id,
            spec,
            request.record.as_ref(),
            &request.command_id,
        )
    });
    recording.manual_control = true;
    recording.desired_enabled = false;
    recording.control_command_id = Some(request.command_id.clone());

    let requested = recording_control_notification(
        handle,
        "recording_stop_requested",
        "info",
        "manual stream recording stop requested",
        &recording,
        request,
        stream_payload(&binding),
    );

    if recording.started && stream_online(handle) {
        let initial_commit = RuntimeMonitorCommit::new(handle.clone(), generation)
            .with_notifications(vec![requested]);
        return Ok(RuntimeRecordingPreparation::Worker {
            initial_commit,
            worker: RuntimeRecordingWorkerRequest {
                handle: handle.clone(),
                generation,
                request: request.clone(),
                binding,
                action: RecordingControlAction::Stop,
                recording,
                monitor_handle,
            },
        });
    }

    let stopped = mark_recording_completion(&recording, request.reason.clone());
    let updated = apply_recording_metadata_to_handle(handle, request, &stopped);
    let completion = recording_control_notification(
        &updated,
        "recording_stopped",
        "info",
        "manual stream recording stopped",
        &stopped,
        request,
        stream_payload(&binding),
    );
    Ok(RuntimeRecordingPreparation::Immediate(recording_commit(
        ctx.settings,
        request,
        handle,
        generation,
        &binding,
        &stopped,
        vec![requested, completion],
    )))
}

fn recording_commit(
    settings: &AgentSettings,
    request: &TaskRecordingControlRequest,
    previous: &RuntimeHandle,
    generation: RuntimeGeneration,
    _binding: &StreamBinding,
    recording: &LiveRelayRecording,
    mut notifications: Vec<RuntimeNotification>,
) -> RuntimeMonitorCommit {
    let updated = apply_recording_metadata_to_handle(previous, request, recording);
    notifications.push(RuntimeNotification::TaskSnapshot(updated.clone()));
    recording_commit_with_handle(
        settings,
        request,
        previous,
        generation,
        updated,
        notifications,
    )
}

fn recording_commit_with_handle(
    settings: &AgentSettings,
    request: &TaskRecordingControlRequest,
    previous: &RuntimeHandle,
    generation: RuntimeGeneration,
    updated: RuntimeHandle,
    notifications: Vec<RuntimeNotification>,
) -> RuntimeMonitorCommit {
    RuntimeMonitorCommit::new(updated, generation)
        .with_persist(
            attempt_work_dir(settings, request.task_id, request.attempt_no),
            success_check_from_handle(previous),
        )
        .with_notifications(notifications)
}

fn apply_recording_metadata_to_handle(
    handle: &RuntimeHandle,
    request: &TaskRecordingControlRequest,
    recording: &LiveRelayRecording,
) -> RuntimeHandle {
    let mut updated = handle.clone();
    updated.last_progress_at = Some(Utc::now());
    updated.metadata["recording"] = json!(recording.clone());
    updated.metadata["recording_error"] = Value::Null;
    updated.metadata["recording_control"] = json!({
        "last_command_id": request.command_id,
        "last_action": recording_control_action_name(request.action),
        "request_fingerprint": recording_control_request_fingerprint(request),
    });
    updated
}

fn stream_payload(binding: &StreamBinding) -> Value {
    json!({
        "schema": binding.schema,
        "vhost": binding.vhost,
        "app": binding.app,
        "stream": binding.stream,
    })
}

pub(crate) async fn close_live_relay(
    ctx: &RuntimeControlContext<'_>,
    handle: &RuntimeHandle,
    force: bool,
) -> Result<(), ExecutorError> {
    let binding = stream_binding_from_handle(handle).ok_or_else(|| {
        ExecutorError::InvalidRequest(
            "live_relay runtime is missing stream binding metadata".to_string(),
        )
    })?;
    let _ = call_zlm_api(
        ctx.http_client,
        ctx.settings,
        "/index/api/close_streams",
        &build_close_stream_params(&binding, force),
    )
    .await?;
    Ok(())
}

pub(crate) async fn stop_live_relay_recording_for_handle(
    ctx: &RuntimeControlContext<'_>,
    handle: &RuntimeHandle,
) -> Result<(), ExecutorError> {
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
    zlm_stop_live_relay_recording(ctx.http_client, ctx.settings, &binding, &recording).await
}

pub(crate) async fn start_manual_recording(
    ctx: &RuntimeControlContext<'_>,
    request: &TaskRecordingControlRequest,
    handle: &RuntimeHandle,
    binding: &StreamBinding,
    recording: LiveRelayRecording,
) -> Result<RuntimeHandle, ExecutorError> {
    if let Some(existing) = live_relay_recording_from_handle(handle) {
        if existing.started && !recording_config_matches(&existing, &recording) {
            return Err(ExecutorError::InvalidRequest(
                "recording is already running with different parameters; stop it first".to_string(),
            ));
        }
        if existing.started {
            return Ok(handle.clone());
        }
    }

    emit_recording_control_event(
        ctx.events,
        handle,
        "recording_start_requested",
        "info",
        "manual stream recording start requested",
        &recording,
        request,
        json!({
            "schema": binding.schema,
            "vhost": binding.vhost,
            "app": binding.app,
            "stream": binding.stream,
        }),
    );

    let work_dir = attempt_work_dir(ctx.settings, request.task_id, request.attempt_no);
    let success_check = success_check_from_handle(handle);
    if !stream_online(handle) {
        let pending_handle = ctx
            .registry
            .update(handle.runtime_id, |runtime| {
                runtime.last_progress_at = Some(Utc::now());
                runtime.metadata["recording"] = json!(recording.clone());
                runtime.metadata["recording_error"] = Value::Null;
            })
            .unwrap_or_else(|| {
                let mut updated = handle.clone();
                updated.last_progress_at = Some(Utc::now());
                updated.metadata["recording"] = json!(recording.clone());
                updated.metadata["recording_error"] = Value::Null;
                updated
            });
        let _ = persist_runtime_state(&work_dir, &pending_handle, &success_check);
        emit_recording_control_event(
            ctx.events,
            &pending_handle,
            "recording_start_pending",
            "info",
            "manual stream recording will start after source reconnects",
            &recording,
            request,
            json!({
                "schema": binding.schema,
                "vhost": binding.vhost,
                "app": binding.app,
                "stream": binding.stream,
            }),
        );
        let _ = ctx
            .events
            .send(RuntimeNotification::TaskSnapshot(pending_handle.clone()));
        return Ok(pending_handle);
    }

    let updated_recording = start_stream_recording(
        ctx.http_client,
        ctx.settings,
        binding,
        &recording,
        Utc::now(),
    )
    .await?;
    let updated_handle = ctx
        .registry
        .update(handle.runtime_id, |runtime| {
            runtime.last_progress_at = Some(Utc::now());
            runtime.metadata["recording"] = json!(updated_recording.clone());
            runtime.metadata["recording_error"] = Value::Null;
        })
        .unwrap_or_else(|| {
            let mut updated = handle.clone();
            updated.last_progress_at = Some(Utc::now());
            updated.metadata["recording"] = json!(updated_recording.clone());
            updated.metadata["recording_error"] = Value::Null;
            updated
        });
    let _ = persist_runtime_state(&work_dir, &updated_handle, &success_check);
    emit_recording_control_event(
        ctx.events,
        &updated_handle,
        "recording_started",
        "info",
        "manual stream recording started",
        &updated_recording,
        request,
        json!({
            "schema": binding.schema,
            "vhost": binding.vhost,
            "app": binding.app,
            "stream": binding.stream,
        }),
    );
    maybe_spawn_manual_recording_duration_timer(
        updated_handle.runtime_id,
        work_dir,
        success_check,
        binding.clone(),
        ctx.settings.clone(),
        ctx.http_client.clone(),
        ctx.registry.clone(),
        ctx.runtimes.clone(),
        ctx.events.clone(),
        updated_recording,
    );
    let _ = ctx
        .events
        .send(RuntimeNotification::TaskSnapshot(updated_handle.clone()));
    Ok(updated_handle)
}

pub(crate) async fn stop_manual_recording(
    ctx: &RuntimeControlContext<'_>,
    request: &TaskRecordingControlRequest,
    handle: &RuntimeHandle,
    binding: &StreamBinding,
    spec: &TaskSpec,
) -> Result<RuntimeHandle, ExecutorError> {
    let mut recording = live_relay_recording_from_handle(handle).unwrap_or_else(|| {
        build_manual_live_relay_recording(
            ctx.settings,
            request.task_id,
            spec,
            request.record.as_ref(),
            &request.command_id,
        )
    });
    recording.manual_control = true;
    recording.desired_enabled = false;
    recording.control_command_id = Some(request.command_id.clone());

    emit_recording_control_event(
        ctx.events,
        handle,
        "recording_stop_requested",
        "info",
        "manual stream recording stop requested",
        &recording,
        request,
        json!({
            "schema": binding.schema,
            "vhost": binding.vhost,
            "app": binding.app,
            "stream": binding.stream,
        }),
    );

    if recording.started && stream_online(handle) {
        zlm_stop_live_relay_recording(ctx.http_client, ctx.settings, binding, &recording).await?;
    }

    let stopped = mark_recording_completion(&recording, request.reason.clone());
    let work_dir = attempt_work_dir(ctx.settings, request.task_id, request.attempt_no);
    let success_check = success_check_from_handle(handle);
    let updated_handle = ctx
        .registry
        .update(handle.runtime_id, |runtime| {
            runtime.last_progress_at = Some(Utc::now());
            runtime.metadata["recording"] = json!(stopped.clone());
            runtime.metadata["recording_error"] = Value::Null;
        })
        .unwrap_or_else(|| {
            let mut updated = handle.clone();
            updated.last_progress_at = Some(Utc::now());
            updated.metadata["recording"] = json!(stopped.clone());
            updated.metadata["recording_error"] = Value::Null;
            updated
        });
    let _ = persist_runtime_state(&work_dir, &updated_handle, &success_check);
    emit_recording_control_event(
        ctx.events,
        &updated_handle,
        "recording_stopped",
        "info",
        "manual stream recording stopped",
        &stopped,
        request,
        json!({
            "schema": binding.schema,
            "vhost": binding.vhost,
            "app": binding.app,
            "stream": binding.stream,
        }),
    );
    let _ = ctx
        .events
        .send(RuntimeNotification::TaskSnapshot(updated_handle.clone()));
    Ok(updated_handle)
}

pub(crate) async fn close_rtp_receive(
    ctx: &RuntimeControlContext<'_>,
    handle: &RuntimeHandle,
) -> Result<(), ExecutorError> {
    let stream_id =
        crate::runtime_metadata::rtp_stream_id_from_handle(handle).ok_or_else(|| {
            ExecutorError::InvalidRequest(
                "rtp_receive runtime is missing rtp_stream_id metadata".to_string(),
            )
        })?;
    close_zlm_rtp_server(ctx.http_client, ctx.settings, &stream_id).await?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordDurationStopAction {
    SignalProcess { pid: i32 },
    CloseStream,
}

pub(crate) async fn request_live_relay_record_duration_stop(
    handle: &RuntimeHandle,
    binding: &StreamBinding,
    settings: &AgentSettings,
    http_client: &Client,
    runtimes: &Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
) -> Result<RecordDurationStopAction, ExecutorError> {
    if let Some(process) = process_identity_from_handle(handle) {
        signal_process(&process, libc::SIGTERM)
            .map_err(|error| ExecutorError::ProcessSignal(error.to_string()))?;
        schedule_force_kill_if_running(
            handle.runtime_id,
            vec![process],
            runtimes.clone(),
            RECORD_DURATION_FORCE_KILL_DELAY,
            "record_duration_reached",
        );
        Ok(RecordDurationStopAction::SignalProcess { pid: process.pid })
    } else {
        call_zlm_api(
            http_client,
            settings,
            "/index/api/close_streams",
            &build_close_stream_params(binding, true),
        )
        .await?;
        Ok(RecordDurationStopAction::CloseStream)
    }
}

pub(crate) async fn start_stream_recording(
    client: &Client,
    settings: &AgentSettings,
    binding: &StreamBinding,
    recording: &LiveRelayRecording,
    now: DateTime<Utc>,
) -> Result<LiveRelayRecording, ExecutorError> {
    zlm_start_live_relay_recording(client, settings, binding, recording).await?;
    Ok(mark_recording_started(recording, now))
}

pub(crate) fn maybe_spawn_manual_recording_duration_timer(
    runtime_id: Uuid,
    work_dir: PathBuf,
    success_check: SuccessCheck,
    binding: StreamBinding,
    settings: AgentSettings,
    http_client: Client,
    registry: LocalRuntimeRegistry,
    runtimes: Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    events: RuntimeEventSink,
    recording: LiveRelayRecording,
) {
    if recording.stop_task_on_duration || !recording.started || !recording.manual_control {
        return;
    }
    let Some(duration_sec) = recording.duration_sec.filter(|value| *value > 0) else {
        return;
    };
    let command_id = recording.control_command_id.clone();

    tokio::spawn(async move {
        sleep(Duration::from_secs(u64::from(duration_sec))).await;
        let Some(handle) = registry.get(runtime_id) else {
            return;
        };
        let runtime_still_tracked = {
            let runtimes = runtimes.read().expect("runtime map lock poisoned");
            runtimes.contains_key(&runtime_id)
        };
        if !runtime_still_tracked {
            return;
        }
        let Some(current) = live_relay_recording_from_handle(&handle) else {
            return;
        };
        if current.stop_task_on_duration
            || !current.manual_control
            || !current.started
            || !current.desired_enabled
            || current.duration_sec != Some(duration_sec)
            || current.control_command_id != command_id
        {
            return;
        }

        let _ = zlm_stop_live_relay_recording(&http_client, &settings, &binding, &current).await;
        let stopped = mark_recording_completion(&current, "manual_duration_reached");
        let updated = registry
            .update(runtime_id, |runtime| {
                runtime.last_progress_at = Some(Utc::now());
                runtime.metadata["recording"] = json!(stopped.clone());
                runtime.metadata["recording_error"] = Value::Null;
            })
            .unwrap_or_else(|| {
                let mut updated = handle.clone();
                updated.last_progress_at = Some(Utc::now());
                updated.metadata["recording"] = json!(stopped.clone());
                updated.metadata["recording_error"] = Value::Null;
                updated
            });
        let _ = persist_runtime_state(&work_dir, &updated, &success_check);
        let request = TaskRecordingControlRequest {
            task_id: updated.task_id,
            attempt_no: updated.attempt_no,
            lease_token: runtime_lease_token(&updated).unwrap_or_default(),
            action: RecordingControlAction::Stop,
            record: None,
            reason: "manual_duration_reached".to_string(),
            command_id: command_id.unwrap_or_else(|| Uuid::now_v7().to_string()),
        };
        emit_recording_control_event(
            &events,
            &updated,
            "recording_stopped",
            "info",
            "manual stream recording duration reached",
            &stopped,
            &request,
            json!({
                "schema": binding.schema,
                "vhost": binding.vhost,
                "app": binding.app,
                "stream": binding.stream,
            }),
        );
        let _ = events.send(RuntimeNotification::TaskSnapshot(updated));
    });
}

pub(crate) fn maybe_spawn_manual_recording_duration_timer_with_monitor(
    monitor_handle: RuntimeMonitorHandle,
    binding: StreamBinding,
    settings: AgentSettings,
    http_client: Client,
    recording: LiveRelayRecording,
) {
    if recording.stop_task_on_duration || !recording.started || !recording.manual_control {
        return;
    }
    let Some(duration_sec) = recording.duration_sec.filter(|value| *value > 0) else {
        return;
    };
    let command_id = recording.control_command_id.clone();

    tokio::spawn(async move {
        sleep(Duration::from_secs(u64::from(duration_sec))).await;
        let Some(snapshot) = monitor_handle.snapshot().await else {
            return;
        };
        let handle = snapshot.handle;
        let Some(current) = live_relay_recording_from_handle(&handle) else {
            return;
        };
        if current.stop_task_on_duration
            || !current.manual_control
            || !current.started
            || !current.desired_enabled
            || current.duration_sec != Some(duration_sec)
            || current.control_command_id != command_id
        {
            return;
        }

        let _ = zlm_stop_live_relay_recording(&http_client, &settings, &binding, &current).await;
        let stopped = mark_recording_completion(&current, "manual_duration_reached");
        let request = TaskRecordingControlRequest {
            task_id: handle.task_id,
            attempt_no: handle.attempt_no,
            lease_token: runtime_lease_token(&handle).unwrap_or_default(),
            action: RecordingControlAction::Stop,
            record: None,
            reason: "manual_duration_reached".to_string(),
            command_id: command_id.unwrap_or_else(|| Uuid::now_v7().to_string()),
        };
        let commit = recording_commit(
            &settings,
            &request,
            &handle,
            monitor_handle.generation(),
            &binding,
            &stopped,
            vec![recording_control_notification(
                &apply_recording_metadata_to_handle(&handle, &request, &stopped),
                "recording_stopped",
                "info",
                "manual stream recording duration reached",
                &stopped,
                &request,
                stream_payload(&binding),
            )],
        );
        monitor_handle
            .send_event(RuntimeInternalEvent::ApplyMonitorCommit(commit))
            .await;
    });
}
