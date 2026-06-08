#[cfg(test)]
#[path = "tests/task.rs"]
mod tests;

use std::{
    fmt,
    path::{Component, Path},
    str::FromStr,
};

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
    Reclaiming,
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
            Self::Reclaiming => "RECLAIMING",
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
            "RECLAIMING" => Ok(Self::Reclaiming),
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
    Ftp,
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
            Self::Ftp => "ftp",
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
                | Self::Ftp
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
            Self::Ftp | Self::HttpMp4 | Self::File => Some(SourceMode::Vod),
            Self::Hls | Self::HttpTs => None,
        }
    }
}

pub const MANAGED_FILE_INPUT_ROOT: &str = "/data/media/work";

fn validate_ftp_input_url(value: &str) -> Result<(), &'static str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("must be provided for ftp input");
    }

    let lowered = trimmed.to_ascii_lowercase();
    if lowered.starts_with("ftps://") {
        return Err("ftps:// is not supported; use ftp://");
    }
    if !lowered.starts_with("ftp://") {
        return Err("must start with ftp:// for ftp input");
    }

    Ok(())
}

fn contains_whitespace(value: &str) -> bool {
    value.chars().any(char::is_whitespace)
}

pub fn normalize_relative_file_input_path(value: &str) -> Result<String, &'static str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("must be provided for file input");
    }

    let stripped = trimmed.trim_start_matches('/');
    if stripped.is_empty() {
        return Err("must be a relative path under /data/media/work");
    }
    if stripped.contains("://") {
        return Err("must be a relative path under /data/media/work, not a URL");
    }

    let mut segments = Vec::new();
    for component in Path::new(stripped).components() {
        match component {
            Component::Normal(segment) => segments.push(segment.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir => return Err("must not contain '..' segments"),
            Component::RootDir | Component::Prefix(_) => {
                return Err("must be a relative path under /data/media/work");
            }
        }
    }

    if segments.is_empty() {
        return Err("must be a relative path under /data/media/work");
    }

    Ok(segments.join("/"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublishTargetKind {
    File,
    UdpMpegtsMulticast,
    RtpMulticast,
    RtmpPush,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamIngestRecordMode {
    Realtime,
    Fast,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingControlSpec {
    #[serde(default)]
    pub format: Option<RecordFormat>,
    #[serde(default)]
    pub duration_sec: Option<u32>,
    #[serde(default)]
    pub segment_sec: Option<u32>,
    #[serde(default)]
    pub as_player: Option<bool>,
}

impl StreamIngestRecordMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Realtime => "realtime",
            Self::Fast => "fast",
        }
    }
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

        // resolved() 是任务入库前后的统一归一化入口，只补齐缺省值，不改变用户显式选择。
        if resolved.input.source_mode.is_none() {
            resolved.input.source_mode =
                resolved.input.kind.and_then(InputKind::default_source_mode);
        }
        resolved.input.loop_enabled = Some(resolved.input.loop_enabled.unwrap_or(false));
        if resolved.task_type == TaskType::StreamIngest {
            // stream_ingest 默认暴露内部 live 流，并把播放协议开关补齐为确定值。
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
            resolved.expose.enable_http_fmp4 =
                Some(resolved.expose.enable_http_fmp4.unwrap_or(true));
            resolved.expose.enable_hls = Some(resolved.expose.enable_hls.unwrap_or(false));
            resolved.expose.stop_on_no_reader =
                Some(resolved.expose.stop_on_no_reader.unwrap_or(false));
            resolved.record.enabled = Some(resolved.record.enabled.unwrap_or(false));
            // stream_ingest 的录像输出路径由系统统一托管，客户端传入的 save_path 一律忽略。
            resolved.record.save_path = None;
        }
        if matches!(resolved.input.kind, Some(InputKind::File)) {
            // file 输入只允许 work_root 下的相对路径；这里先规范前导斜杠和当前目录片段。
            if let Some(url) = resolved.input.url.as_deref() {
                if let Ok(normalized) = normalize_relative_file_input_path(url) {
                    resolved.input.url = Some(normalized);
                }
            }
        }
        if matches!(resolved.input.kind, Some(InputKind::GbRtp)) {
            resolved.input.tcp_mode = Some(resolved.input.tcp_mode.unwrap_or(0));
        }
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

    pub fn stream_ingest_record_mode(&self) -> Option<StreamIngestRecordMode> {
        // VOD 接入录像有两条路径：需要对外播放时实时推流录制，否则直接快速落盘。
        if self.task_type != TaskType::StreamIngest
            || self.input.source_mode != Some(SourceMode::Vod)
            || !self.record.enabled.unwrap_or(false)
        {
            return None;
        }

        if self.expose.any_playback_enabled() {
            Some(StreamIngestRecordMode::Realtime)
        } else {
            Some(StreamIngestRecordMode::Fast)
        }
    }

    pub fn stream_ingest_is_continuous(&self) -> bool {
        // 未设置时长的直播任务需要视为连续任务，异常退出时由恢复逻辑接管。
        if self.task_type != TaskType::StreamIngest {
            return false;
        }

        match self.input.source_mode {
            Some(SourceMode::Live) => self.record.duration_sec.is_none(),
            Some(SourceMode::Vod) => {
                self.input.loop_enabled.unwrap_or(false)
                    && self.expose.any_playback_enabled()
                    && self.record.duration_sec.is_none()
            }
            None => false,
        }
    }

    pub fn stream_ingest_uses_sticky_reconnect(&self) -> bool {
        // sticky reconnect 只用于可长期保活的接入任务，有限时长录像不参与自动粘连恢复。
        if self.task_type != TaskType::StreamIngest
            || self.record.duration_sec.is_some()
            || self
                .recovery
                .policy
                .unwrap_or(RecoveryPolicy::default_for(self.task_type))
                != RecoveryPolicy::Auto
        {
            return false;
        }

        match self.input.source_mode {
            Some(SourceMode::Live) => true,
            Some(SourceMode::Vod) => {
                self.input.loop_enabled.unwrap_or(false) && self.expose.any_playback_enabled()
            }
            None => false,
        }
    }

    pub fn supports_runtime_recording_control(&self) -> bool {
        if self.task_type != TaskType::StreamIngest {
            return false;
        }

        match self.input.source_mode {
            Some(SourceMode::Live) => true,
            Some(SourceMode::Vod) => self.expose.any_playback_enabled(),
            None => false,
        }
    }

    pub fn stream_ingest_uses_wall_clock_record_duration(&self) -> bool {
        self.task_type == TaskType::StreamIngest
            && self.record.enabled.unwrap_or(false)
            && self.record.duration_sec.is_some()
            && (self.input.source_mode == Some(SourceMode::Live)
                || self.stream_ingest_record_mode() == Some(StreamIngestRecordMode::Realtime))
    }

    pub fn stream_ingest_requires_realtime_pacing(&self) -> bool {
        self.task_type == TaskType::StreamIngest && self.input.source_mode == Some(SourceMode::Vod)
    }

    pub fn validate(&self) -> Result<(), TaskValidationError> {
        let mut issues = Vec::new();
        let resolved = self.resolved();

        // 基础身份字段先校验，后面的类型分支会复用 name/priority/common.created_by
        // 作为任务可审计性和调度排序的前提。
        if self.name.trim().is_empty() {
            issues.push(ValidationIssue::new("name", "must not be empty"));
        } else if contains_whitespace(&self.name) {
            issues.push(ValidationIssue::new("name", "must not contain whitespace"));
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
        if self
            .publish
            .format
            .as_deref()
            .map(str::trim)
            .is_some_and(|format| format.eq_ignore_ascii_case("webm"))
        {
            issues.push(ValidationIssue::new(
                "publish.format",
                "webm output is temporarily disabled; upload webm inputs remain supported",
            ));
        }

        // 录像时长属于 stream_ingest 的运行时控制字段；未启用 record 或非接入任务
        // 配置该字段都会让执行计划产生歧义。
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

        // input.kind 决定后续执行器选择，必须在这里把 URL、文件相对路径、
        // 组播地址和 GB RTP 端口等互斥输入约束一次性收齐。
        match self.input.kind {
            None => issues.push(ValidationIssue::new("input.kind", "must be provided")),
            Some(InputKind::File) => match self.input.url.as_deref() {
                Some(value) => {
                    if let Err(message) = normalize_relative_file_input_path(value) {
                        issues.push(ValidationIssue::new("input.url", message));
                    }
                }
                None => issues.push(ValidationIssue::new(
                    "input.url",
                    "must be provided for file input",
                )),
            },
            Some(InputKind::Ftp) => match self.input.url.as_deref() {
                Some(value) => {
                    if let Err(message) = validate_ftp_input_url(value) {
                        issues.push(ValidationIssue::new("input.url", message));
                    }
                }
                None => issues.push(ValidationIssue::new(
                    "input.url",
                    "must be provided for ftp input",
                )),
            },
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

        // source_mode 是输入族的语义补充：部分协议只能是 live/vod 中的固定值，
        // 不校验会让后续恢复、循环播放和录制模式推断互相冲突。
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

        // 循环播放只允许 VOD 接入；直播或组播循环没有稳定的 EOF 语义，
        // 也不能用于非 stream_ingest 任务。
        if self.input.loop_enabled.unwrap_or(false) {
            if self.task_type != TaskType::StreamIngest {
                issues.push(ValidationIssue::new(
                    "input.loop_enabled",
                    "is only supported for stream_ingest",
                ));
            }
            if self.input.source_mode != Some(SourceMode::Vod) {
                issues.push(ValidationIssue::new(
                    "input.loop_enabled",
                    "requires source_mode=vod",
                ));
            }
            if !matches!(
                self.input.kind,
                Some(InputKind::File | InputKind::HttpMp4 | InputKind::Hls | InputKind::HttpTs)
            ) {
                issues.push(ValidationIssue::new(
                    "input.loop_enabled",
                    "requires file, http_mp4, hls(vod), or http_ts(vod) input",
                ));
            }
        }

        // 快速录像绕过实时播放链路，如果 loop_enabled 又没有时长限制，
        // 执行器会生成无法自然结束的快速落盘任务。
        if resolved.stream_ingest_record_mode() == Some(StreamIngestRecordMode::Fast)
            && resolved.input.loop_enabled.unwrap_or(false)
            && resolved.record.duration_sec.is_none()
        {
            issues.push(ValidationIssue::new(
                "record.duration_sec",
                "is required for fast recording when input.loop_enabled=true",
            ));
        }

        // 任务类型是最终的能力边界：同一字段在 stream_ingest、stream_bridge、
        // file_transcode 下含义不同，类型分支负责禁止跨能力误用。
        match self.task_type {
            TaskType::StreamIngest => {
                if !matches!(
                    self.input.kind,
                    Some(
                        InputKind::Rtsp
                            | InputKind::Rtmp
                            | InputKind::Hls
                            | InputKind::Ftp
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
                for (field, value) in [
                    ("stream.app", self.stream.app.as_deref()),
                    ("stream.name", self.stream.name.as_deref()),
                    ("stream.vhost", self.stream.vhost.as_deref()),
                ] {
                    if let Some(value) = value {
                        if value.trim().is_empty() {
                            issues.push(ValidationIssue::new(
                                field,
                                "must not be empty when provided",
                            ));
                        } else if contains_whitespace(value) {
                            issues.push(ValidationIssue::new(
                                field,
                                "must not contain whitespace when provided",
                            ));
                        }
                    }
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
                            .is_some_and(|value| !value.is_empty())
                        {
                            issues.push(ValidationIssue::new(
                                "publish.url",
                                "must not be provided for file publish; output path is managed by the platform",
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
                    Some(PublishTargetKind::RtmpPush) => {
                        let publish_url = self
                            .publish
                            .url
                            .as_deref()
                            .map(str::trim)
                            .filter(|value| !value.is_empty());
                        match publish_url {
                            Some(url)
                                if url.starts_with("rtmp://") || url.starts_with("rtmps://") => {}
                            Some(_) => issues.push(ValidationIssue::new(
                                "publish.url",
                                "must start with rtmp:// or rtmps:// for rtmp_push output",
                            )),
                            None => issues.push(ValidationIssue::new(
                                "publish.url",
                                "must be provided for rtmp_push output",
                            )),
                        }
                        if let Some(format) = self.publish.format.as_deref().map(str::trim) {
                            if !format.is_empty() && format != "flv" {
                                issues.push(ValidationIssue::new(
                                    "publish.format",
                                    "rtmp_push only supports flv format",
                                ));
                            }
                        }
                        for field in [
                            (
                                "publish.group",
                                self.publish.group.as_deref().map(str::trim),
                            ),
                            (
                                "publish.interface_name",
                                self.publish.interface_name.as_deref().map(str::trim),
                            ),
                            (
                                "publish.interface_ip",
                                self.publish.interface_ip.as_deref().map(str::trim),
                            ),
                        ] {
                            if field.1.is_some_and(|value| !value.is_empty()) {
                                issues.push(ValidationIssue::new(
                                    field.0,
                                    "is not supported for rtmp_push output",
                                ));
                            }
                        }
                        for field in [
                            ("publish.port", self.publish.port.is_some()),
                            ("publish.ttl", self.publish.ttl.is_some()),
                            ("publish.reuse", self.publish.reuse.is_some()),
                            ("publish.pkt_size", self.publish.pkt_size.is_some()),
                            ("publish.dscp", self.publish.dscp.is_some()),
                            ("publish.buffer_size", self.publish.buffer_size.is_some()),
                            ("publish.fifo_size", self.publish.fifo_size.is_some()),
                        ] {
                            if field.1 {
                                issues.push(ValidationIssue::new(
                                    field.0,
                                    "is not supported for rtmp_push output",
                                ));
                            }
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
                    Some(
                        InputKind::File
                            | InputKind::Ftp
                            | InputKind::HttpMp4
                            | InputKind::Hls
                            | InputKind::HttpTs
                    )
                ) {
                    issues.push(ValidationIssue::new(
                        "input.kind",
                        "file_transcode requires file, ftp, http_mp4, hls, or http_ts input",
                    ));
                }
                if self.input.source_mode != Some(SourceMode::Vod) {
                    issues.push(ValidationIssue::new(
                        "input.source_mode",
                        "file_transcode requires source_mode=vod",
                    ));
                }
                match self.publish.kind {
                    Some(PublishTargetKind::File) => {}
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
                if self
                    .publish
                    .url
                    .as_deref()
                    .map(str::trim)
                    .is_some_and(|value| !value.is_empty())
                {
                    issues.push(ValidationIssue::new(
                        "publish.url",
                        "must not be provided for file_transcode output; output path is managed by the platform",
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
    pub loop_enabled: Option<bool>,
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

    pub fn any_playback_enabled(&self) -> bool {
        self.enable_rtsp.unwrap_or(false)
            || self.enable_rtmp.unwrap_or(false)
            || self.enable_http_ts.unwrap_or(false)
            || self.enable_http_fmp4.unwrap_or(false)
            || self.enable_hls.unwrap_or(false)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProcessSpec {
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub bitrate: Option<u32>,
    #[serde(default)]
    pub fps: Option<u32>,
    #[serde(default)]
    pub gop: Option<u32>,
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
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
