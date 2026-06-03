//! Live relay 启动超时辅助：处理启动期未上线/录制未就绪后的重试或失败收尾。
//!
//! 这里只封装 startup_timeout 触发后的 sticky reconnect 标记、重连事件、录制 gap 事件、
//! 超时失败事件、ZLM 清理和 runtime 移除；是否已经超时仍由 monitor 主循环判断。

use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, RwLock},
};

use chrono::Utc;
use media_domain::{RuntimeHandle, RuntimeState};
use reqwest::Client;
use serde_json::json;
use uuid::Uuid;

use crate::{
    config::AgentSettings,
    runtime::{STARTUP_PROBE_TIMEOUT, StartupProbe, SuccessCheck},
    runtime_events::{
        RuntimeEventSink, RuntimeNotification, RuntimeTaskEvent, runtime_session_epoch,
    },
    runtime_live_relay_cleanup::cleanup_live_relay_runtime,
    runtime_metadata::{
        StreamBinding, emit_recording_gap_started_event, emit_source_reconnecting_event,
        mark_source_reconnecting, runtime_lease_token, should_emit_recording_gap_started,
        should_emit_source_reconnecting, sticky_reconnect_stream_ingest_from_handle,
        stream_binding_from_handle,
    },
    runtime_persistence::persist_runtime_state,
    runtime_process::{ManagedRuntime, remove_managed_runtime},
    runtime_registry::LocalRuntimeRegistry,
};

#[derive(Debug, Clone, Copy)]
pub(crate) enum LiveRelayStartupTimeoutMode {
    RecordingStartup,
    StreamOnline,
}

pub(crate) enum LiveRelayStartupTimeoutOutcome {
    Retry,
    Fatal,
}

pub(crate) struct LiveRelayStartupTimeoutContext<'a> {
    pub(crate) runtime_id: Uuid,
    pub(crate) work_dir: &'a Path,
    pub(crate) settings: &'a AgentSettings,
    pub(crate) http_client: &'a Client,
    pub(crate) registry: &'a LocalRuntimeRegistry,
    pub(crate) runtimes: &'a Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    pub(crate) events: &'a RuntimeEventSink,
}

pub(crate) async fn handle_live_relay_startup_timeout(
    ctx: LiveRelayStartupTimeoutContext<'_>,
    handle: &RuntimeHandle,
    startup_probe: &StartupProbe,
    mode: LiveRelayStartupTimeoutMode,
) -> LiveRelayStartupTimeoutOutcome {
    if sticky_reconnect_stream_ingest_from_handle(handle) {
        let emit_event = should_emit_source_reconnecting(handle, "startup_timeout");
        let emit_gap_started = should_emit_recording_gap_started(handle);
        let reconnecting_handle = ctx
            .registry
            .update(ctx.runtime_id, |runtime| {
                runtime.metadata["startup_timeout"] = json!(true);
                mark_source_reconnecting(runtime, "startup_timeout");
            })
            .unwrap_or_else(|| {
                let mut handle = handle.clone();
                handle.metadata["startup_timeout"] = json!(true);
                mark_source_reconnecting(&mut handle, "startup_timeout");
                handle
            });
        let _ = persist_runtime_state(
            ctx.work_dir,
            &reconnecting_handle,
            &SuccessCheck::ProcessExit,
        );
        if emit_event {
            emit_source_reconnecting_event(
                ctx.events,
                &reconnecting_handle,
                retry_message(mode, startup_probe),
                json!({
                    "runtime_id": reconnecting_handle.runtime_id,
                    "schema": startup_probe.schema,
                    "vhost": startup_probe.vhost,
                    "app": startup_probe.app,
                    "stream": startup_probe.stream,
                    "reason": "startup_timeout",
                }),
            );
            let _ = ctx.events.send(RuntimeNotification::TaskSnapshot(
                reconnecting_handle.clone(),
            ));
        }
        if emit_gap_started {
            emit_recording_gap_started_event(
                ctx.events,
                &reconnecting_handle,
                "startup_timeout",
                json!({
                    "runtime_id": reconnecting_handle.runtime_id,
                    "schema": startup_probe.schema,
                    "vhost": startup_probe.vhost,
                    "app": startup_probe.app,
                    "stream": startup_probe.stream,
                }),
            );
        }
        return LiveRelayStartupTimeoutOutcome::Retry;
    }

    let binding = stream_binding_from_handle(handle).unwrap_or(StreamBinding {
        schema: startup_probe.schema.clone(),
        vhost: startup_probe.vhost.clone(),
        app: startup_probe.app.clone(),
        stream: startup_probe.stream.clone(),
    });
    cleanup_live_relay_runtime(ctx.http_client, ctx.settings, handle, &binding).await;
    let failed_handle = ctx
        .registry
        .update(ctx.runtime_id, |runtime| {
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
    let _ = ctx
        .events
        .send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
            task_id: failed_handle.task_id,
            attempt_no: failed_handle.attempt_no,
            lease_token: runtime_lease_token(&failed_handle).unwrap_or_default(),
            session_epoch: runtime_session_epoch(&failed_handle),
            event_type: "startup_timeout".to_string(),
            event_level: "error".to_string(),
            message: timeout_message(mode, startup_probe),
            payload: json!({
                "schema": startup_probe.schema,
                "vhost": startup_probe.vhost,
                "app": startup_probe.app,
                "stream": startup_probe.stream,
            }),
        }));
    let _ = ctx
        .events
        .send(RuntimeNotification::TaskSnapshot(failed_handle.clone()));
    let _ = ctx
        .events
        .send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
            task_id: failed_handle.task_id,
            attempt_no: failed_handle.attempt_no,
            lease_token: runtime_lease_token(&failed_handle).unwrap_or_default(),
            session_epoch: runtime_session_epoch(&failed_handle),
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
    let _ = persist_runtime_state(ctx.work_dir, &failed_handle, &SuccessCheck::ProcessExit);
    let _ = remove_managed_runtime(ctx.runtimes, ctx.runtime_id);
    let _ = ctx.registry.remove(ctx.runtime_id);
    LiveRelayStartupTimeoutOutcome::Fatal
}

fn retry_message(mode: LiveRelayStartupTimeoutMode, startup_probe: &StartupProbe) -> String {
    match mode {
        LiveRelayStartupTimeoutMode::RecordingStartup => format!(
            "live_relay recording for {}/{}/{} is not active yet; continuing to retry",
            startup_probe.vhost, startup_probe.app, startup_probe.stream
        ),
        LiveRelayStartupTimeoutMode::StreamOnline => format!(
            "live_relay stream {}/{}/{} is not online yet; continuing to retry",
            startup_probe.vhost, startup_probe.app, startup_probe.stream
        ),
    }
}

fn timeout_message(mode: LiveRelayStartupTimeoutMode, startup_probe: &StartupProbe) -> String {
    match mode {
        LiveRelayStartupTimeoutMode::RecordingStartup => format!(
            "live_relay recording for {}/{}/{} did not start within {} seconds",
            startup_probe.vhost,
            startup_probe.app,
            startup_probe.stream,
            STARTUP_PROBE_TIMEOUT.as_secs()
        ),
        LiveRelayStartupTimeoutMode::StreamOnline => format!(
            "live_relay stream {}/{}/{} did not become online within {} seconds",
            startup_probe.vhost,
            startup_probe.app,
            startup_probe.stream,
            STARTUP_PROBE_TIMEOUT.as_secs()
        ),
    }
}
