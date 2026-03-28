use std::{fmt, str::FromStr};

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
    pub agent_stream_addr: String,
    pub network_mode: NetworkMode,
    pub ffmpeg_bin: String,
    pub ffprobe_bin: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeartbeatSnapshot {
    pub node_time: DateTime<Utc>,
    pub cpu_percent: f64,
    pub mem_percent: f64,
    pub disk_percent: f64,
    pub running_tasks: u32,
    pub slot_usage: f64,
    pub zlm_alive: bool,
    pub ffmpeg_alive: bool,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_mode_roundtrips() {
        let mode = NetworkMode::from_str("host").expect("mode should parse");
        assert_eq!(mode, NetworkMode::Host);
        assert_eq!(mode.to_string(), "host");
    }

    #[test]
    fn runtime_state_roundtrips() {
        let state = RuntimeState::from_str("running").expect("state should parse");
        assert_eq!(state, RuntimeState::Running);
        assert_eq!(state.to_string(), "running");
    }
}
