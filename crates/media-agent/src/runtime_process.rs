//! 运行时进程管理：维护本地进程槽位、托管进程句柄、进程移除以及信号/延迟强杀辅助。

use std::{
    collections::HashMap,
    io,
    path::Path,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Duration,
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::runtime::ExecutorError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProcessIdentity {
    pub(crate) pid: i32,
    #[serde(default)]
    pub(crate) pgid: Option<i32>,
    #[serde(default)]
    pub(crate) pid_start_time: Option<u64>,
}

#[derive(Debug, Clone)]
pub(crate) struct ManagedRuntime {
    pub(crate) process: Option<ProcessIdentity>,
    pub(crate) companion_processes: Vec<ProcessIdentity>,
    pub(crate) _slot_permit: Arc<RuntimeSlotPermit>,
    pub(crate) stop_requested: Arc<AtomicBool>,
    pub(crate) suppress_companion_events: Arc<AtomicBool>,
}

#[derive(Debug)]
pub(crate) struct RuntimeSlotLimiter {
    limit: u32,
    occupied: AtomicU32,
}

#[derive(Debug)]
pub(crate) struct RuntimeSlotPermit {
    limiter: Option<Arc<RuntimeSlotLimiter>>,
    released: AtomicBool,
}

impl RuntimeSlotLimiter {
    pub(crate) fn new(limit: u32) -> Self {
        Self {
            limit,
            occupied: AtomicU32::new(0),
        }
    }

    pub(crate) fn try_acquire(self: &Arc<Self>) -> Result<Arc<RuntimeSlotPermit>, ExecutorError> {
        if self.limit == 0 {
            return Ok(RuntimeSlotPermit::unbounded());
        }

        let mut current = self.occupied.load(Ordering::Acquire);
        loop {
            if current >= self.limit {
                return Err(ExecutorError::InvalidRequest(format!(
                    "max_runtime_slots exhausted: {current}/{}",
                    self.limit
                )));
            }
            match self.occupied.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(RuntimeSlotPermit::tracked(self.clone())),
                Err(observed) => current = observed,
            }
        }
    }

    pub(crate) fn attach_existing(self: &Arc<Self>) -> Arc<RuntimeSlotPermit> {
        if self.limit == 0 {
            return RuntimeSlotPermit::unbounded();
        }

        self.occupied.fetch_add(1, Ordering::AcqRel);
        RuntimeSlotPermit::tracked(self.clone())
    }
}

impl RuntimeSlotPermit {
    fn tracked(limiter: Arc<RuntimeSlotLimiter>) -> Arc<Self> {
        Arc::new(Self {
            limiter: Some(limiter),
            released: AtomicBool::new(false),
        })
    }

    pub(crate) fn unbounded() -> Arc<Self> {
        Arc::new(Self {
            limiter: None,
            released: AtomicBool::new(false),
        })
    }

    fn release(&self) {
        if self.released.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(limiter) = &self.limiter {
            limiter.occupied.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl Drop for RuntimeSlotPermit {
    fn drop(&mut self) {
        self.release();
    }
}

impl ProcessIdentity {
    pub(crate) fn legacy_pid(pid: i32) -> Self {
        Self {
            pid,
            pgid: None,
            pid_start_time: None,
        }
    }

    pub(crate) fn pid_only(pid: i32) -> Self {
        Self {
            pid,
            pgid: None,
            pid_start_time: linux_pid_start_time(pid),
        }
    }

    pub(crate) fn spawned_process_group(pid: i32) -> Self {
        Self {
            pid,
            pgid: process_group_id(pid).filter(|pgid| *pgid == pid),
            pid_start_time: linux_pid_start_time(pid),
        }
    }
}

pub(crate) fn remove_managed_runtime(
    runtimes: &Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    runtime_id: Uuid,
) -> Option<ManagedRuntime> {
    {
        let mut runtimes = runtimes.write().expect("runtime map lock poisoned");
        runtimes.remove(&runtime_id)
    }
}

pub(crate) fn is_pid_running(pid: i32) -> bool {
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        true
    } else {
        matches!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        )
    }
}

pub(crate) fn is_process_running(process: &ProcessIdentity) -> bool {
    if !is_pid_running(process.pid) {
        return false;
    }
    if let Some(expected_start_time) = process.pid_start_time {
        return linux_pid_start_time(process.pid)
            .map(|current_start_time| current_start_time == expected_start_time)
            .unwrap_or_else(|| process.pgid.is_some_and(process_group_running));
    }
    true
}

pub(crate) fn process_group_id(pid: i32) -> Option<i32> {
    let pgid = unsafe { libc::getpgid(pid) };
    (pgid >= 0).then_some(pgid)
}

#[cfg(unix)]
pub(crate) fn configure_new_process_group(command: &mut tokio::process::Command) {
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        });
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn linux_pid_start_time(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_linux_proc_stat_start_time(&stat)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn linux_pid_start_time(_pid: i32) -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
pub(crate) fn parse_linux_proc_stat_start_time(stat: &str) -> Option<u64> {
    let end_comm = stat.rfind(") ")?;
    let fields = stat[end_comm + 2..].split_whitespace().collect::<Vec<_>>();
    fields.get(19)?.parse().ok()
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn parse_linux_proc_stat_start_time(_stat: &str) -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn linux_proc_cmdline(pid: i32) -> Option<Vec<String>> {
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args = bytes
        .split(|byte| *byte == b'\0')
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect::<Vec<_>>();
    (!args.is_empty()).then_some(args)
}

#[cfg(not(target_os = "linux"))]
fn linux_proc_cmdline(_pid: i32) -> Option<Vec<String>> {
    None
}

pub(crate) fn is_process_running_for_command_line(
    process: &ProcessIdentity,
    command_line: Option<&str>,
) -> bool {
    if !is_process_running(process) {
        return false;
    }
    if process.pid_start_time.is_some() {
        return true;
    }
    command_line
        .map(|command_line| process_command_line_matches(process.pid, command_line))
        .unwrap_or(false)
}

fn process_command_line_matches(pid: i32, expected_command_line: &str) -> bool {
    linux_proc_cmdline(pid)
        .map(|actual_args| command_line_matches_args(&actual_args, expected_command_line))
        .unwrap_or(false)
}

fn command_line_matches_args(actual_args: &[String], expected_command_line: &str) -> bool {
    let expected_args = expected_command_line.split_whitespace().collect::<Vec<_>>();
    let Some((expected_executable, expected_tail)) = expected_args.split_first() else {
        return false;
    };
    let expected_executable_name = file_name(expected_executable);

    actual_args
        .iter()
        .enumerate()
        .filter(|(_, actual)| file_name(actual) == expected_executable_name)
        .any(|(index, _)| {
            let actual_tail = &actual_args[index + 1..];
            actual_tail.len() >= expected_tail.len()
                && actual_tail
                    .iter()
                    .zip(expected_tail.iter())
                    .all(|(actual, expected)| actual == expected)
        })
}

fn file_name(value: &str) -> &str {
    Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(value)
}

pub(crate) fn runtime_processes(runtime: &ManagedRuntime) -> Vec<ProcessIdentity> {
    let mut pids = Vec::new();
    if let Some(process) = runtime.process {
        pids.push(process);
    }
    pids.extend(runtime.companion_processes.iter().copied());
    pids
}

pub(crate) fn signal_runtime_processes(
    runtime: &ManagedRuntime,
    signal: i32,
) -> Result<(), ExecutorError> {
    for process in runtime_processes(runtime) {
        signal_process(&process, signal)
            .map_err(|error| ExecutorError::ProcessSignal(error.to_string()))?;
    }
    Ok(())
}

pub(crate) fn schedule_force_kill_if_running(
    runtime_id: Uuid,
    processes: Vec<ProcessIdentity>,
    runtimes: Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    delay: Duration,
    reason: &'static str,
) {
    if processes.is_empty() {
        return;
    }

    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let runtime_still_tracked = {
            let runtimes = runtimes.read().expect("runtime map lock poisoned");
            runtimes.contains_key(&runtime_id)
        };
        if !runtime_still_tracked {
            return;
        }

        for process in processes {
            if !process_should_receive_force_kill(&process, runtime_id, reason) {
                continue;
            }
            tracing::warn!(
                runtime_id = %runtime_id,
                pid = process.pid,
                pgid = ?process.pgid,
                delay_sec = delay.as_secs_f64(),
                reason,
                "process still running after graceful stop; sending SIGKILL"
            );
            let _ = signal_process(&process, libc::SIGKILL);
        }
    });
}

pub(crate) fn schedule_force_kill_processes_if_running(
    processes: Vec<ProcessIdentity>,
    delay: Duration,
    reason: &'static str,
) {
    if processes.is_empty() {
        return;
    }

    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        for process in processes {
            if !process_should_receive_force_kill(&process, Uuid::nil(), reason) {
                continue;
            }
            tracing::warn!(
                pid = process.pid,
                pgid = ?process.pgid,
                delay_sec = delay.as_secs_f64(),
                reason,
                "stale process still running after graceful stop; sending SIGKILL"
            );
            let _ = signal_process(&process, libc::SIGKILL);
        }
    });
}

fn process_should_receive_force_kill(
    process: &ProcessIdentity,
    runtime_id: Uuid,
    reason: &'static str,
) -> bool {
    if let Some(expected_start_time) = process.pid_start_time {
        if let Some(current_start_time) = linux_pid_start_time(process.pid) {
            if current_start_time != expected_start_time {
                tracing::warn!(
                    runtime_id = %runtime_id,
                    pid = process.pid,
                    expected_start_time,
                    current_start_time,
                    reason,
                    "skipping force kill because pid start time changed"
                );
                return false;
            }
            return true;
        }
        return process.pgid.is_some();
    }

    process
        .pgid
        .map(process_group_running)
        .unwrap_or_else(|| is_process_running(process))
}

fn process_group_running(pgid: i32) -> bool {
    signal_pgid(pgid, 0).is_ok()
}

pub(crate) fn signal_process(process: &ProcessIdentity, signal: i32) -> io::Result<()> {
    if let Some(pgid) = process.pgid {
        match signal_pgid(pgid, signal) {
            Ok(()) => return Ok(()),
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {}
            Err(error) => return Err(error),
        }
    }
    signal_pid(process.pid, signal)
}

pub(crate) fn signal_pgid(pgid: i32, signal: i32) -> io::Result<()> {
    let rc = unsafe { libc::kill(-pgid, signal) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

pub(crate) fn signal_pid(pid: i32, signal: i32) -> std::io::Result<()> {
    let rc = unsafe { libc::kill(pid, signal) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_match_accepts_ld_linux_argv0_wrapper() {
        let actual_args = vec![
            "/home/streamserver/runtime/ffmpeg/cpu/lib/ld-linux-x86-64.so.2".to_string(),
            "--library-path".to_string(),
            "/home/streamserver/runtime/ffmpeg/cpu/lib".to_string(),
            "--argv0".to_string(),
            "/home/streamserver/bin/ffmpeg".to_string(),
            "/home/streamserver/runtime/ffmpeg/cpu/bin/ffmpeg".to_string(),
            "-hide_banner".to_string(),
            "-nostdin".to_string(),
            "-i".to_string(),
            "udp://231.40.1.101:5001?localaddr=192.168.17.110".to_string(),
            "-f".to_string(),
            "flv".to_string(),
            "rtmp://172.17.13.196:1935/preview/test".to_string(),
        ];

        assert!(command_line_matches_args(
            &actual_args,
            "ffmpeg -hide_banner -nostdin -i udp://231.40.1.101:5001?localaddr=192.168.17.110 -f flv rtmp://172.17.13.196:1935/preview/test"
        ));
    }

    #[test]
    fn command_line_match_rejects_unrelated_process() {
        let actual_args = vec!["kworker/21:0H-kblockd".to_string(), "ignored".to_string()];

        assert!(!command_line_matches_args(
            &actual_args,
            "ffmpeg -hide_banner -nostdin -i udp://231.40.1.101:5001"
        ));
    }
}
