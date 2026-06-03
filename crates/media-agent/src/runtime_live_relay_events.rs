//! Live relay 事件辅助：集中构造终态事件类型、级别、消息和 payload。
//!
//! 这里只处理事件语义归类与发送，不修改 runtime 状态、不做 ZLM 调用，也不负责持久化。

use media_domain::RuntimeHandle;
use serde_json::{Value, json};

use crate::{
    config::AgentSettings,
    runtime::StartupProbe,
    runtime_events::{
        RuntimeEventSink, RuntimeNotification, RuntimeTaskEvent, runtime_session_epoch,
    },
    runtime_metadata::{
        StreamBinding, completion_reason_from_handle, live_relay_auto_close_enabled_from_handle,
        runtime_lease_token, stop_reason_from_handle,
    },
};

#[derive(Debug, Clone, Copy)]
pub(crate) struct LiveRelayTerminalEvent {
    event_type: &'static str,
    event_level: &'static str,
    message: &'static str,
    reason: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct LiveRelayEventStream<'a> {
    schema: Option<&'a str>,
    vhost: &'a str,
    app: &'a str,
    stream: &'a str,
}

impl<'a> From<&'a StreamBinding> for LiveRelayEventStream<'a> {
    fn from(binding: &'a StreamBinding) -> Self {
        Self {
            schema: binding.schema.as_deref(),
            vhost: &binding.vhost,
            app: &binding.app,
            stream: &binding.stream,
        }
    }
}

impl<'a> From<&'a StartupProbe> for LiveRelayEventStream<'a> {
    fn from(probe: &'a StartupProbe) -> Self {
        Self {
            schema: probe.schema.as_deref(),
            vhost: &probe.vhost,
            app: &probe.app,
            stream: &probe.stream,
        }
    }
}

pub(crate) fn live_relay_stopped_terminal_event(
    exited_handle: &RuntimeHandle,
) -> LiveRelayTerminalEvent {
    let completion_reason = completion_reason_from_handle(exited_handle);
    let stop_reason = stop_reason_from_handle(exited_handle);
    if completion_reason.as_deref() == Some("record_duration_reached") {
        LiveRelayTerminalEvent {
            event_type: "succeeded",
            event_level: "info",
            message: "live_relay completed after recording duration reached",
            reason: "record_duration_reached",
        }
    } else if stop_reason.as_deref() == Some("disk_threshold_exceeded") {
        LiveRelayTerminalEvent {
            event_type: "failed",
            event_level: "error",
            message: "live_relay stopped after disk threshold was exceeded",
            reason: "disk_threshold_exceeded",
        }
    } else {
        LiveRelayTerminalEvent {
            event_type: "canceled",
            event_level: "info",
            message: "live_relay stream stopped",
            reason: "stop_requested",
        }
    }
}

pub(crate) fn live_relay_offline_terminal_event(
    settings: &AgentSettings,
    current_handle: &RuntimeHandle,
    exited_handle: &RuntimeHandle,
    stop_requested: bool,
) -> LiveRelayTerminalEvent {
    let completion_reason = completion_reason_from_handle(exited_handle);
    let stop_reason = stop_reason_from_handle(exited_handle);
    let auto_close_enabled = live_relay_auto_close_enabled_from_handle(settings, current_handle);
    if completion_reason.as_deref() == Some("record_duration_reached") {
        LiveRelayTerminalEvent {
            event_type: "succeeded",
            event_level: "info",
            message: "live_relay completed after recording duration reached",
            reason: "record_duration_reached",
        }
    } else if stop_reason.as_deref() == Some("disk_threshold_exceeded") {
        LiveRelayTerminalEvent {
            event_type: "failed",
            event_level: "error",
            message: "live_relay stopped after disk threshold was exceeded",
            reason: "disk_threshold_exceeded",
        }
    } else if stop_requested {
        LiveRelayTerminalEvent {
            event_type: "canceled",
            event_level: "info",
            message: "live_relay stream stopped",
            reason: "stop_requested",
        }
    } else if auto_close_enabled {
        LiveRelayTerminalEvent {
            event_type: "canceled",
            event_level: "info",
            message: "live_relay stopped after no-reader auto-close policy",
            reason: "no_reader_auto_close",
        }
    } else {
        LiveRelayTerminalEvent {
            event_type: "failed",
            event_level: "error",
            message: "live_relay stream went offline unexpectedly",
            reason: "unexpected_offline",
        }
    }
}

pub(crate) fn emit_live_relay_terminal_event(
    events: &RuntimeEventSink,
    exited_handle: &RuntimeHandle,
    stream: LiveRelayEventStream<'_>,
    terminal_event: LiveRelayTerminalEvent,
    include_orphaned: bool,
) {
    let mut payload = json!({
        "schema": stream.schema,
        "vhost": stream.vhost,
        "app": stream.app,
        "stream": stream.stream,
        "reason": terminal_event.reason,
    });
    if include_orphaned {
        payload["orphaned"] = json!(
            exited_handle
                .metadata
                .get("orphaned")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        );
    }
    let _ = events.send(RuntimeNotification::TaskEvent(RuntimeTaskEvent {
        task_id: exited_handle.task_id,
        attempt_no: exited_handle.attempt_no,
        lease_token: runtime_lease_token(exited_handle).unwrap_or_default(),
        session_epoch: runtime_session_epoch(exited_handle),
        event_type: terminal_event.event_type.to_string(),
        event_level: terminal_event.event_level.to_string(),
        message: terminal_event.message.to_string(),
        payload,
    }));
}
