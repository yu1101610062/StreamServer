//! 任务规格解析：合并请求规格、应用默认值，并校验回调地址和托管文件输出目标。

use media_domain::{PublishTargetKind, TaskSpec, TaskType, TaskValidationError, ValidationIssue};
use reqwest::Url;
use serde_json::{Value, json};

use super::{RepoError, TaskRepository};

impl TaskRepository {
    pub(super) async fn resolve_requested_task(
        &self,
        requested_spec: &TaskSpec,
    ) -> Result<ResolvedTaskRequest, RepoError> {
        let overlay = task_spec_overlay(requested_spec);
        let merged_json = build_resolved_task_json(requested_spec.task_type, &overlay)?;
        let merged_spec: TaskSpec = serde_json::from_value(merged_json)?;
        merged_spec.validate()?;
        validate_task_callback_url(&merged_spec)?;
        validate_managed_file_publish_target(&merged_spec)?;
        let resolved_spec = merged_spec.resolved();
        resolved_spec.validate()?;
        validate_task_callback_url(&resolved_spec)?;
        validate_managed_file_publish_target(&resolved_spec)?;

        Ok(ResolvedTaskRequest {
            requested_spec: merged_spec,
            resolved_spec,
        })
    }
}

#[derive(Debug, Clone)]
pub(super) struct ResolvedTaskRequest {
    pub(super) requested_spec: TaskSpec,
    pub(super) resolved_spec: TaskSpec,
}

pub(crate) fn validation_error(field: &'static str, message: impl Into<String>) -> RepoError {
    RepoError::Validation(TaskValidationError {
        issues: vec![ValidationIssue::new(field, message)],
    })
}

pub(super) fn validate_managed_file_publish_target(spec: &TaskSpec) -> Result<(), RepoError> {
    if !matches!(spec.publish.kind, Some(PublishTargetKind::File)) {
        return Ok(());
    }

    // 文件输出路径由平台分配，禁止客户端指定绝对路径绕过 allowlist 和清理策略。
    if spec
        .publish
        .url
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
    {
        return Err(validation_error(
            "publish.url",
            "must not be provided for file output; output path is managed by the platform",
        ));
    }
    Ok(())
}

fn validate_task_callback_url(spec: &TaskSpec) -> Result<(), RepoError> {
    let Some(callback_url) = spec
        .common
        .callback_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(());
    };

    let url = Url::parse(callback_url).map_err(|_| {
        validation_error(
            "common.callback_url",
            "must be an absolute http:// or https:// URL",
        )
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(validation_error(
            "common.callback_url",
            "must use http:// or https://",
        ));
    }
    if url.host_str().is_none() {
        return Err(validation_error(
            "common.callback_url",
            "must include a host",
        ));
    }
    Ok(())
}

pub(super) fn build_resolved_task_json(
    task_type: TaskType,
    request_overrides: &Value,
) -> Result<Value, RepoError> {
    let mut merged = json!({});
    if !request_overrides.is_object() {
        return Err(validation_error(
            "task",
            "request payload must be a JSON object",
        ));
    }
    deep_merge(&mut merged, request_overrides.clone());
    merged["type"] = Value::String(task_type.as_str().to_string());
    Ok(merged)
}

pub(super) fn task_spec_overlay(spec: &TaskSpec) -> Value {
    // overlay 只保留用户请求中显式出现的字段；默认值由 build_resolved_task_json
    // 与 TaskSpec::resolved() 统一补齐，避免把空值误持久化成用户选择。
    let mut overlay = serde_json::Map::new();
    overlay.insert(
        "type".to_string(),
        Value::String(spec.task_type.as_str().to_string()),
    );
    overlay.insert("name".to_string(), Value::String(spec.name.clone()));
    overlay.insert("priority".to_string(), json!(spec.priority));

    insert_overlay_section(&mut overlay, "common", common_overlay(spec));
    insert_overlay_section(&mut overlay, "input", input_overlay(spec));
    insert_overlay_section(&mut overlay, "process", process_overlay(spec));
    insert_overlay_section(&mut overlay, "stream", stream_overlay(spec));
    insert_overlay_section(&mut overlay, "expose", expose_overlay(spec));
    insert_overlay_section(&mut overlay, "publish", publish_overlay(spec));
    insert_overlay_section(&mut overlay, "record", record_overlay(spec));
    insert_overlay_section(&mut overlay, "recovery", recovery_overlay(spec));
    insert_overlay_section(&mut overlay, "schedule", schedule_overlay(spec));
    insert_overlay_section(&mut overlay, "resource", resource_overlay(spec));

    Value::Object(overlay)
}

fn insert_overlay_section(
    overlay: &mut serde_json::Map<String, Value>,
    key: &str,
    section: Option<Value>,
) {
    if let Some(section) = section {
        overlay.insert(key.to_string(), section);
    }
}

fn common_overlay(spec: &TaskSpec) -> Option<Value> {
    // common 字段承担审计和回调归属，空字符串不进入 overlay，
    // 后续 validate() 会决定 created_by 是否必须存在。
    let mut common = serde_json::Map::new();
    if let Some(created_by) = spec
        .common
        .created_by
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        common.insert(
            "created_by".to_string(),
            Value::String(created_by.trim().to_string()),
        );
    }
    if let Some(callback_url) = spec
        .common
        .callback_url
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        common.insert(
            "callback_url".to_string(),
            Value::String(callback_url.trim().to_string()),
        );
    }
    if !spec.common.labels.is_empty() {
        common.insert("labels".to_string(), json!(spec.common.labels));
    }
    (!common.is_empty()).then_some(Value::Object(common))
}

fn input_overlay(spec: &TaskSpec) -> Option<Value> {
    overlay_optional_fields(&[
        ("kind", spec.input.kind.map(|value| json!(value))),
        (
            "source_mode",
            spec.input.source_mode.map(|value| json!(value)),
        ),
        (
            "loop_enabled",
            spec.input.loop_enabled.map(|value| json!(value)),
        ),
        (
            "start_offset_sec",
            spec.input.start_offset_sec.map(|value| json!(value)),
        ),
        ("url", spec.input.url.as_ref().map(|value| json!(value))),
        ("group", spec.input.group.as_ref().map(|value| json!(value))),
        ("port", spec.input.port.map(|value| json!(value))),
        (
            "interface_name",
            spec.input.interface_name.as_ref().map(|value| json!(value)),
        ),
        (
            "interface_ip",
            spec.input.interface_ip.as_ref().map(|value| json!(value)),
        ),
        ("ttl", spec.input.ttl.map(|value| json!(value))),
        ("reuse", spec.input.reuse.map(|value| json!(value))),
        ("pkt_size", spec.input.pkt_size.map(|value| json!(value))),
        ("dscp", spec.input.dscp.map(|value| json!(value))),
        (
            "buffer_size",
            spec.input.buffer_size.map(|value| json!(value)),
        ),
        ("fifo_size", spec.input.fifo_size.map(|value| json!(value))),
        (
            "probe_timeout_ms",
            spec.input.probe_timeout_ms.map(|value| json!(value)),
        ),
        ("tcp_mode", spec.input.tcp_mode.map(|value| json!(value))),
        ("ssrc", spec.input.ssrc.map(|value| json!(value))),
    ])
}

fn process_overlay(spec: &TaskSpec) -> Option<Value> {
    overlay_optional_fields(&[
        ("mode", spec.process.mode.as_ref().map(|value| json!(value))),
        ("bitrate", spec.process.bitrate.map(|value| json!(value))),
        ("fps", spec.process.fps.map(|value| json!(value))),
        ("gop", spec.process.gop.map(|value| json!(value))),
    ])
}

fn stream_overlay(spec: &TaskSpec) -> Option<Value> {
    overlay_optional_fields(&[
        ("app", spec.stream.app.as_ref().map(|value| json!(value))),
        ("name", spec.stream.name.as_ref().map(|value| json!(value))),
        (
            "vhost",
            spec.stream.vhost.as_ref().map(|value| json!(value)),
        ),
    ])
}

fn expose_overlay(spec: &TaskSpec) -> Option<Value> {
    overlay_optional_fields(&[
        (
            "enable_rtsp",
            spec.expose.enable_rtsp.map(|value| json!(value)),
        ),
        (
            "enable_rtmp",
            spec.expose.enable_rtmp.map(|value| json!(value)),
        ),
        (
            "enable_http_ts",
            spec.expose.enable_http_ts.map(|value| json!(value)),
        ),
        (
            "enable_http_fmp4",
            spec.expose.enable_http_fmp4.map(|value| json!(value)),
        ),
        (
            "enable_hls",
            spec.expose.enable_hls.map(|value| json!(value)),
        ),
        (
            "stop_on_no_reader",
            spec.expose.stop_on_no_reader.map(|value| json!(value)),
        ),
    ])
}

fn publish_overlay(spec: &TaskSpec) -> Option<Value> {
    // publish 字段在 stream_bridge 和 file_transcode 中含义不同；
    // 这里只保存原始选择，不在组装阶段做跨任务类型判断。
    overlay_optional_fields(&[
        ("kind", spec.publish.kind.map(|value| json!(value))),
        ("url", spec.publish.url.as_ref().map(|value| json!(value))),
        (
            "group",
            spec.publish.group.as_ref().map(|value| json!(value)),
        ),
        ("port", spec.publish.port.map(|value| json!(value))),
        (
            "interface_name",
            spec.publish
                .interface_name
                .as_ref()
                .map(|value| json!(value)),
        ),
        (
            "interface_ip",
            spec.publish.interface_ip.as_ref().map(|value| json!(value)),
        ),
        ("ttl", spec.publish.ttl.map(|value| json!(value))),
        ("reuse", spec.publish.reuse.map(|value| json!(value))),
        ("pkt_size", spec.publish.pkt_size.map(|value| json!(value))),
        ("dscp", spec.publish.dscp.map(|value| json!(value))),
        (
            "buffer_size",
            spec.publish.buffer_size.map(|value| json!(value)),
        ),
        (
            "fifo_size",
            spec.publish.fifo_size.map(|value| json!(value)),
        ),
        (
            "format",
            spec.publish.format.as_ref().map(|value| json!(value)),
        ),
    ])
}

fn record_overlay(spec: &TaskSpec) -> Option<Value> {
    overlay_optional_fields(&[
        ("enabled", spec.record.enabled.map(|value| json!(value))),
        ("format", spec.record.format.map(|value| json!(value))),
        (
            "duration_sec",
            spec.record.duration_sec.map(|value| json!(value)),
        ),
        (
            "segment_sec",
            spec.record.segment_sec.map(|value| json!(value)),
        ),
        (
            "save_path",
            spec.record.save_path.as_ref().map(|value| json!(value)),
        ),
        ("as_player", spec.record.as_player.map(|value| json!(value))),
    ])
}

fn recovery_overlay(spec: &TaskSpec) -> Option<Value> {
    overlay_optional_fields(&[
        ("policy", spec.recovery.policy.map(|value| json!(value))),
        (
            "resume_mode",
            spec.recovery.resume_mode.as_ref().map(|value| json!(value)),
        ),
        (
            "max_consecutive_failures",
            spec.recovery
                .max_consecutive_failures
                .map(|value| json!(value)),
        ),
    ])
}

fn schedule_overlay(spec: &TaskSpec) -> Option<Value> {
    // schedule 为空时保持缺省 immediate 语义；cron 空字符串不写入 overlay，
    // 避免覆盖配置中的默认调度方式。
    let mut schedule = serde_json::Map::new();
    if let Some(start_mode) = spec.schedule.start_mode {
        schedule.insert("start_mode".to_string(), json!(start_mode));
    }
    if let Some(start_at) = spec.schedule.start_at {
        schedule.insert("start_at".to_string(), json!(start_at));
    }
    if let Some(cron) = spec
        .schedule
        .cron
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        schedule.insert("cron".to_string(), json!(cron));
    }
    (!schedule.is_empty()).then_some(Value::Object(schedule))
}

fn resource_overlay(spec: &TaskSpec) -> Option<Value> {
    let mut resource = serde_json::Map::new();
    if !spec.resource.required_labels.is_empty() {
        resource.insert(
            "required_labels".to_string(),
            json!(spec.resource.required_labels),
        );
    }
    (!resource.is_empty()).then_some(Value::Object(resource))
}

fn overlay_optional_fields(fields: &[(&str, Option<Value>)]) -> Option<Value> {
    // Option::None 表示请求未显式设置该字段，而不是设置为 JSON null。
    let mut object = serde_json::Map::new();
    for (key, value) in fields {
        if let Some(value) = value {
            object.insert((*key).to_string(), value.clone());
        }
    }
    (!object.is_empty()).then_some(Value::Object(object))
}

fn deep_merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base_map), Value::Object(overlay_map)) => {
            for (key, overlay_value) in overlay_map {
                match base_map.get_mut(&key) {
                    Some(base_value) => deep_merge(base_value, overlay_value),
                    None => {
                        base_map.insert(key, overlay_value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}
