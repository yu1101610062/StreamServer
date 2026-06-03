#[cfg(test)]
#[path = "tests/runtime.rs"]
mod tests;

use std::time::Duration;

#[cfg(test)]
pub(crate) use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};
#[cfg(test)]
pub(crate) use uuid::Uuid;

#[cfg(test)]
pub(crate) use chrono::Utc;
#[cfg(test)]
pub(crate) use reqwest::Client;
#[cfg(test)]
use tokio::{
    sync::mpsc,
    time::{sleep, timeout},
};

#[cfg(test)]
pub(crate) use std::path::PathBuf;
#[cfg(test)]
use std::sync::atomic::AtomicBool;
#[cfg(test)]
use std::time::Instant;
#[cfg(test)]
use std::{ffi::CStr, ptr};
#[cfg(test)]
pub(crate) use std::{fs, path::Path};

#[cfg(test)]
pub(crate) use media_domain::{
    InputKind, InputSpec, RuntimeHandle, RuntimeState, TaskSpec, TaskType, WorkerKind,
};
#[cfg(test)]
pub(crate) use serde_json::{Value, json};

#[cfg(test)]
pub(crate) use crate::config::AgentSettings;
#[cfg(test)]
pub(crate) use crate::ffmpeg_probe::probe_input_media_profile;

#[cfg(test)]
pub(crate) use crate::media_policy::{
    AudioOutputPolicy, InputSourceFamily, VideoCodecFamily, VideoOutputPolicy,
};

#[cfg(test)]
use crate::ffmpeg_probe::{
    DEFAULT_INPUT_PROBE_TIMEOUT_MS, MockFfprobeAudioStream, MockFfprobeBinary,
    register_mock_ffprobe_binary,
};

#[cfg(test)]
pub(crate) use crate::runtime_artifacts::attach_file_artifact_metadata;
#[cfg(test)]
pub(crate) use crate::runtime_events::MAX_LOG_BATCH_BYTES;
pub use crate::runtime_events::{
    RuntimeEventSink, RuntimeNotification, RuntimeTaskEvent, RuntimeTaskLogBatch,
    RuntimeTaskProgress, TerminalRuntimeReplay,
};
pub(crate) use crate::runtime_events::{bounded_log_batches, runtime_session_epoch};
pub use crate::runtime_executor::{LocalExecutor, ManagedProcessExecutor};
#[cfg(test)]
pub(crate) use crate::runtime_io::{build_input_url, resolve_interface_binding_ip};
pub(crate) use crate::runtime_metadata::{
    CompanionProcessKind, StreamBinding, runtime_lease_token,
};
#[cfg(test)]
pub(crate) use crate::runtime_metadata::{
    RtpServerMetadata, clear_source_reconnecting, live_relay_recording_from_handle,
    managed_stream_restart_cleanup_binding, mark_source_reconnecting, recording_gap_active,
    should_emit_recording_gap_started, should_emit_source_reconnecting,
    sticky_reconnect_stream_ingest_from_handle,
};
#[cfg(test)]
pub(crate) use crate::runtime_monitors::cleanup_live_relay_runtime;
#[cfg(test)]
pub(crate) use crate::runtime_monitors::spawn_startup_probe_monitor;
#[cfg(test)]
pub(crate) use crate::runtime_outputs::{ManagedFileOutputKind, managed_output_dir};
#[cfg(test)]
pub(crate) use crate::runtime_persistence::scan_persisted_runtimes;
#[cfg(test)]
pub(crate) use crate::runtime_persistence::{
    RUNTIME_COMMAND_FILE, RUNTIME_PID_FILE, RUNTIME_STATE_FILE, persist_runtime_state,
};
pub use crate::runtime_persistence::{
    cleanup_persisted_runtime_state, collect_terminal_runtime_replays, is_terminal_runtime_event,
};
#[cfg(test)]
pub(crate) use crate::runtime_plan::{
    ProcessPlan, build_file_transcode_plan, build_live_relay_api_params, build_live_relay_plan,
    build_multicast_bridge_plan, build_open_rtp_server_params,
    build_open_rtp_server_params_from_metadata, build_process_plan, build_rtp_receive_plan,
    build_stream_ingest_plan_with_capability_hints, build_stream_ingest_realtime_plan,
    parse_task_spec, prepare_plan_paths,
};
pub(crate) use crate::runtime_plan::{TaskRuntimeMode, task_runtime_mode};
#[cfg(test)]
pub(crate) use crate::runtime_process::{ManagedRuntime, RuntimeSlotPermit};
pub(crate) use crate::runtime_recording::{LiveRelayRecording, ZlmRecordKind};
#[cfg(test)]
pub(crate) use crate::runtime_recording::{
    recording_duration_reached, should_auto_stop_live_relay_recording,
    should_start_live_relay_recording,
};
pub(crate) use crate::runtime_recovery::classify_adopted_exit;
#[cfg(test)]
pub(crate) use crate::runtime_recovery::{
    LIVE_STREAM_OFFLINE_GRACE_POLLS, RTP_SERVER_MISSING_GRACE_POLLS, next_live_relay_offline_polls,
    next_rtp_server_missing_polls, should_auto_restart_process,
};
pub use crate::runtime_registry::{AdoptFilter, AdoptRuntimeFilter, LocalRuntimeRegistry};
#[cfg(test)]
pub(crate) use crate::runtime_transcode::{
    resolve_transcode_selection_for_input_family, resolve_video_families,
};
pub use crate::runtime_types::{
    ExecutorError, RecordingControlAction, StartTaskRequest, StopTaskRequest,
    TaskRecordingControlRequest, rejected_runtime_handle,
};
pub(crate) use crate::runtime_types::{
    RuntimeCapabilityHints, StartupProbe, SuccessCheck, ZlmMediaStatus,
};
#[cfg(test)]
pub(crate) use crate::runtime_zlm::{build_record_api_params, zlm_stream_status_in_body};

pub(crate) const STARTUP_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const STARTUP_PROBE_POLL_INTERVAL: Duration = Duration::from_secs(1);
pub(crate) const STOP_REQUESTED_STILL_RUNNING_LOG_INTERVAL: Duration = Duration::from_secs(10);
pub(crate) const AUTO_STOP_FORCE_KILL_DELAY: Duration = Duration::from_secs(1);
pub(crate) const RECORD_DURATION_FORCE_KILL_DELAY: Duration = Duration::from_millis(250);
