//! Live relay 录制辅助：处理常态监控中的录制启动、降级和时长停止请求。
//!
//! 这里只封装 live relay 主循环里重复的录制启动状态回写、失败降级/fatal 收尾、录制完成持久化
//! 和停止请求；主循环仍负责判断何时探测、何时进入运行态。

use chrono::Utc;
use media_domain::RuntimeHandle;
use tracing::info;

use crate::{
    runtime_manager::{RecordDurationReachedEvent, RuntimeInternalEvent, RuntimeMonitorHandle},
    runtime_metadata::live_relay_recording_from_handle,
    runtime_recording::{recording_elapsed_seconds, should_auto_stop_live_relay_recording},
};

pub(crate) async fn notify_live_relay_record_duration_if_reached(
    monitor_handle: &RuntimeMonitorHandle,
    handle: &RuntimeHandle,
) -> bool {
    let now = Utc::now();
    let Some(recording) = live_relay_recording_from_handle(handle)
        .filter(|recording| should_auto_stop_live_relay_recording(recording, now))
    else {
        return false;
    };

    info!(
        task_id = %handle.task_id,
        attempt_no = handle.attempt_no,
        runtime_id = %handle.runtime_id,
        generation = monitor_handle.generation().value(),
        recording_started_at = ?recording.recording_started_at,
        duration_sec = recording.duration_sec.unwrap_or_default(),
        now = %now.to_rfc3339(),
        elapsed_sec = recording_elapsed_seconds(&recording, now).unwrap_or_default(),
        command_line = handle.command_line.as_deref().unwrap_or(""),
        "wall-clock recording duration reached; notifying runtime manager"
    );
    monitor_handle
        .send_event(RuntimeInternalEvent::RecordDurationReached(
            RecordDurationReachedEvent {
                runtime_id: handle.runtime_id,
                generation: monitor_handle.generation(),
            },
        ))
        .await;
    true
}
