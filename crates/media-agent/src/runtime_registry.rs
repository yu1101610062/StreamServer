use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use media_domain::{RuntimeHandle, RuntimeState, WorkerKind};
use serde_json::Value;
use uuid::Uuid;

/// Legacy in-memory registry retained for tests and direct executor compatibility.
///
/// Production runtime reads come from `RuntimeReadHandle`, which is maintained by
/// `RuntimeManagerState`.
#[derive(Debug, Clone)]
pub struct LocalRuntimeRegistry {
    inner: Arc<RwLock<RuntimeRegistryState>>,
}

#[derive(Debug, Clone)]
pub struct RuntimeReadHandle {
    inner: Arc<RwLock<RuntimeRegistryState>>,
}

// The registry keeps the std::sync::RwLock fully internal. Public methods either
// complete a short synchronous mutation or return cloned snapshots, so callers
// cannot accidentally hold a registry guard across an async boundary.
#[derive(Debug, Default)]
struct RuntimeRegistryState {
    by_runtime_id: HashMap<Uuid, RuntimeHandle>,
    by_task_attempt: HashMap<(Uuid, i32), Uuid>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuntimeStateCounts {
    pub running: u32,
    pub starting: u32,
    pub stopping: u32,
    pub orphaned: u32,
}

#[derive(Debug, Clone, Default)]
pub struct AdoptFilter {
    pub session_epoch: u64,
    pub runtimes: Vec<AdoptRuntimeFilter>,
}

#[derive(Debug, Clone)]
pub struct AdoptRuntimeFilter {
    pub task_id: Uuid,
    pub attempt_no: i32,
    pub lease_token: String,
    pub worker_kind: WorkerKind,
}

pub trait RuntimeReadModel: Send + Sync {
    #[allow(dead_code)]
    fn state_counts(&self) -> RuntimeStateCounts;
    fn active_handles(&self) -> Vec<RuntimeHandle>;
    fn find_by_task_attempt(&self, task_id: Uuid, attempt_no: i32) -> Option<RuntimeHandle>;
    #[allow(dead_code)]
    fn snapshots(&self, filter: &AdoptFilter) -> Vec<RuntimeHandle>;
}

impl LocalRuntimeRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(RuntimeRegistryState::default())),
        }
    }

    pub fn track(&self, handle: RuntimeHandle) {
        let mut runtimes = self.inner.write().expect("runtime registry lock poisoned");
        let key = (handle.task_id, handle.attempt_no);
        if let Some(previous_runtime_id) = runtimes.by_task_attempt.insert(key, handle.runtime_id) {
            if previous_runtime_id != handle.runtime_id {
                runtimes.by_runtime_id.remove(&previous_runtime_id);
            }
        }
        runtimes.by_runtime_id.insert(handle.runtime_id, handle);
    }

    pub fn remove(&self, runtime_id: Uuid) -> Option<RuntimeHandle> {
        let mut runtimes = self.inner.write().expect("runtime registry lock poisoned");
        let removed = runtimes.by_runtime_id.remove(&runtime_id)?;
        runtimes
            .by_task_attempt
            .remove(&(removed.task_id, removed.attempt_no));
        Some(removed)
    }

    pub fn update(
        &self,
        runtime_id: Uuid,
        update: impl FnOnce(&mut RuntimeHandle),
    ) -> Option<RuntimeHandle> {
        let mut runtimes = self.inner.write().expect("runtime registry lock poisoned");
        let handle = runtimes.by_runtime_id.get_mut(&runtime_id)?;
        update(handle);
        Some(handle.clone())
    }

    pub fn get(&self, runtime_id: Uuid) -> Option<RuntimeHandle> {
        let runtimes = self.inner.read().expect("runtime registry lock poisoned");
        runtimes.by_runtime_id.get(&runtime_id).cloned()
    }

    pub fn find_by_task_attempt(&self, task_id: Uuid, attempt_no: i32) -> Option<RuntimeHandle> {
        let runtimes = self.inner.read().expect("runtime registry lock poisoned");
        let runtime_id = runtimes.by_task_attempt.get(&(task_id, attempt_no))?;
        runtimes.by_runtime_id.get(runtime_id).cloned()
    }

    #[cfg(test)]
    pub fn count(&self) -> usize {
        let runtimes = self.inner.read().expect("runtime registry lock poisoned");
        runtimes.by_runtime_id.len()
    }

    pub fn state_counts(&self) -> RuntimeStateCounts {
        let runtimes = self.inner.read().expect("runtime registry lock poisoned");
        let mut counts = RuntimeStateCounts::default();
        for handle in runtimes.by_runtime_id.values() {
            match handle.state {
                RuntimeState::Pending | RuntimeState::Starting => {
                    counts.starting = counts.starting.saturating_add(1);
                }
                RuntimeState::Running => {
                    counts.running = counts.running.saturating_add(1);
                }
                RuntimeState::Stopping => {
                    counts.stopping = counts.stopping.saturating_add(1);
                }
                RuntimeState::Orphaned => {
                    counts.orphaned = counts.orphaned.saturating_add(1);
                }
                RuntimeState::Exited => {}
            }
        }
        counts
    }

    pub fn snapshots(&self, filter: &AdoptFilter) -> Vec<RuntimeHandle> {
        let runtimes = self.inner.read().expect("runtime registry lock poisoned");
        runtimes
            .by_runtime_id
            .values()
            .filter(|handle| filter.matches(handle))
            .cloned()
            .collect()
    }

    pub fn active_handles(&self) -> Vec<RuntimeHandle> {
        let runtimes = self.inner.read().expect("runtime registry lock poisoned");
        runtimes
            .by_runtime_id
            .values()
            .filter(|handle| handle.state != RuntimeState::Exited)
            .cloned()
            .collect()
    }
}

impl RuntimeReadHandle {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(RuntimeRegistryState::default())),
        }
    }

    pub(crate) fn apply_handle(&self, handle: RuntimeHandle) {
        if handle.state == RuntimeState::Exited {
            self.remove_runtime_id(handle.runtime_id);
            return;
        }
        self.track(handle);
    }

    pub(crate) fn apply_handles(&self, handles: Vec<RuntimeHandle>) {
        for handle in handles {
            self.apply_handle(handle);
        }
    }

    pub(crate) fn remove_runtime_id(&self, runtime_id: Uuid) -> Option<RuntimeHandle> {
        let mut runtimes = self.inner.write().expect("runtime read lock poisoned");
        let removed = runtimes.by_runtime_id.remove(&runtime_id)?;
        if runtimes
            .by_task_attempt
            .get(&(removed.task_id, removed.attempt_no))
            == Some(&runtime_id)
        {
            runtimes
                .by_task_attempt
                .remove(&(removed.task_id, removed.attempt_no));
        }
        Some(removed)
    }

    pub(crate) fn remove_by_task_attempt(
        &self,
        task_id: Uuid,
        attempt_no: i32,
    ) -> Option<RuntimeHandle> {
        let mut runtimes = self.inner.write().expect("runtime read lock poisoned");
        let runtime_id = runtimes.by_task_attempt.remove(&(task_id, attempt_no))?;
        runtimes.by_runtime_id.remove(&runtime_id)
    }

    fn track(&self, handle: RuntimeHandle) {
        let mut runtimes = self.inner.write().expect("runtime read lock poisoned");
        let key = (handle.task_id, handle.attempt_no);
        if let Some(previous_runtime_id) = runtimes.by_task_attempt.insert(key, handle.runtime_id) {
            if previous_runtime_id != handle.runtime_id {
                runtimes.by_runtime_id.remove(&previous_runtime_id);
            }
        }
        runtimes.by_runtime_id.insert(handle.runtime_id, handle);
    }

    pub fn state_counts(&self) -> RuntimeStateCounts {
        let runtimes = self.inner.read().expect("runtime read lock poisoned");
        state_counts_from_handles(runtimes.by_runtime_id.values())
    }

    pub fn active_handles(&self) -> Vec<RuntimeHandle> {
        let runtimes = self.inner.read().expect("runtime read lock poisoned");
        runtimes
            .by_runtime_id
            .values()
            .filter(|handle| handle.state != RuntimeState::Exited)
            .cloned()
            .collect()
    }

    pub fn find_by_task_attempt(&self, task_id: Uuid, attempt_no: i32) -> Option<RuntimeHandle> {
        let runtimes = self.inner.read().expect("runtime read lock poisoned");
        let runtime_id = runtimes.by_task_attempt.get(&(task_id, attempt_no))?;
        runtimes.by_runtime_id.get(runtime_id).cloned()
    }

    pub fn snapshots(&self, filter: &AdoptFilter) -> Vec<RuntimeHandle> {
        let runtimes = self.inner.read().expect("runtime read lock poisoned");
        runtimes
            .by_runtime_id
            .values()
            .filter(|handle| filter.matches(handle))
            .cloned()
            .collect()
    }
}

impl Default for RuntimeReadHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl Default for LocalRuntimeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeReadModel for LocalRuntimeRegistry {
    fn state_counts(&self) -> RuntimeStateCounts {
        LocalRuntimeRegistry::state_counts(self)
    }

    fn active_handles(&self) -> Vec<RuntimeHandle> {
        LocalRuntimeRegistry::active_handles(self)
    }

    fn find_by_task_attempt(&self, task_id: Uuid, attempt_no: i32) -> Option<RuntimeHandle> {
        LocalRuntimeRegistry::find_by_task_attempt(self, task_id, attempt_no)
    }

    fn snapshots(&self, filter: &AdoptFilter) -> Vec<RuntimeHandle> {
        LocalRuntimeRegistry::snapshots(self, filter)
    }
}

impl RuntimeReadModel for RuntimeReadHandle {
    fn state_counts(&self) -> RuntimeStateCounts {
        RuntimeReadHandle::state_counts(self)
    }

    fn active_handles(&self) -> Vec<RuntimeHandle> {
        RuntimeReadHandle::active_handles(self)
    }

    fn find_by_task_attempt(&self, task_id: Uuid, attempt_no: i32) -> Option<RuntimeHandle> {
        RuntimeReadHandle::find_by_task_attempt(self, task_id, attempt_no)
    }

    fn snapshots(&self, filter: &AdoptFilter) -> Vec<RuntimeHandle> {
        RuntimeReadHandle::snapshots(self, filter)
    }
}

fn state_counts_from_handles<'a>(
    handles: impl Iterator<Item = &'a RuntimeHandle>,
) -> RuntimeStateCounts {
    let mut counts = RuntimeStateCounts::default();
    for handle in handles {
        match handle.state {
            RuntimeState::Pending | RuntimeState::Starting => {
                counts.starting = counts.starting.saturating_add(1);
            }
            RuntimeState::Running => {
                counts.running = counts.running.saturating_add(1);
            }
            RuntimeState::Stopping => {
                counts.stopping = counts.stopping.saturating_add(1);
            }
            RuntimeState::Orphaned => {
                counts.orphaned = counts.orphaned.saturating_add(1);
            }
            RuntimeState::Exited => {}
        }
    }
    counts
}

impl AdoptFilter {
    pub(crate) fn matches(&self, handle: &RuntimeHandle) -> bool {
        if self.runtimes.is_empty() {
            return false;
        }

        self.runtimes.iter().any(|runtime| {
            runtime.task_id == handle.task_id
                && runtime.attempt_no == handle.attempt_no
                && runtime.worker_kind == handle.worker_kind
                && runtime.lease_token == runtime_lease_token(handle).unwrap_or_default()
        })
    }
}

fn runtime_lease_token(handle: &RuntimeHandle) -> Option<String> {
    handle
        .metadata
        .get("lease_token")
        .and_then(Value::as_str)
        .map(str::to_string)
}
