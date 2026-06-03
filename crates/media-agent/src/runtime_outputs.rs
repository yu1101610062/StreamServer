//! 托管输出路径：负责 Agent 托管文件产物的格式校验、目录分桶、文件名分配和产物元数据类型。

use std::{net::IpAddr, path::PathBuf};

use chrono::Local;
use media_domain::{PublishSpec, TaskSpec, TaskType};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    config::AgentSettings,
    ffmpeg_args::hls_output_args,
    ffmpeg_plan::PublishOutput,
    media_policy::{
        default_file_extension_for_format, disabled_output_format_message, ffmpeg_muxer_for_format,
        logical_output_format_for_format,
    },
    runtime::{ExecutorError, SuccessCheck},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ManagedFileOutputKind {
    Transcode,
    Bridge,
    StreamIngestRecord,
}

impl ManagedFileOutputKind {
    pub(crate) fn metadata_key(self) -> &'static str {
        match self {
            Self::Transcode => "transcode_artifact",
            Self::Bridge => "bridge_artifact",
            Self::StreamIngestRecord => "stream_ingest_record_artifacts",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagedOutputBucket {
    Mp4,
    Hls,
}

impl ManagedOutputBucket {
    fn as_str(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Hls => "hls",
        }
    }

    fn root(self, settings: &AgentSettings) -> &str {
        match self {
            Self::Mp4 => settings.zlm_output_mp4_root.as_str(),
            Self::Hls => settings.zlm_output_hls_root.as_str(),
        }
    }
}

pub(crate) fn managed_file_output_kind_for_task(
    task_type: TaskType,
) -> Option<ManagedFileOutputKind> {
    match task_type {
        TaskType::FileTranscode => Some(ManagedFileOutputKind::Transcode),
        TaskType::StreamBridge => Some(ManagedFileOutputKind::Bridge),
        _ => None,
    }
}

fn normalize_optional_publish_format(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase)
}

pub(crate) fn ensure_output_format_enabled(format: &str) -> Result<(), ExecutorError> {
    if let Some(message) = disabled_output_format_message(format) {
        return Err(ExecutorError::InvalidRequest(message.to_string()));
    }
    Ok(())
}

fn managed_output_bucket_for_format(format: &str) -> ManagedOutputBucket {
    if format.eq_ignore_ascii_case("hls") {
        ManagedOutputBucket::Hls
    } else {
        ManagedOutputBucket::Mp4
    }
}

fn sanitize_output_node_token(value: &str) -> String {
    let mut sanitized = String::new();
    let mut previous_was_separator = false;
    for value in value.trim().chars() {
        let mapped = match value {
            value if value.is_ascii_alphanumeric() => Some(value.to_ascii_lowercase()),
            '-' => Some('-'),
            '_' | '.' | ':' => Some('_'),
            _ => Some('_'),
        };
        let Some(mapped) = mapped else {
            continue;
        };
        if mapped == '_' {
            if previous_was_separator {
                continue;
            }
            previous_was_separator = true;
        } else {
            previous_was_separator = false;
        }
        sanitized.push(mapped);
    }
    let sanitized = sanitized.trim_matches('_').to_string();
    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}

fn managed_output_node_token(settings: &AgentSettings) -> String {
    if settings
        .primary_interface_ip
        .trim()
        .parse::<IpAddr>()
        .is_ok()
    {
        return sanitize_output_node_token(&settings.primary_interface_ip);
    }
    if let Ok(url) = Url::parse(settings.agent_stream_addr.trim()) {
        if let Some(host) = url.host_str() {
            if host.parse::<IpAddr>().is_ok() {
                return sanitize_output_node_token(host);
            }
        }
    }
    "unknown".to_string()
}

pub(crate) fn managed_output_dir(settings: &AgentSettings, task_id: Uuid, format: &str) -> PathBuf {
    let bucket = managed_output_bucket_for_format(format);
    let node_dir = format!(
        "node-{}-{}",
        managed_output_node_token(settings),
        bucket.as_str()
    );
    PathBuf::from(bucket.root(settings))
        .join(node_dir)
        .join(task_id.to_string())
}

pub(crate) fn allocate_managed_output(
    settings: &AgentSettings,
    task_id: Uuid,
    requested_format: Option<&str>,
) -> Result<PublishOutput, ExecutorError> {
    let format =
        normalize_optional_publish_format(requested_format).unwrap_or_else(|| "mp4".to_string());
    ensure_output_format_enabled(&format)?;
    let logical_format = logical_output_format_for_format(&format);
    let extension = default_file_extension_for_format(&format);
    let muxer = ffmpeg_muxer_for_format(&format);
    let timestamp = Local::now().naive_local();
    let file_stem = timestamp.format("%H%M%S").to_string();
    let dir = managed_output_dir(settings, task_id, &logical_format);
    let mut path = dir.join(format!("{file_stem}.{extension}"));
    let mut suffix = 1_u32;
    while path.exists() {
        path = dir.join(format!("{file_stem}-{suffix:02}.{extension}"));
        suffix += 1;
    }

    let target = path.to_string_lossy().to_string();
    let output_args = if format.eq_ignore_ascii_case("hls") {
        hls_output_args(&target, settings.hls_record_segment_sec)
    } else {
        Vec::new()
    };

    Ok(PublishOutput {
        success_check: SuccessCheck::FileExists(PathBuf::from(&target)),
        target,
        format: logical_format,
        muxer,
        output_args,
    })
}

pub(crate) fn hls_record_segment_sec(settings: &AgentSettings, spec: &TaskSpec) -> u32 {
    spec.record
        .segment_sec
        .filter(|value| *value > 0)
        .unwrap_or(settings.hls_record_segment_sec)
}

pub(crate) fn allocate_managed_file_output(
    settings: &AgentSettings,
    task_id: Uuid,
    publish: &PublishSpec,
) -> Result<PublishOutput, ExecutorError> {
    if publish
        .url
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
    {
        return Err(ExecutorError::InvalidRequest(
            "publish.url must not be provided for file output; output path is managed by the platform".to_string(),
        ));
    }

    allocate_managed_output(settings, task_id, publish.format.as_deref())
}
