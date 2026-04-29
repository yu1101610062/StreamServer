#[cfg(test)]
#[path = "tests/node.rs"]
mod tests;

use std::{
    fmt,
    path::{Component, Path},
    str::FromStr,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{WorkerKind, task::ParseEnumError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkMode {
    Bridge,
    Host,
    Macvlan,
}

impl NetworkMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bridge => "bridge",
            Self::Host => "host",
            Self::Macvlan => "macvlan",
        }
    }
}

impl fmt::Display for NetworkMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for NetworkMode {
    type Err = ParseEnumError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "bridge" => Ok(Self::Bridge),
            "host" => Ok(Self::Host),
            "macvlan" => Ok(Self::Macvlan),
            _ => Err(ParseEnumError::new("network_mode", value)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRegistration {
    pub node_id: Uuid,
    pub node_name: String,
    pub agent_version: String,
    pub hostname: String,
    pub labels: Vec<String>,
    pub interfaces: Vec<String>,
    pub zlm_api_base: String,
    pub zlm_api_secret: String,
    pub agent_stream_addr: String,
    pub agent_http_base_url: String,
    pub zlm_rtmp_port: u16,
    pub zlm_rtsp_port: u16,
    pub network_mode: NetworkMode,
    pub ffmpeg_bin: String,
    pub ffprobe_bin: String,
    pub zlm_server_id: String,
    pub output_mount_relative_prefix_mp4: String,
    pub output_mount_relative_prefix_hls: String,
}

pub fn normalize_output_mount_relative_prefix(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }

    let path = Path::new(trimmed);
    if path.is_absolute() {
        return Err("must be a relative path".to_string());
    }

    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => parts.push(value.to_string_lossy().to_string()),
            Component::ParentDir => {
                return Err("must not contain parent segments".to_string());
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err("must be a relative path".to_string());
            }
        }
    }

    Ok(parts.join("/"))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeartbeatSnapshot {
    pub node_time: DateTime<Utc>,
    pub cpu_percent: f64,
    pub mem_percent: f64,
    pub disk_percent: f64,
    pub upload_disk_total_bytes: u64,
    pub upload_disk_available_bytes: u64,
    pub upload_disk_used_percent: f64,
    pub running_tasks: u32,
    pub starting_tasks: u32,
    pub stopping_tasks: u32,
    pub orphaned_tasks: u32,
    pub slot_usage: f64,
    pub zlm_alive: bool,
    pub ffmpeg_alive: bool,
    pub artifact_cleanup_blocked: bool,
    pub artifact_cleanup_block_reason: Option<String>,
    pub gpu_runtime: Vec<GpuRuntimeStats>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuDeviceInfo {
    pub index: u32,
    pub uuid: String,
    pub name: String,
    pub memory_total_mb: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuRuntimeStats {
    pub index: u32,
    pub gpu_util_percent: f64,
    pub memory_used_mb: u64,
    pub memory_total_mb: u64,
    pub encoder_util_percent: f64,
    pub decoder_util_percent: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilitySnapshot {
    pub ffmpeg_protocols: Vec<String>,
    pub ffmpeg_formats: Vec<String>,
    pub ffmpeg_encoders: Vec<String>,
    pub ffmpeg_decoders: Vec<String>,
    pub zlm_version: Option<String>,
    pub zlm_api_list: Vec<String>,
    pub gpu: Vec<String>,
    pub gpu_devices: Vec<GpuDeviceInfo>,
    pub captured_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeState {
    Pending,
    Starting,
    Running,
    Stopping,
    Exited,
    Orphaned,
}

impl RuntimeState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Exited => "exited",
            Self::Orphaned => "orphaned",
        }
    }
}

impl fmt::Display for RuntimeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RuntimeState {
    type Err = ParseEnumError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "pending" => Ok(Self::Pending),
            "starting" => Ok(Self::Starting),
            "running" => Ok(Self::Running),
            "stopping" => Ok(Self::Stopping),
            "exited" => Ok(Self::Exited),
            "orphaned" => Ok(Self::Orphaned),
            _ => Err(ParseEnumError::new("runtime_state", value)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeHandle {
    pub runtime_id: Uuid,
    pub task_id: Uuid,
    pub attempt_no: i32,
    pub worker_kind: WorkerKind,
    pub pid: Option<i32>,
    pub started_at: DateTime<Utc>,
    pub last_progress_at: Option<DateTime<Utc>>,
    pub state: RuntimeState,
    pub command_line: Option<String>,
    pub outputs: Vec<String>,
    pub metadata: Value,
}
