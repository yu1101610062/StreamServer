//! 运行时进程管理：维护本地进程槽位、托管进程句柄、进程移除以及信号/延迟强杀辅助。

use std::{
    collections::HashMap,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Duration,
};

use uuid::Uuid;

use crate::runtime::ExecutorError;

#[derive(Debug, Clone)]
pub(crate) struct ManagedRuntime {
    pub(crate) pid: Option<i32>,
    pub(crate) companion_pids: Vec<i32>,
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

pub(crate) fn remove_managed_runtime(
    runtimes: &Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    runtime_id: Uuid,
) -> Option<ManagedRuntime> {
    runtimes
        .write()
        .expect("runtime map lock poisoned")
        .remove(&runtime_id)
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

pub(crate) fn runtime_pids(runtime: &ManagedRuntime) -> Vec<i32> {
    let mut pids = Vec::new();
    if let Some(pid) = runtime.pid {
        pids.push(pid);
    }
    pids.extend(runtime.companion_pids.iter().copied());
    pids
}

pub(crate) fn signal_runtime_pids(
    runtime: &ManagedRuntime,
    signal: i32,
) -> Result<(), ExecutorError> {
    for pid in runtime_pids(runtime) {
        signal_pid(pid, signal).map_err(|error| ExecutorError::ProcessSignal(error.to_string()))?;
    }
    Ok(())
}

pub(crate) fn schedule_force_kill_if_running(
    runtime_id: Uuid,
    pids: Vec<i32>,
    runtimes: Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    delay: Duration,
    reason: &'static str,
) {
    if pids.is_empty() {
        return;
    }

    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let runtime_still_tracked = runtimes
            .read()
            .expect("runtime map lock poisoned")
            .contains_key(&runtime_id);
        if !runtime_still_tracked {
            return;
        }

        for pid in pids {
            if !is_pid_running(pid) {
                continue;
            }
            tracing::warn!(
                runtime_id = %runtime_id,
                pid,
                delay_sec = delay.as_secs_f64(),
                reason,
                "process still running after graceful stop; sending SIGKILL"
            );
            let _ = signal_pid(pid, libc::SIGKILL);
        }
    });
}

pub(crate) fn schedule_force_kill_pids_if_running(
    pids: Vec<i32>,
    delay: Duration,
    reason: &'static str,
) {
    if pids.is_empty() {
        return;
    }

    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        for pid in pids {
            if !is_pid_running(pid) {
                continue;
            }
            tracing::warn!(
                pid,
                delay_sec = delay.as_secs_f64(),
                reason,
                "stale process still running after graceful stop; sending SIGKILL"
            );
            let _ = signal_pid(pid, libc::SIGKILL);
        }
    });
}

pub(crate) fn signal_pid(pid: i32, signal: i32) -> std::io::Result<()> {
    let rc = unsafe { libc::kill(pid, signal) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}
