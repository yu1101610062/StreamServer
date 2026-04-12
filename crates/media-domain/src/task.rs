use std::{fmt, str::FromStr};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskType {
    StreamIngest,
    StreamBridge,
    FileTranscode,
}

impl TaskType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StreamIngest => "stream_ingest",
            Self::StreamBridge => "stream_bridge",
            Self::FileTranscode => "file_transcode",
        }
    }

    pub const fn default_worker_kind(self) -> WorkerKind {
        match self {
            Self::StreamIngest => WorkerKind::Hybrid,
            Self::StreamBridge => WorkerKind::Ffmpeg,
            Self::FileTranscode => WorkerKind::Ffmpeg,
        }
    }
}

impl fmt::Display for TaskType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TaskType {
    type Err = ParseEnumError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "stream_ingest" => Ok(Self::StreamIngest),
            "stream_bridge" => Ok(Self::StreamBridge),
            "file_transcode" => Ok(Self::FileTranscode),
            _ => Err(ParseEnumError::new("task_type", value)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskStatus {
    Created,
    Validating,
    Queued,
    Dispatching,
    Starting,
    Running,
    Stopping,
    Recovering,
    Succeeded,
    Failed,
    Canceled,
    Lost,
}

impl TaskStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "CREATED",
            Self::Validating => "VALIDATING",
            Self::Queued => "QUEUED",
            Self::Dispatching => "DISPATCHING",
            Self::Starting => "STARTING",
            Self::Running => "RUNNING",
            Self::Stopping => "STOPPING",
            Self::Recovering => "RECOVERING",
            Self::Succeeded => "SUCCEEDED",
            Self::Failed => "FAILED",
            Self::Canceled => "CANCELED",
            Self::Lost => "LOST",
        }
    }
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TaskStatus {
    type Err = ParseEnumError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "CREATED" => Ok(Self::Created),
            "VALIDATING" => Ok(Self::Validating),
            "QUEUED" => Ok(Self::Queued),
            "DISPATCHING" => Ok(Self::Dispatching),
            "STARTING" => Ok(Self::Starting),
            "RUNNING" => Ok(Self::Running),
            "STOPPING" => Ok(Self::Stopping),
            "RECOVERING" => Ok(Self::Recovering),
            "SUCCEEDED" => Ok(Self::Succeeded),
            "FAILED" => Ok(Self::Failed),
            "CANCELED" => Ok(Self::Canceled),
            "LOST" => Ok(Self::Lost),
            _ => Err(ParseEnumError::new("task_status", value)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AttemptStatus {
    Pending,
    Starting,
    Running,
    Stopping,
    Succeeded,
    Failed,
    Adopted,
    Orphaned,
}

impl AttemptStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Starting => "STARTING",
            Self::Running => "RUNNING",
            Self::Stopping => "STOPPING",
            Self::Succeeded => "SUCCEEDED",
            Self::Failed => "FAILED",
            Self::Adopted => "ADOPTED",
            Self::Orphaned => "ORPHANED",
        }
    }
}

impl fmt::Display for AttemptStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AttemptStatus {
    type Err = ParseEnumError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "PENDING" => Ok(Self::Pending),
            "STARTING" => Ok(Self::Starting),
            "RUNNING" => Ok(Self::Running),
            "STOPPING" => Ok(Self::Stopping),
            "SUCCEEDED" => Ok(Self::Succeeded),
            "FAILED" => Ok(Self::Failed),
            "ADOPTED" => Ok(Self::Adopted),
            "ORPHANED" => Ok(Self::Orphaned),
            _ => Err(ParseEnumError::new("attempt_status", value)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerKind {
    ZlmProxy,
    Ffmpeg,
    ZlmRtpServer,
    Hybrid,
}

impl WorkerKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ZlmProxy => "zlm_proxy",
            Self::Ffmpeg => "ffmpeg",
            Self::ZlmRtpServer => "zlm_rtp_server",
            Self::Hybrid => "hybrid",
        }
    }
}

impl fmt::Display for WorkerKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for WorkerKind {
    type Err = ParseEnumError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "zlm_proxy" => Ok(Self::ZlmProxy),
            "ffmpeg" => Ok(Self::Ffmpeg),
            "zlm_rtp_server" => Ok(Self::ZlmRtpServer),
            "hybrid" => Ok(Self::Hybrid),
            _ => Err(ParseEnumError::new("worker_kind", value)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSource {
    Core,
    Agent,
    Ffmpeg,
    ZlmApi,
    ZlmHook,
    Scheduler,
    User,
}

impl EventSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::Agent => "agent",
            Self::Ffmpeg => "ffmpeg",
            Self::ZlmApi => "zlm_api",
            Self::ZlmHook => "zlm_hook",
            Self::Scheduler => "scheduler",
            Self::User => "user",
        }
    }
}

impl fmt::Display for EventSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EventSource {
    type Err = ParseEnumError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "core" => Ok(Self::Core),
            "agent" => Ok(Self::Agent),
            "ffmpeg" => Ok(Self::Ffmpeg),
            "zlm_api" => Ok(Self::ZlmApi),
            "zlm_hook" => Ok(Self::ZlmHook),
            "scheduler" => Ok(Self::Scheduler),
            "user" => Ok(Self::User),
            _ => Err(ParseEnumError::new("event_source", value)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    Rtsp,
    Rtmp,
    Hls,
    HttpMp4,
    HttpFlv,
    HttpTs,
    File,
    UdpMpegtsMulticast,
    RtpMulticast,
    GbRtp,
}

impl InputKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rtsp => "rtsp",
            Self::Rtmp => "rtmp",
            Self::Hls => "hls",
            Self::HttpMp4 => "http_mp4",
            Self::HttpFlv => "http_flv",
            Self::HttpTs => "http_ts",
            Self::File => "file",
            Self::UdpMpegtsMulticast => "udp_mpegts_multicast",
            Self::RtpMulticast => "rtp_multicast",
            Self::GbRtp => "gb_rtp",
        }
    }

    pub const fn is_url_based(self) -> bool {
        matches!(
            self,
            Self::Rtsp
                | Self::Rtmp
                | Self::Hls
                | Self::HttpMp4
                | Self::HttpFlv
                | Self::HttpTs
                | Self::File
        )
    }

    pub const fn default_source_mode(self) -> Option<SourceMode> {
        match self {
            Self::Rtsp
            | Self::Rtmp
            | Self::HttpFlv
            | Self::UdpMpegtsMulticast
            | Self::RtpMulticast
            | Self::GbRtp => Some(SourceMode::Live),
            Self::HttpMp4 | Self::File => Some(SourceMode::Vod),
            Self::Hls | Self::HttpTs => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublishTargetKind {
    File,
    UdpMpegtsMulticast,
    RtpMulticast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceMode {
    Live,
    Vod,
}

impl SourceMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Vod => "vod",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartMode {
    Immediate,
    Manual,
    Cron,
    At,
}

impl StartMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Immediate => "immediate",
            Self::Manual => "manual",
            Self::Cron => "cron",
            Self::At => "at",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPolicy {
    Never,
    #[serde(alias = "on_failure", alias = "always")]
    Auto,
}

impl RecoveryPolicy {
    pub const fn default_for(_task_type: TaskType) -> Self {
        Self::Auto
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordFormat {
    Mp4,
    Hls,
    Both,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskSpec {
    #[serde(rename = "type")]
    pub task_type: TaskType,
    pub name: String,
    #[serde(default = "default_priority")]
    pub priority: u8,
    #[serde(default)]
    pub common: CommonSpec,
    #[serde(default)]
    pub input: InputSpec,
    #[serde(default)]
    pub stream: StreamSpec,
    #[serde(default)]
    pub expose: ExposeSpec,
    #[serde(default)]
    pub process: ProcessSpec,
    #[serde(default)]
    pub publish: PublishSpec,
    #[serde(default)]
    pub record: RecordSpec,
    #[serde(default)]
    pub recovery: RecoverySpec,
    #[serde(default)]
    pub schedule: ScheduleSpec,
    #[serde(default)]
    pub resource: ResourceSpec,
}

impl TaskSpec {
    pub fn resolved(&self) -> Self {
        let mut resolved = self.clone();

        if resolved.input.source_mode.is_none() {
            resolved.input.source_mode =
                resolved.input.kind.and_then(InputKind::default_source_mode);
        }
        resolved.stream.app = Some(
            resolved
                .stream
                .app
                .clone()
                .unwrap_or_else(|| "live".to_string()),
        );
        resolved.stream.vhost = Some(
            resolved
                .stream
                .vhost
                .clone()
                .unwrap_or_else(|| "__defaultVhost__".to_string()),
        );
        resolved.expose.enable_rtsp = Some(resolved.expose.enable_rtsp.unwrap_or(true));
        resolved.expose.enable_rtmp = Some(resolved.expose.enable_rtmp.unwrap_or(true));
        resolved.expose.enable_http_ts = Some(resolved.expose.enable_http_ts.unwrap_or(true));
        resolved.expose.enable_http_fmp4 = Some(resolved.expose.enable_http_fmp4.unwrap_or(true));
        resolved.expose.enable_hls = Some(resolved.expose.enable_hls.unwrap_or(false));
        resolved.expose.stop_on_no_reader =
            Some(resolved.expose.stop_on_no_reader.unwrap_or(false));
        if matches!(resolved.input.kind, Some(InputKind::GbRtp)) {
            resolved.input.tcp_mode = Some(resolved.input.tcp_mode.unwrap_or(0));
        }
        resolved.record.enabled = Some(resolved.record.enabled.unwrap_or(false));
        resolved.recovery.policy = Some(
            resolved
                .recovery
                .policy
                .unwrap_or(RecoveryPolicy::default_for(resolved.task_type)),
        );
        resolved.schedule.start_mode =
            Some(resolved.schedule.start_mode.unwrap_or(StartMode::Immediate));

        resolved
    }

    pub fn initial_status(&self) -> TaskStatus {
        match self
            .resolved()
            .schedule
            .start_mode
            .unwrap_or(StartMode::Immediate)
        {
            StartMode::Manual => TaskStatus::Created,
            StartMode::Immediate | StartMode::Cron | StartMode::At => TaskStatus::Validating,
        }
    }

    pub fn created_by(&self) -> Option<&str> {
        self.common.created_by.as_deref()
    }

    pub fn validate(&self) -> Result<(), TaskValidationError> {
        let mut issues = Vec::new();

        if self.name.trim().is_empty() {
            issues.push(ValidationIssue::new("name", "must not be empty"));
        }
        if self.priority > 100 {
            issues.push(ValidationIssue::new(
                "priority",
                "must be between 0 and 100",
            ));
        }
        if self
            .common
            .created_by
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .is_none()
        {
            issues.push(ValidationIssue::new(
                "common.created_by",
                "must be provided",
            ));
        }

        if let Some(duration_sec) = self.record.duration_sec {
            if !self.record.enabled.unwrap_or(false) {
                issues.push(ValidationIssue::new(
                    "record.duration_sec",
                    "requires record.enabled=true",
                ));
            }
            if duration_sec == 0 {
                issues.push(ValidationIssue::new(
                    "record.duration_sec",
                    "must be greater than 0",
                ));
            }
            if self.task_type != TaskType::StreamIngest {
                issues.push(ValidationIssue::new(
                    "record.duration_sec",
                    "is only supported for stream_ingest",
                ));
            }
        }

        match self.input.kind {
            None => issues.push(ValidationIssue::new("input.kind", "must be provided")),
            Some(kind) if kind.is_url_based() => {
                if self
                    .input
                    .url
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .is_none()
                {
                    issues.push(ValidationIssue::new(
                        "input.url",
                        "must be provided for the selected input kind",
                    ));
                }
            }
            Some(InputKind::UdpMpegtsMulticast | InputKind::RtpMulticast) => {
                if self
                    .input
                    .group
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .is_none()
                {
                    issues.push(ValidationIssue::new(
                        "input.group",
                        "must be provided for multicast input",
                    ));
                }
                if self.input.port.is_none() {
                    issues.push(ValidationIssue::new(
                        "input.port",
                        "must be provided for multicast input",
                    ));
                }
            }
            Some(InputKind::GbRtp) => {
                if self.input.port.is_none() {
                    issues.push(ValidationIssue::new(
                        "input.port",
                        "must be provided for gb_rtp input",
                    ));
                }
                if self.input.tcp_mode.is_some_and(|mode| mode > 2) {
                    issues.push(ValidationIssue::new(
                        "input.tcp_mode",
                        "must be one of 0 (udp), 1 (tcp_passive), 2 (tcp_active)",
                    ));
                }
            }
            Some(_) => {}
        }

        match (self.input.kind, self.input.source_mode) {
            (Some(InputKind::Hls | InputKind::HttpTs), None) => issues.push(ValidationIssue::new(
                "input.source_mode",
                "must be provided for hls and http_ts input",
            )),
            (Some(kind), Some(mode)) => {
                if let Some(expected) = kind.default_source_mode() {
                    if expected != mode {
                        issues.push(ValidationIssue::new(
                            "input.source_mode",
                            format!(
                                "{} input requires source_mode={}",
                                kind.as_str(),
                                expected.as_str()
                            ),
                        ));
                    }
                }
            }
            _ => {}
        }

        match self.task_type {
            TaskType::StreamIngest => {
                if !matches!(
                    self.input.kind,
                    Some(
                        InputKind::Rtsp
                            | InputKind::Rtmp
                            | InputKind::Hls
                            | InputKind::HttpFlv
                            | InputKind::HttpTs
                            | InputKind::HttpMp4
                            | InputKind::File
                            | InputKind::UdpMpegtsMulticast
                            | InputKind::RtpMulticast
                            | InputKind::GbRtp
                    )
                ) {
                    issues.push(ValidationIssue::new(
                        "input.kind",
                        "stream_ingest requires a supported ingest input kind",
                    ));
                }
                if self.publish.is_configured() {
                    issues.push(ValidationIssue::new(
                        "publish.kind",
                        "stream_ingest does not accept publish settings",
                    ));
                }
                if self
                    .stream
                    .app
                    .as_deref()
                    .is_some_and(|value| value.trim().is_empty())
                {
                    issues.push(ValidationIssue::new(
                        "stream.app",
                        "must not be empty when provided",
                    ));
                }
                if self
                    .stream
                    .name
                    .as_deref()
                    .is_some_and(|value| value.trim().is_empty())
                {
                    issues.push(ValidationIssue::new(
                        "stream.name",
                        "must not be empty when provided",
                    ));
                }
                if self
                    .stream
                    .vhost
                    .as_deref()
                    .is_some_and(|value| value.trim().is_empty())
                {
                    issues.push(ValidationIssue::new(
                        "stream.vhost",
                        "must not be empty when provided",
                    ));
                }
            }
            TaskType::StreamBridge => {
                if self.input.kind == Some(InputKind::GbRtp) {
                    issues.push(ValidationIssue::new(
                        "input.kind",
                        "stream_bridge does not accept gb_rtp input",
                    ));
                }
                match self.publish.kind {
                    Some(PublishTargetKind::File) => {
                        if self.input.source_mode == Some(SourceMode::Vod) {
                            issues.push(ValidationIssue::new(
                                "publish.kind",
                                "stream_bridge does not support file output for vod input; use file_transcode instead",
                            ));
                        }
                        if self
                            .publish
                            .url
                            .as_deref()
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .is_none()
                        {
                            issues.push(ValidationIssue::new(
                                "publish.url",
                                "must be provided for file publish",
                            ));
                        }
                    }
                    Some(
                        PublishTargetKind::UdpMpegtsMulticast | PublishTargetKind::RtpMulticast,
                    ) => {
                        if self
                            .publish
                            .group
                            .as_deref()
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .is_none()
                        {
                            issues.push(ValidationIssue::new(
                                "publish.group",
                                "must be provided for multicast publish",
                            ));
                        }
                        if self.publish.port.is_none() {
                            issues.push(ValidationIssue::new(
                                "publish.port",
                                "must be provided for multicast publish",
                            ));
                        }
                    }
                    None => issues.push(ValidationIssue::new(
                        "publish.kind",
                        "must be provided for stream_bridge",
                    )),
                }
                if self.stream.is_configured() {
                    issues.push(ValidationIssue::new(
                        "stream",
                        "stream_bridge does not accept stream settings",
                    ));
                }
                if self.expose.is_configured() {
                    issues.push(ValidationIssue::new(
                        "expose",
                        "stream_bridge does not accept expose settings",
                    ));
                }
                if self.record.is_configured() {
                    issues.push(ValidationIssue::new(
                        "record",
                        "stream_bridge does not accept recording settings",
                    ));
                }
            }
            TaskType::FileTranscode => {
                if !matches!(
                    self.input.kind,
                    Some(InputKind::File | InputKind::HttpMp4 | InputKind::Hls | InputKind::HttpTs)
                ) {
                    issues.push(ValidationIssue::new(
                        "input.kind",
                        "file_transcode requires file, http_mp4, hls, or http_ts input",
                    ));
                }
                if self.input.source_mode != Some(SourceMode::Vod) {
                    issues.push(ValidationIssue::new(
                        "input.source_mode",
                        "file_transcode requires source_mode=vod",
                    ));
                }
                match self.publish.kind {
                    Some(PublishTargetKind::File) => {
                        if self
                            .publish
                            .url
                            .as_deref()
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .is_none()
                        {
                            issues.push(ValidationIssue::new(
                                "publish.url",
                                "must be provided for file_transcode output",
                            ));
                        }
                    }
                    Some(_) => issues.push(ValidationIssue::new(
                        "publish.kind",
                        "file_transcode requires file output",
                    )),
                    None => issues.push(ValidationIssue::new(
                        "publish.kind",
                        "must be provided for file_transcode",
                    )),
                }
                if self.stream.is_configured() {
                    issues.push(ValidationIssue::new(
                        "stream",
                        "file_transcode does not accept stream settings",
                    ));
                }
                if self.expose.is_configured() {
                    issues.push(ValidationIssue::new(
                        "expose",
                        "file_transcode does not accept expose settings",
                    ));
                }
                if self.record.is_configured() {
                    issues.push(ValidationIssue::new(
                        "record",
                        "file_transcode does not accept recording settings",
                    ));
                }
            }
        }

        if issues.is_empty() {
            Ok(())
        } else {
            Err(TaskValidationError { issues })
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CommonSpec {
    #[serde(default)]
    pub created_by: Option<String>,
    #[serde(default)]
    pub callback_url: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InputSpec {
    #[serde(default)]
    pub kind: Option<InputKind>,
    #[serde(default)]
    pub source_mode: Option<SourceMode>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub interface_name: Option<String>,
    #[serde(default)]
    pub interface_ip: Option<String>,
    #[serde(default)]
    pub ttl: Option<u8>,
    #[serde(default)]
    pub reuse: Option<bool>,
    #[serde(default)]
    pub pkt_size: Option<u16>,
    #[serde(default)]
    pub dscp: Option<u8>,
    #[serde(default)]
    pub buffer_size: Option<u32>,
    #[serde(default)]
    pub fifo_size: Option<u32>,
    #[serde(default)]
    pub probe_timeout_ms: Option<u64>,
    #[serde(default)]
    pub tcp_mode: Option<u8>,
    #[serde(default)]
    pub ssrc: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StreamSpec {
    #[serde(default)]
    pub app: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub vhost: Option<String>,
}

impl StreamSpec {
    pub fn is_configured(&self) -> bool {
        self.app.is_some() || self.name.is_some() || self.vhost.is_some()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExposeSpec {
    #[serde(default)]
    pub enable_rtsp: Option<bool>,
    #[serde(default)]
    pub enable_rtmp: Option<bool>,
    #[serde(default)]
    pub enable_http_ts: Option<bool>,
    #[serde(default)]
    pub enable_http_fmp4: Option<bool>,
    #[serde(default)]
    pub enable_hls: Option<bool>,
    #[serde(default)]
    pub stop_on_no_reader: Option<bool>,
}

impl ExposeSpec {
    pub fn is_configured(&self) -> bool {
        self.enable_rtsp.is_some()
            || self.enable_rtmp.is_some()
            || self.enable_http_ts.is_some()
            || self.enable_http_fmp4.is_some()
            || self.enable_hls.is_some()
            || self.stop_on_no_reader.is_some()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProcessSpec {
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub video_codec: Option<String>,
    #[serde(default)]
    pub audio_codec: Option<String>,
    #[serde(default)]
    pub bitrate: Option<u32>,
    #[serde(default)]
    pub fps: Option<u32>,
    #[serde(default)]
    pub gop: Option<u32>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub preset: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PublishSpec {
    #[serde(default)]
    pub kind: Option<PublishTargetKind>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub interface_name: Option<String>,
    #[serde(default)]
    pub interface_ip: Option<String>,
    #[serde(default)]
    pub ttl: Option<u8>,
    #[serde(default)]
    pub reuse: Option<bool>,
    #[serde(default)]
    pub pkt_size: Option<u16>,
    #[serde(default)]
    pub dscp: Option<u8>,
    #[serde(default)]
    pub buffer_size: Option<u32>,
    #[serde(default)]
    pub fifo_size: Option<u32>,
    #[serde(default)]
    pub format: Option<String>,
}

impl PublishSpec {
    pub fn is_configured(&self) -> bool {
        self.kind.is_some()
            || self.url.is_some()
            || self.group.is_some()
            || self.port.is_some()
            || self.interface_name.is_some()
            || self.interface_ip.is_some()
            || self.ttl.is_some()
            || self.reuse.is_some()
            || self.pkt_size.is_some()
            || self.dscp.is_some()
            || self.buffer_size.is_some()
            || self.fifo_size.is_some()
            || self.format.is_some()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RecordSpec {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub format: Option<RecordFormat>,
    #[serde(default)]
    pub duration_sec: Option<u32>,
    #[serde(default)]
    pub segment_sec: Option<u32>,
    #[serde(default)]
    pub save_path: Option<String>,
    #[serde(default)]
    pub as_player: Option<bool>,
    #[serde(default)]
    pub archive_policy: Option<String>,
    #[serde(default)]
    pub retention_days: Option<u16>,
}

impl RecordSpec {
    pub fn wants_mp4(&self) -> bool {
        self.enabled.unwrap_or(false)
            && matches!(
                self.format.unwrap_or(RecordFormat::Mp4),
                RecordFormat::Mp4 | RecordFormat::Both
            )
    }

    pub fn wants_hls(&self) -> bool {
        self.enabled.unwrap_or(false)
            && matches!(
                self.format.unwrap_or(RecordFormat::Mp4),
                RecordFormat::Hls | RecordFormat::Both
            )
    }

    pub fn is_configured(&self) -> bool {
        self.enabled.is_some()
            || self.format.is_some()
            || self.duration_sec.is_some()
            || self.segment_sec.is_some()
            || self.save_path.is_some()
            || self.as_player.is_some()
            || self.archive_policy.is_some()
            || self.retention_days.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecoverySpec {
    #[serde(default)]
    pub policy: Option<RecoveryPolicy>,
    #[serde(default)]
    pub resume_mode: Option<String>,
    #[serde(default)]
    pub backoff: Option<BackoffPolicy>,
    #[serde(default)]
    pub max_consecutive_failures: Option<u32>,
}

impl Default for RecoverySpec {
    fn default() -> Self {
        Self {
            policy: None,
            resume_mode: None,
            backoff: None,
            max_consecutive_failures: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScheduleSpec {
    #[serde(default)]
    pub start_mode: Option<StartMode>,
    #[serde(default)]
    pub start_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub cron: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResourceSpec {
    #[serde(default)]
    pub required_labels: Vec<String>,
    #[serde(default)]
    pub preferred_labels: Vec<String>,
    #[serde(default)]
    pub need_gpu: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackoffPolicy {
    #[serde(default)]
    pub initial_delay_sec: Option<u32>,
    #[serde(default)]
    pub max_delay_sec: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValidationIssue {
    pub field: &'static str,
    pub message: String,
}

impl ValidationIssue {
    pub fn new(field: &'static str, message: impl Into<String>) -> Self {
        Self {
            field,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Error)]
#[error("task validation failed")]
pub struct TaskValidationError {
    pub issues: Vec<ValidationIssue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("failed to parse {kind}: {value}")]
pub struct ParseEnumError {
    kind: &'static str,
    value: String,
}

impl ParseEnumError {
    pub fn new(kind: &'static str, value: impl Into<String>) -> Self {
        Self {
            kind,
            value: value.into(),
        }
    }
}

const fn default_priority() -> u8 {
    50
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_task(task_type: TaskType) -> TaskSpec {
        TaskSpec {
            task_type,
            name: "relay-camera-01".to_string(),
            priority: 50,
            common: CommonSpec {
                created_by: Some("alice".to_string()),
                callback_url: None,
                labels: Vec::new(),
            },
            input: InputSpec {
                kind: Some(InputKind::Rtsp),
                url: Some("rtsp://camera.example/live".to_string()),
                source_mode: Some(SourceMode::Live),
                ..InputSpec::default()
            },
            stream: StreamSpec::default(),
            expose: ExposeSpec::default(),
            process: ProcessSpec::default(),
            publish: PublishSpec::default(),
            record: RecordSpec::default(),
            recovery: RecoverySpec::default(),
            schedule: ScheduleSpec::default(),
            resource: ResourceSpec::default(),
        }
    }

    #[test]
    fn resolve_applies_documented_defaults() {
        let resolved = sample_task(TaskType::StreamIngest).resolved();

        assert_eq!(resolved.stream.app.as_deref(), Some("live"));
        assert_eq!(resolved.stream.vhost.as_deref(), Some("__defaultVhost__"));
        assert_eq!(resolved.expose.enable_rtsp, Some(true));
        assert_eq!(resolved.expose.enable_rtmp, Some(true));
        assert_eq!(resolved.expose.enable_http_ts, Some(true));
        assert_eq!(resolved.expose.enable_http_fmp4, Some(true));
        assert_eq!(resolved.expose.enable_hls, Some(false));
        assert_eq!(resolved.expose.stop_on_no_reader, Some(false));
        assert_eq!(resolved.record.enabled, Some(false));
        assert_eq!(resolved.recovery.policy, Some(RecoveryPolicy::Auto));
        assert_eq!(resolved.schedule.start_mode, Some(StartMode::Immediate));
    }

    #[test]
    fn file_transcode_defaults_to_auto_recovery() {
        let resolved = sample_task(TaskType::FileTranscode).resolved();
        assert_eq!(resolved.recovery.policy, Some(RecoveryPolicy::Auto));
    }

    #[test]
    fn recovery_policy_deserializes_legacy_aliases() {
        let on_failure: RecoveryPolicy = serde_json::from_str("\"on_failure\"").unwrap();
        let always: RecoveryPolicy = serde_json::from_str("\"always\"").unwrap();
        assert_eq!(on_failure, RecoveryPolicy::Auto);
        assert_eq!(always, RecoveryPolicy::Auto);
    }

    #[test]
    fn validate_rejects_missing_input_and_creator() {
        let task = TaskSpec {
            task_type: TaskType::StreamIngest,
            name: " ".to_string(),
            priority: 101,
            common: CommonSpec::default(),
            input: InputSpec::default(),
            stream: StreamSpec::default(),
            expose: ExposeSpec::default(),
            process: ProcessSpec::default(),
            publish: PublishSpec::default(),
            record: RecordSpec::default(),
            recovery: RecoverySpec::default(),
            schedule: ScheduleSpec::default(),
            resource: ResourceSpec::default(),
        };

        let error = task.validate().expect_err("validation should fail");
        assert!(error.issues.iter().any(|issue| issue.field == "name"));
        assert!(
            error
                .issues
                .iter()
                .any(|issue| issue.field == "common.created_by")
        );
        assert!(error.issues.iter().any(|issue| issue.field == "input.kind"));
    }

    #[test]
    fn validate_allows_stream_bridge_multicast_input_without_explicit_interface_binding() {
        let task = TaskSpec {
            task_type: TaskType::StreamBridge,
            name: "bridge".to_string(),
            priority: 50,
            common: CommonSpec {
                created_by: Some("alice".to_string()),
                callback_url: None,
                labels: Vec::new(),
            },
            input: InputSpec {
                kind: Some(InputKind::UdpMpegtsMulticast),
                group: Some("239.0.0.1".to_string()),
                port: Some(1234),
                source_mode: Some(SourceMode::Live),
                ..InputSpec::default()
            },
            stream: StreamSpec::default(),
            expose: ExposeSpec::default(),
            process: ProcessSpec::default(),
            publish: PublishSpec {
                kind: Some(PublishTargetKind::File),
                url: Some("/tmp/out.ts".to_string()),
                ..PublishSpec::default()
            },
            record: RecordSpec::default(),
            recovery: RecoverySpec::default(),
            schedule: ScheduleSpec::default(),
            resource: ResourceSpec::default(),
        };

        task.validate()
            .expect("validation should allow agent-level multicast defaults");
    }

    #[test]
    fn validate_rejects_stream_ingest_with_publish_settings() {
        let mut task = sample_task(TaskType::StreamIngest);
        task.publish.kind = Some(PublishTargetKind::File);
        task.publish.url = Some("/tmp/out.ts".to_string());

        let error = task.validate().expect_err("validation should fail");
        assert!(
            error
                .issues
                .iter()
                .any(|issue| issue.field == "publish.kind")
        );
    }

    #[test]
    fn validate_rejects_stream_bridge_without_publish_target() {
        let task = TaskSpec {
            task_type: TaskType::StreamBridge,
            name: "bridge".to_string(),
            priority: 50,
            common: CommonSpec {
                created_by: Some("alice".to_string()),
                callback_url: None,
                labels: Vec::new(),
            },
            input: InputSpec {
                kind: Some(InputKind::UdpMpegtsMulticast),
                group: Some("239.0.0.1".to_string()),
                port: Some(1234),
                source_mode: Some(SourceMode::Live),
                ..InputSpec::default()
            },
            stream: StreamSpec::default(),
            expose: ExposeSpec::default(),
            process: ProcessSpec::default(),
            publish: PublishSpec::default(),
            record: RecordSpec::default(),
            recovery: RecoverySpec::default(),
            schedule: ScheduleSpec::default(),
            resource: ResourceSpec::default(),
        };

        let error = task.validate().expect_err("validation should fail");
        assert!(
            error
                .issues
                .iter()
                .any(|issue| issue.field == "publish.kind")
        );
    }

    #[test]
    fn validate_rejects_stream_bridge_gb_rtp_input() {
        let task = TaskSpec {
            task_type: TaskType::StreamBridge,
            name: "bridge".to_string(),
            priority: 50,
            common: CommonSpec {
                created_by: Some("alice".to_string()),
                callback_url: None,
                labels: Vec::new(),
            },
            input: InputSpec {
                kind: Some(InputKind::GbRtp),
                source_mode: Some(SourceMode::Live),
                port: Some(30000),
                ..InputSpec::default()
            },
            stream: StreamSpec::default(),
            expose: ExposeSpec::default(),
            process: ProcessSpec::default(),
            publish: PublishSpec {
                kind: Some(PublishTargetKind::UdpMpegtsMulticast),
                group: Some("239.0.0.1".to_string()),
                port: Some(1234),
                ..PublishSpec::default()
            },
            record: RecordSpec::default(),
            recovery: RecoverySpec::default(),
            schedule: ScheduleSpec::default(),
            resource: ResourceSpec::default(),
        };

        let error = task.validate().expect_err("validation should fail");
        assert!(error.issues.iter().any(|issue| issue.field == "input.kind"));
    }

    #[test]
    fn validate_rejects_stream_bridge_vod_file_output() {
        let task = TaskSpec {
            task_type: TaskType::StreamBridge,
            name: "vod-bridge".to_string(),
            priority: 50,
            common: CommonSpec {
                created_by: Some("alice".to_string()),
                callback_url: None,
                labels: Vec::new(),
            },
            input: InputSpec {
                kind: Some(InputKind::HttpMp4),
                source_mode: Some(SourceMode::Vod),
                url: Some("http://vod.example.com/archive.mp4".to_string()),
                ..InputSpec::default()
            },
            stream: StreamSpec::default(),
            expose: ExposeSpec::default(),
            process: ProcessSpec::default(),
            publish: PublishSpec {
                kind: Some(PublishTargetKind::File),
                url: Some("/tmp/out.mp4".to_string()),
                ..PublishSpec::default()
            },
            record: RecordSpec::default(),
            recovery: RecoverySpec::default(),
            schedule: ScheduleSpec::default(),
            resource: ResourceSpec::default(),
        };

        let error = task.validate().expect_err("validation should fail");
        assert!(
            error
                .issues
                .iter()
                .any(|issue| issue.field == "publish.kind")
        );
    }

    #[test]
    fn validate_allows_file_transcode_with_http_mp4_vod_input() {
        let task = TaskSpec {
            task_type: TaskType::FileTranscode,
            name: "file-transcode".to_string(),
            priority: 50,
            common: CommonSpec {
                created_by: Some("alice".to_string()),
                callback_url: None,
                labels: Vec::new(),
            },
            input: InputSpec {
                kind: Some(InputKind::HttpMp4),
                source_mode: Some(SourceMode::Vod),
                url: Some("http://vod.example.com/archive.mp4".to_string()),
                ..InputSpec::default()
            },
            stream: StreamSpec::default(),
            expose: ExposeSpec::default(),
            process: ProcessSpec::default(),
            publish: PublishSpec {
                kind: Some(PublishTargetKind::File),
                url: Some("/tmp/out.mp4".to_string()),
                ..PublishSpec::default()
            },
            record: RecordSpec::default(),
            recovery: RecoverySpec::default(),
            schedule: ScheduleSpec::default(),
            resource: ResourceSpec::default(),
        };

        task.validate()
            .expect("validation should allow http_mp4 file_transcode input");
    }

    #[test]
    fn validate_rejects_hls_without_source_mode() {
        let task = TaskSpec {
            task_type: TaskType::StreamIngest,
            name: "hls-ingest".to_string(),
            priority: 50,
            common: CommonSpec {
                created_by: Some("alice".to_string()),
                callback_url: None,
                labels: Vec::new(),
            },
            input: InputSpec {
                kind: Some(InputKind::Hls),
                url: Some("http://vod.example.com/index.m3u8".to_string()),
                ..InputSpec::default()
            },
            stream: StreamSpec::default(),
            expose: ExposeSpec::default(),
            process: ProcessSpec::default(),
            publish: PublishSpec::default(),
            record: RecordSpec::default(),
            recovery: RecoverySpec::default(),
            schedule: ScheduleSpec::default(),
            resource: ResourceSpec::default(),
        };

        let error = task.validate().expect_err("validation should fail");
        assert!(
            error
                .issues
                .iter()
                .any(|issue| issue.field == "input.source_mode")
        );
    }

    #[test]
    fn validate_rejects_record_duration_for_unsupported_task_types() {
        let mut task = sample_task(TaskType::StreamBridge);
        task.publish.kind = Some(PublishTargetKind::File);
        task.publish.url = Some("/tmp/out.ts".to_string());
        task.record.enabled = Some(true);
        task.record.duration_sec = Some(300);

        let error = task.validate().expect_err("validation should fail");
        assert!(
            error
                .issues
                .iter()
                .any(|issue| issue.field == "record.duration_sec")
        );
    }

    #[test]
    fn validate_rejects_non_positive_record_duration() {
        let mut task = sample_task(TaskType::StreamIngest);
        task.record.enabled = Some(true);
        task.record.duration_sec = Some(0);

        let error = task.validate().expect_err("validation should fail");
        assert!(
            error
                .issues
                .iter()
                .any(|issue| issue.field == "record.duration_sec")
        );
    }

    #[test]
    fn resolve_defaults_gb_rtp_tcp_mode_to_udp() {
        let task = TaskSpec {
            task_type: TaskType::StreamIngest,
            name: "rtp-recv".to_string(),
            priority: 50,
            common: CommonSpec {
                created_by: Some("alice".to_string()),
                callback_url: None,
                labels: Vec::new(),
            },
            input: InputSpec {
                kind: Some(InputKind::GbRtp),
                port: Some(0),
                ..InputSpec::default()
            },
            stream: StreamSpec::default(),
            expose: ExposeSpec::default(),
            process: ProcessSpec::default(),
            publish: PublishSpec::default(),
            record: RecordSpec::default(),
            recovery: RecoverySpec::default(),
            schedule: ScheduleSpec::default(),
            resource: ResourceSpec::default(),
        };

        let resolved = task.resolved();
        assert_eq!(resolved.input.tcp_mode, Some(0));
    }

    #[test]
    fn validate_rejects_file_transcode_with_stream_settings() {
        let task = TaskSpec {
            task_type: TaskType::FileTranscode,
            name: "file-transcode".to_string(),
            priority: 50,
            common: CommonSpec {
                created_by: Some("alice".to_string()),
                callback_url: None,
                labels: Vec::new(),
            },
            input: InputSpec {
                kind: Some(InputKind::File),
                source_mode: Some(SourceMode::Vod),
                url: Some("/tmp/input.mp4".to_string()),
                ..InputSpec::default()
            },
            stream: StreamSpec {
                name: Some("should-not-be-here".to_string()),
                ..StreamSpec::default()
            },
            expose: ExposeSpec::default(),
            process: ProcessSpec::default(),
            publish: PublishSpec {
                kind: Some(PublishTargetKind::File),
                url: Some("/tmp/out.mp4".to_string()),
                ..PublishSpec::default()
            },
            record: RecordSpec::default(),
            recovery: RecoverySpec::default(),
            schedule: ScheduleSpec::default(),
            resource: ResourceSpec::default(),
        };

        let error = task.validate().expect_err("validation should fail");
        assert!(error.issues.iter().any(|issue| issue.field == "stream"));
    }
}
