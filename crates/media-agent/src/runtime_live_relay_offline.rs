//! Live relay 离线辅助：处理运行中代理流连续离线后的重连或终态退出。
//!
//! 这里只封装 source_disconnected 的 sticky reconnect 标记、重连事件、录制 gap 事件、
//! 以及超过离线阈值后的终态事件和 runtime 移除；离线计数和阈值判断仍由 monitor 主循环负责。

use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, RwLock},
};

use chrono::Utc;
use media_domain::{RuntimeHandle, RuntimeState};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    config::AgentSettings,
    runtime::{StartupProbe, SuccessCheck},
    runtime_events::{RuntimeEventSink, RuntimeNotification},
    runtime_live_relay_events::{
        LiveRelayEventStream, emit_live_relay_terminal_event, live_relay_offline_terminal_event,
    },
    runtime_metadata::{
        emit_recording_gap_started_event, emit_source_reconnecting_event, mark_source_reconnecting,
        should_emit_recording_gap_started, should_emit_source_reconnecting,
        sticky_reconnect_stream_ingest_from_handle,
    },
    runtime_persistence::persist_runtime_state,
    runtime_process::{ManagedRuntime, remove_managed_runtime},
    runtime_registry::LocalRuntimeRegistry,
};

pub(crate) enum LiveRelayOfflineOutcome {
    Retry,
    Fatal,
}

pub(crate) struct LiveRelayOfflineContext<'a> {
    pub(crate) runtime_id: Uuid,
    pub(crate) work_dir: &'a Path,
    pub(crate) settings: &'a AgentSettings,
    pub(crate) registry: &'a LocalRuntimeRegistry,
    pub(crate) runtimes: &'a Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    pub(crate) events: &'a RuntimeEventSink,
}

pub(crate) fn handle_live_relay_offline_after_threshold(
    ctx: LiveRelayOfflineContext<'_>,
    current_handle: &RuntimeHandle,
    startup_probe: &StartupProbe,
    stop_requested: bool,
) -> LiveRelayOfflineOutcome {
    if sticky_reconnect_stream_ingest_from_handle(current_handle) {
        let emit_event = should_emit_source_reconnecting(current_handle, "source_disconnected");
        let emit_gap_started = should_emit_recording_gap_started(current_handle);
        let reconnecting_handle = ctx
            .registry
            .update(ctx.runtime_id, |runtime| {
                mark_source_reconnecting(runtime, "source_disconnected");
            })
            .unwrap_or_else(|| {
                let mut handle = current_handle.clone();
                mark_source_reconnecting(&mut handle, "source_disconnected");
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
                "live_relay stream went offline; waiting for ZLM reconnect",
                json!({
                    "runtime_id": reconnecting_handle.runtime_id,
                    "schema": startup_probe.schema,
                    "vhost": startup_probe.vhost,
                    "app": startup_probe.app,
                    "stream": startup_probe.stream,
                    "reason": "source_disconnected",
                    "orphaned": reconnecting_handle.metadata.get("orphaned").and_then(Value::as_bool).unwrap_or(false),
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
                "source_disconnected",
                json!({
                    "runtime_id": reconnecting_handle.runtime_id,
                    "schema": startup_probe.schema,
                    "vhost": startup_probe.vhost,
                    "app": startup_probe.app,
                    "stream": startup_probe.stream,
                    "orphaned": reconnecting_handle.metadata.get("orphaned").and_then(Value::as_bool).unwrap_or(false),
                }),
            );
        }
        return LiveRelayOfflineOutcome::Retry;
    }

    let exited_handle = ctx
        .registry
        .update(ctx.runtime_id, |runtime| {
            runtime.state = RuntimeState::Exited;
            runtime.last_progress_at = Some(Utc::now());
            runtime.metadata["stream_online"] = json!(false);
        })
        .unwrap_or_else(|| {
            let mut handle = current_handle.clone();
            handle.state = RuntimeState::Exited;
            handle.last_progress_at = Some(Utc::now());
            handle
        });
    emit_live_relay_terminal_event(
        ctx.events,
        &exited_handle,
        LiveRelayEventStream::from(startup_probe),
        live_relay_offline_terminal_event(
            ctx.settings,
            current_handle,
            &exited_handle,
            stop_requested,
        ),
        true,
    );
    let _ = persist_runtime_state(ctx.work_dir, &exited_handle, &SuccessCheck::ProcessExit);
    let _ = ctx
        .events
        .send(RuntimeNotification::TaskSnapshot(exited_handle.clone()));
    let _ = remove_managed_runtime(ctx.runtimes, ctx.runtime_id);
    let _ = ctx.registry.remove(ctx.runtime_id);
    LiveRelayOfflineOutcome::Fatal
}
