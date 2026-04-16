#[cfg(test)]
#[path = "tests/heartbeat.rs"]
mod tests;

use std::{ffi::CString, fs};

use chrono::Utc;
use media_domain::{GpuRuntimeStats, HeartbeatSnapshot};

#[derive(Debug, Clone)]
pub struct HeartbeatSampler {
    work_root: String,
    max_runtime_slots: u32,
    previous_cpu: Option<CpuCounters>,
}

#[derive(Debug, Clone, Copy)]
struct CpuCounters {
    total: u64,
    idle: u64,
}

impl HeartbeatSampler {
    pub fn new(work_root: impl Into<String>, max_runtime_slots: u32) -> Self {
        Self {
            work_root: work_root.into(),
            max_runtime_slots,
            previous_cpu: None,
        }
    }

    pub fn sample(
        &mut self,
        running_tasks: u32,
        starting_tasks: u32,
        stopping_tasks: u32,
        orphaned_tasks: u32,
        zlm_alive: bool,
        ffmpeg_alive: bool,
        gpu_runtime: Vec<GpuRuntimeStats>,
    ) -> HeartbeatSnapshot {
        let cpu_percent = self.sample_cpu_percent().unwrap_or(0.0);
        let mem_percent = sample_mem_percent().unwrap_or(0.0);
        let disk_percent = sample_disk_percent(&self.work_root).unwrap_or(0.0);
        let slot_usage = if self.max_runtime_slots == 0 {
            0.0
        } else {
            (running_tasks as f64 / self.max_runtime_slots as f64).clamp(0.0, 1.0)
        };

        HeartbeatSnapshot {
            node_time: Utc::now(),
            cpu_percent,
            mem_percent,
            disk_percent,
            running_tasks,
            starting_tasks,
            stopping_tasks,
            orphaned_tasks,
            slot_usage,
            zlm_alive,
            ffmpeg_alive,
            gpu_runtime,
        }
    }

    fn sample_cpu_percent(&mut self) -> Option<f64> {
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

fn sample_disk_percent(path: &str) -> Option<f64> {
    let path = CString::new(path).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    let rc = unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }

    let stat = unsafe { stat.assume_init() };
    let total = (stat.f_blocks as u64).saturating_mul(stat.f_frsize as u64);
    let free = (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64);
    if total == 0 {
        return Some(0.0);
    }

    Some(((total - free) as f64 / total as f64) * 100.0)
}
