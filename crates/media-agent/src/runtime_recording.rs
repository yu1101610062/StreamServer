//! 实时录制状态：定义 ZLM 录制格式、录制元数据，并封装录制配置、时长和状态转换判断。

use chrono::{DateTime, Utc};
use media_domain::{RecordingControlSpec, TaskSpec};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{config::AgentSettings, runtime::ExecutorError, runtime_outputs::managed_output_dir};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ZlmRecordKind {
    Hls,
    Mp4,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LiveRelayRecording {
    pub(crate) formats: Vec<ZlmRecordKind>,
    pub(crate) root_path_mp4: Option<String>,
    pub(crate) root_path_hls: Option<String>,
    pub(crate) duration_sec: Option<u32>,
    pub(crate) segment_sec: Option<u32>,
    pub(crate) as_player: bool,
    #[serde(default = "default_true")]
    pub(crate) desired_enabled: bool,
    #[serde(default)]
    pub(crate) manual_control: bool,
    #[serde(default = "default_true")]
    pub(crate) stop_task_on_duration: bool,
    #[serde(default)]
    pub(crate) control_command_id: Option<String>,
    #[serde(default)]
    pub(crate) recording_started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub(crate) auto_stop_requested: bool,
    #[serde(default)]
    pub(crate) completion_reason: Option<String>,
    #[serde(default)]
    pub(crate) started: bool,
    #[serde(default)]
    pub(crate) failed: bool,
}

fn default_true() -> bool {
    true
}

impl LiveRelayRecording {
    pub(crate) fn root_path_for_kind(&self, kind: &ZlmRecordKind) -> Option<&str> {
        match kind {
            ZlmRecordKind::Mp4 => self.root_path_mp4.as_deref(),
            ZlmRecordKind::Hls => self.root_path_hls.as_deref(),
        }
    }

    pub(crate) fn primary_root_path(&self) -> Option<&str> {
        self.formats
            .iter()
            .find_map(|kind| self.root_path_for_kind(kind))
    }

    pub(crate) fn all_root_paths(&self) -> Vec<String> {
        self.formats
            .iter()
            .filter_map(|kind| self.root_path_for_kind(kind))
            .map(str::to_string)
            .collect()
    }

    pub(crate) fn root_paths_payload(&self) -> Value {
        json!({
            "mp4": self.root_path_mp4,
            "hls": self.root_path_hls,
        })
    }
}

pub(crate) fn build_live_relay_recording(
    settings: &AgentSettings,
    task_id: Uuid,
    spec: &TaskSpec,
) -> Result<Option<LiveRelayRecording>, ExecutorError> {
    if !spec.record.enabled.unwrap_or(false) {
        return Ok(None);
    }

    let formats = match spec
        .record
        .format
        .unwrap_or(media_domain::RecordFormat::Mp4)
    {
        media_domain::RecordFormat::Mp4 => vec![ZlmRecordKind::Mp4],
        media_domain::RecordFormat::Hls => vec![ZlmRecordKind::Hls],
        media_domain::RecordFormat::Both => vec![ZlmRecordKind::Mp4, ZlmRecordKind::Hls],
    };
    let root_path_mp4 = formats
        .iter()
        .any(|kind| matches!(kind, ZlmRecordKind::Mp4))
        .then(|| {
            managed_output_dir(settings, task_id, "mp4")
                .to_string_lossy()
                .to_string()
        });
    let root_path_hls = formats
        .iter()
        .any(|kind| matches!(kind, ZlmRecordKind::Hls))
        .then(|| {
            managed_output_dir(settings, task_id, "hls")
                .to_string_lossy()
                .to_string()
        });

    Ok(Some(LiveRelayRecording {
        formats,
        root_path_mp4,
        root_path_hls,
        duration_sec: spec.record.duration_sec,
        segment_sec: spec.record.segment_sec,
        as_player: spec.record.as_player.unwrap_or(false),
        desired_enabled: true,
        manual_control: false,
        stop_task_on_duration: true,
        control_command_id: None,
        recording_started_at: None,
        auto_stop_requested: false,
        completion_reason: None,
        started: false,
        failed: false,
    }))
}

pub(crate) fn build_manual_live_relay_recording(
    settings: &AgentSettings,
    task_id: Uuid,
    spec: &TaskSpec,
    control: Option<&RecordingControlSpec>,
    command_id: &str,
) -> LiveRelayRecording {
    let format = control
        .and_then(|control| control.format)
        .or(spec.record.format)
        .unwrap_or(media_domain::RecordFormat::Mp4);
    let formats = record_kinds_from_format(format);
    let root_path_mp4 = formats
        .iter()
        .any(|kind| matches!(kind, ZlmRecordKind::Mp4))
        .then(|| {
            managed_output_dir(settings, task_id, "mp4")
                .to_string_lossy()
                .to_string()
        });
    let root_path_hls = formats
        .iter()
        .any(|kind| matches!(kind, ZlmRecordKind::Hls))
        .then(|| {
            managed_output_dir(settings, task_id, "hls")
                .to_string_lossy()
                .to_string()
        });

    LiveRelayRecording {
        formats,
        root_path_mp4,
        root_path_hls,
        duration_sec: control.and_then(|control| control.duration_sec),
        segment_sec: control
            .and_then(|control| control.segment_sec)
            .or(spec.record.segment_sec),
        as_player: control
            .and_then(|control| control.as_player)
            .or(spec.record.as_player)
            .unwrap_or(false),
        desired_enabled: true,
        manual_control: true,
        stop_task_on_duration: false,
        control_command_id: Some(command_id.to_string()),
        recording_started_at: None,
        auto_stop_requested: false,
        completion_reason: None,
        started: false,
        failed: false,
    }
}

fn record_kinds_from_format(format: media_domain::RecordFormat) -> Vec<ZlmRecordKind> {
    match format {
        media_domain::RecordFormat::Mp4 => vec![ZlmRecordKind::Mp4],
        media_domain::RecordFormat::Hls => vec![ZlmRecordKind::Hls],
        media_domain::RecordFormat::Both => vec![ZlmRecordKind::Mp4, ZlmRecordKind::Hls],
    }
}

pub(crate) fn recording_config_matches(
    existing: &LiveRelayRecording,
    requested: &LiveRelayRecording,
) -> bool {
    existing.formats == requested.formats
        && existing.duration_sec == requested.duration_sec
        && existing.segment_sec == requested.segment_sec
        && existing.as_player == requested.as_player
}

pub(crate) fn should_start_live_relay_recording(recording: &LiveRelayRecording) -> bool {
    recording.desired_enabled && !recording.started && !recording.failed
}

pub(crate) fn should_fail_on_recording_start_error(recording: &LiveRelayRecording) -> bool {
    let _ = recording;
    true
}

pub(crate) fn recording_duration_reached(
    recording: &LiveRelayRecording,
    now: DateTime<Utc>,
) -> bool {
    let Some(duration_sec) = recording.duration_sec else {
        return false;
    };
    let Some(started_at) = recording.recording_started_at else {
        return false;
    };
    now >= started_at + chrono::Duration::seconds(i64::from(duration_sec))
}

pub(crate) fn recording_elapsed_seconds(
    recording: &LiveRelayRecording,
    now: DateTime<Utc>,
) -> Option<f64> {
    recording.recording_started_at.and_then(|started_at| {
        now.signed_duration_since(started_at)
            .to_std()
            .ok()
            .map(|elapsed| elapsed.as_secs_f64())
    })
}

pub(crate) fn mark_recording_started(
    recording: &LiveRelayRecording,
    now: DateTime<Utc>,
) -> LiveRelayRecording {
    let mut updated = recording.clone();
    updated.desired_enabled = true;
    updated.started = true;
    updated.failed = false;
    updated.recording_started_at = Some(now);
    updated.auto_stop_requested = false;
    updated.completion_reason = None;
    updated
}

pub(crate) fn mark_recording_failed(recording: &LiveRelayRecording) -> LiveRelayRecording {
    let mut updated = recording.clone();
    updated.started = false;
    updated.failed = true;
    updated
}

pub(crate) fn mark_recording_completion(
    recording: &LiveRelayRecording,
    reason: impl Into<String>,
) -> LiveRelayRecording {
    let mut updated = recording.clone();
    updated.desired_enabled = false;
    updated.started = false;
    updated.auto_stop_requested = true;
    updated.completion_reason = Some(reason.into());
    updated
}

pub(crate) fn should_auto_stop_live_relay_recording(
    recording: &LiveRelayRecording,
    now: DateTime<Utc>,
) -> bool {
    recording.started
        && recording.stop_task_on_duration
        && !recording.auto_stop_requested
        && recording_duration_reached(recording, now)
}
