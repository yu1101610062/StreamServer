//! Live relay 运行态辅助：处理代理流确认在线后的 Running 状态回写和 running 事件。
//!
//! 这里只负责“已经探测到在线/录制已启动”后的状态迁移、录制 gap 结束事件、运行态持久化
//! 和快照通知；离线判定、重连重试和终态清理由 monitor 主循环决定。

use std::path::Path;

use chrono::Utc;
use media_domain::{RuntimeHandle, RuntimeState};
use serde_json::json;
use uuid::Uuid;

use crate::{
    runtime::{StartupProbe, SuccessCheck},
    runtime_events::{
        RuntimeEventSink, RuntimeNotification, RuntimeTaskEvent, runtime_session_epoch,
    },
    runtime_metadata::{
        clear_source_reconnecting, emit_recording_gap_ended_event, live_relay_startup_ready,
        runtime_lease_token, stream_online,
    },
    runtime_persistence::persist_runtime_state,
    runtime_registry::LocalRuntimeRegistry,
};

pub(crate) struct LiveRelayRunningContext<'a> {
    pub(crate) runtime_id: Uuid,
    pub(crate) work_dir: &'a Path,
    pub(crate) registry: &'a LocalRuntimeRegistry,
    pub(crate) events: &'a RuntimeEventSink,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LiveRelayRunningReadiness {
    Ready,
    NotReady,
}

pub(crate) fn ensure_live_relay_running_if_ready(
    ctx: LiveRelayRunningContext<'_>,
    handle: &RuntimeHandle,
    startup_probe: &StartupProbe,
    recording_started: bool,
    message: &'static str,
) -> LiveRelayRunningReadiness {
    let startup_ready = live_relay_startup_ready(handle);
    if !startup_ready {
        return LiveRelayRunningReadiness::NotReady;
    }
    let should_emit_running =
        handle.state != RuntimeState::Running || !stream_online(handle) || recording_started;
    if !should_emit_running {
        return LiveRelayRunningReadiness::Ready;
    }

    emit_recording_gap_ended_event(
        ctx.events,
        handle,
        "source_reconnected",
        json!({
            "schema": startup_probe.schema,
            "vhost": startup_probe.vhost,
            "app": startup_probe.app,
            "stream": startup_probe.stream,
        }),
    );
    let running_handle = ctx
        .registry
        .update(ctx.runtime_id, |runtime| {
            runtime.state = RuntimeState::Running;
            runtime.last_progress_at = Some(Utc::now());
            runtime.metadata["stream_online"] = json!(true);
            clear_source_reconnecting(runtime);
            runtime.metadata["stream_binding"] = json!({
                "schema": startup_probe.schema,
                "vhost": startup_probe.vhost,
                "app": startup_probe.app,
                "stream": startup_probe.stream,
            });
        })
        .unwrap_or_else(|| handle.clone());
    let _ = persist_runtime_state(ctx.work_dir, &running_handle, &SuccessCheck::ProcessExit);
    let _ = ctx
        .events
        .send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
            task_id: running_handle.task_id,
            attempt_no: running_handle.attempt_no,
            lease_token: runtime_lease_token(&running_handle).unwrap_or_default(),
            session_epoch: runtime_session_epoch(&running_handle),
            event_type: "running".to_string(),
            event_level: "info".to_string(),
            message: message.to_string(),
            payload: json!({
                "runtime_id": running_handle.runtime_id,
                "schema": startup_probe.schema,
                "vhost": startup_probe.vhost,
                "app": startup_probe.app,
                "stream": startup_probe.stream,
            }),
        }));
    let _ = ctx
        .events
        .send(RuntimeNotification::TaskSnapshot(running_handle));
    LiveRelayRunningReadiness::Ready
}
