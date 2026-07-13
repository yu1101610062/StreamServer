#[cfg(test)]
#[path = "tests/heartbeat.rs"]
mod tests;

use std::{ffi::CString, fs};

use chrono::Utc;
use media_domain::{GpuRuntimeStats, HeartbeatSnapshot, RuntimeSlotLoad};

use crate::artifact_cleanup::ArtifactCleanupManager;

#[derive(Debug, Clone)]
pub struct HeartbeatSampler {
    work_root: String,
    artifact_cleanup: Option<ArtifactCleanupManager>,
    previous_cpu: Option<CpuCounters>,
}

#[derive(Debug, Clone, Copy)]
struct CpuCounters {
    total: u64,
    idle: u64,
}

#[derive(Debug)]
pub(crate) struct HeartbeatSampleInput {
    pub(crate) running_tasks: u32,
    pub(crate) starting_tasks: u32,
    pub(crate) stopping_tasks: u32,
    pub(crate) orphaned_tasks: u32,
    pub(crate) runtime_slot_loads: Vec<RuntimeSlotLoad>,
    pub(crate) zlm_alive: bool,
    pub(crate) ffmpeg_alive: bool,
    pub(crate) gpu_runtime: Vec<GpuRuntimeStats>,
}

impl HeartbeatSampler {
    pub fn new(
        work_root: impl Into<String>,
        artifact_cleanup: Option<ArtifactCleanupManager>,
    ) -> Self {
        Self {
            work_root: work_root.into(),
            artifact_cleanup,
            previous_cpu: None,
        }
    }

    pub fn sample(&mut self, input: HeartbeatSampleInput) -> HeartbeatSnapshot {
        // 上传盘与产物盘可能不是同一个挂载点：上传盘来自 work_root，产物盘优先来自清理器采样。
        let cpu_percent = self.sample_cpu_percent().unwrap_or(0.0);
        let mem_percent = sample_mem_percent().unwrap_or(0.0);
        let upload_disk = sample_disk(&self.work_root).unwrap_or_default();
        let disk_percent = self
            .artifact_cleanup
            .as_ref()
            .and_then(ArtifactCleanupManager::current_disk_percent)
            .unwrap_or(upload_disk.used_percent);
        let artifact_cleanup_block_reason = self
            .artifact_cleanup
            .as_ref()
            .and_then(ArtifactCleanupManager::control_plane_block_reason);
        let artifact_cleanup_blocked = artifact_cleanup_block_reason.is_some();
        HeartbeatSnapshot {
            node_time: Utc::now(),
            cpu_percent,
            mem_percent,
            disk_percent,
            upload_disk_total_bytes: upload_disk.total_bytes,
            upload_disk_available_bytes: upload_disk.available_bytes,
            upload_disk_used_percent: upload_disk.used_percent,
            running_tasks: input.running_tasks,
            starting_tasks: input.starting_tasks,
            stopping_tasks: input.stopping_tasks,
            orphaned_tasks: input.orphaned_tasks,
            runtime_slot_loads: input.runtime_slot_loads,
            zlm_alive: input.zlm_alive,
            ffmpeg_alive: input.ffmpeg_alive,
            artifact_cleanup_blocked,
            artifact_cleanup_block_reason,
            gpu_runtime: input.gpu_runtime,
        }
    }

    fn sample_cpu_percent(&mut self) -> Option<f64> {
        // CPU 使用率需要两次 /proc/stat 差值，第一次采样没有前序值时返回 None。
        let current = sample_cpu_counters()?;
        let previous = self.previous_cpu.replace(current)?;
        let total_delta = current.total.saturating_sub(previous.total);
        let idle_delta = current.idle.saturating_sub(previous.idle);

        if total_delta == 0 {
            return Some(0.0);
        }

        Some(((total_delta - idle_delta) as f64 / total_delta as f64) * 100.0)
    }
}

fn sample_cpu_counters() -> Option<CpuCounters> {
    let stat = fs::read_to_string("/proc/stat").ok()?;
    let first_line = stat.lines().next()?;
    let mut fields = first_line.split_whitespace();
    let cpu = fields.next()?;
    if cpu != "cpu" {
        return None;
    }

    let values = fields
        .filter_map(|value| value.parse::<u64>().ok())
        .collect::<Vec<_>>();
    if values.len() < 4 {
        return None;
    }

    let idle =
        values.get(3).copied().unwrap_or_default() + values.get(4).copied().unwrap_or_default();
    let total = values.iter().sum();
    Some(CpuCounters { total, idle })
}

fn sample_mem_percent() -> Option<f64> {
    let meminfo = fs::read_to_string("/proc/meminfo").ok()?;
    let mut total_kb = None;
    let mut available_kb = None;

    for line in meminfo.lines() {
        if line.starts_with("MemTotal:") {
            total_kb = line.split_whitespace().nth(1)?.parse::<u64>().ok();
        }
        if line.starts_with("MemAvailable:") {
            available_kb = line.split_whitespace().nth(1)?.parse::<u64>().ok();
        }
    }

    let total_kb = total_kb?;
    let available_kb = available_kb?;
    if total_kb == 0 {
        return Some(0.0);
    }

    Some(((total_kb - available_kb) as f64 / total_kb as f64) * 100.0)
}

#[derive(Debug, Clone, Copy, Default)]
struct DiskSample {
    total_bytes: u64,
    available_bytes: u64,
    used_percent: f64,
}

fn sample_disk(path: &str) -> Option<DiskSample> {
    let path = CString::new(path).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    let rc = unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }

    let stat = unsafe { stat.assume_init() };
    let total = stat.f_blocks.saturating_mul(stat.f_frsize);
    let free = stat.f_bavail.saturating_mul(stat.f_frsize);
    if total == 0 {
        return Some(DiskSample::default());
    }

    Some(DiskSample {
        total_bytes: total,
        available_bytes: free,
        used_percent: ((total - free) as f64 / total as f64) * 100.0,
    })
}
