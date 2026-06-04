//! Runtime 主进程退出监控：处理受管 FFmpeg/进程结束后的清理、事件归类和本地恢复。
//!
//! 这个模块只关注“主进程已经启动并最终退出”之后的收尾路径，包括伴随录制进程
//! 终止、输出 artifact 校验、持续流任务自动重启，以及最终 runtime 事件和快照投递。

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use uuid::Uuid;

use crate::{
    runtime::SuccessCheck,
    runtime_manager::{ProcessExitedEvent, RuntimeInternalEvent, RuntimeMonitorHandle},
    runtime_process::{is_process_running, signal_process},
    runtime_process_monitors::wait_for_companion_pids_exit,
};

pub(crate) struct ProcessExitMonitorContext {
    pub(crate) runtime_id: Uuid,
    pub(crate) work_dir: PathBuf,
    pub(crate) output_target: String,
    pub(crate) success_check: SuccessCheck,
    pub(crate) stop_requested: Arc<AtomicBool>,
    pub(crate) monitor_handle: RuntimeMonitorHandle,
}

pub(crate) fn spawn_process_exit_monitor(
    context: ProcessExitMonitorContext,
    mut child: tokio::process::Child,
) {
    tokio::spawn(async move {
        let ProcessExitMonitorContext {
            runtime_id,
            work_dir,
            output_target,
            success_check,
            stop_requested,
            monitor_handle,
        } = context;

        let status = child.wait().await;
        let Some(snapshot) = monitor_handle.snapshot().await else {
            return;
        };
        let was_stopped = snapshot.stop_requested || stop_requested.load(Ordering::Relaxed);
        if !snapshot.companion_processes.is_empty() {
            for companion_process in &snapshot.companion_processes {
                if is_process_running(companion_process) {
                    let _ = signal_process(companion_process, libc::SIGTERM);
                }
            }
            wait_for_companion_pids_exit(&snapshot.companion_processes, Duration::from_secs(3))
                .await;
            for companion_process in &snapshot.companion_processes {
                if is_process_running(companion_process) {
                    let _ = signal_process(companion_process, libc::SIGKILL);
                }
            }
        }
        monitor_handle
            .send_event(RuntimeInternalEvent::ProcessExited(ProcessExitedEvent {
                runtime_id,
                generation: monitor_handle.generation(),
                work_dir,
                output_target,
                success_check,
                status: status.map_err(|error| error.to_string()),
                was_stopped,
            }))
            .await;
    });
}
