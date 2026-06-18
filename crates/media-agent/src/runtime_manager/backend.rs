use std::{
    collections::HashMap,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};

use uuid::Uuid;

use crate::{
    config::AgentSettings,
    runtime_process::{
        ManagedRuntime, ProcessIdentity, RuntimeSlotClass, RuntimeSlotLimiter, RuntimeSlotPermit,
    },
    runtime_types::ExecutorError,
};

#[derive(Debug, Clone)]
pub(crate) struct RuntimeBackendSnapshot {
    pub(crate) stop_requested: bool,
    pub(crate) suppress_companion_events: bool,
    pub(crate) companion_processes: Vec<ProcessIdentity>,
}

#[derive(Clone)]
pub(crate) struct RuntimeBackendStore {
    inner: Arc<RwLock<HashMap<Uuid, ManagedRuntime>>>,
    live_slot_limiter: Arc<RuntimeSlotLimiter>,
    vod_slot_limiter: Arc<RuntimeSlotLimiter>,
}

impl RuntimeBackendStore {
    pub(crate) fn new(settings: &AgentSettings) -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            live_slot_limiter: Arc::new(RuntimeSlotLimiter::new(settings.max_live_runtime_slots)),
            vod_slot_limiter: Arc::new(RuntimeSlotLimiter::new(settings.max_vod_runtime_slots)),
        }
    }

    pub(crate) fn try_acquire_slot(
        &self,
        slot_class: RuntimeSlotClass,
    ) -> Result<Arc<RuntimeSlotPermit>, ExecutorError> {
        self.slot_limiter(slot_class).try_acquire(slot_class)
    }

    pub(crate) fn attach_existing_slot(
        &self,
        slot_class: RuntimeSlotClass,
    ) -> Arc<RuntimeSlotPermit> {
        self.slot_limiter(slot_class).attach_existing()
    }

    fn slot_limiter(&self, slot_class: RuntimeSlotClass) -> &Arc<RuntimeSlotLimiter> {
        match slot_class {
            RuntimeSlotClass::Live => &self.live_slot_limiter,
            RuntimeSlotClass::Vod => &self.vod_slot_limiter,
        }
    }

    pub(crate) fn get(&self, runtime_id: Uuid) -> Option<ManagedRuntime> {
        let runtimes = self.inner.read().expect("runtime backend lock poisoned");
        runtimes.get(&runtime_id).cloned()
    }

    pub(crate) fn insert(
        &self,
        runtime_id: Uuid,
        runtime: ManagedRuntime,
    ) -> Option<ManagedRuntime> {
        let mut runtimes = self.inner.write().expect("runtime backend lock poisoned");
        runtimes.insert(runtime_id, runtime)
    }

    pub(crate) fn remove(&self, runtime_id: Uuid) -> Option<ManagedRuntime> {
        let mut runtimes = self.inner.write().expect("runtime backend lock poisoned");
        runtimes.remove(&runtime_id)
    }

    pub(crate) fn snapshot(&self, runtime_id: Uuid) -> Option<RuntimeBackendSnapshot> {
        let runtime = self.get(runtime_id)?;
        Some(RuntimeBackendSnapshot {
            stop_requested: runtime.stop_requested.load(Ordering::Relaxed),
            suppress_companion_events: runtime.suppress_companion_events.load(Ordering::Relaxed),
            companion_processes: runtime.companion_processes,
        })
    }

    pub(crate) fn apply_monitor_backend_delta(
        &self,
        commit: &crate::runtime_manager::RuntimeMonitorCommit,
    ) {
        if commit.mark_stop_requested.is_some()
            || commit.suppress_companion_events.is_some()
            || commit.remove_companion_pid.is_some()
        {
            let mut runtimes = self.inner.write().expect("runtime backend lock poisoned");
            if let Some(runtime) = runtimes.get_mut(&commit.runtime_id) {
                if let Some(stop_requested) = commit.mark_stop_requested {
                    runtime
                        .stop_requested
                        .store(stop_requested, Ordering::Relaxed);
                }
                if let Some(suppress_companion_events) = commit.suppress_companion_events {
                    runtime
                        .suppress_companion_events
                        .store(suppress_companion_events, Ordering::Relaxed);
                }
                if let Some(companion_pid) = commit.remove_companion_pid {
                    runtime
                        .companion_processes
                        .retain(|process| process.pid != companion_pid);
                }
            }
        }

        if commit.remove_backend {
            let _ = self.remove(commit.runtime_id);
        }
    }

    pub(crate) fn adopted_runtime(
        &self,
        slot_class: RuntimeSlotClass,
        process: Option<ProcessIdentity>,
        companion_processes: Vec<ProcessIdentity>,
    ) -> ManagedRuntime {
        ManagedRuntime {
            process,
            companion_processes,
            _slot_permit: self.attach_existing_slot(slot_class),
            stop_requested: Arc::new(AtomicBool::new(false)),
            suppress_companion_events: Arc::new(AtomicBool::new(false)),
        }
    }
}
