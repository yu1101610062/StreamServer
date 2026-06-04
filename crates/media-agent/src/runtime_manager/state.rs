use std::collections::{HashMap, HashSet};

use media_domain::{RuntimeHandle, RuntimeState, WorkerKind};
use uuid::Uuid;

#[cfg(test)]
use crate::runtime_registry::RuntimeReadModel;
use crate::runtime_registry::RuntimeStateCounts;

use super::internal_event::RuntimeGeneration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RuntimeOperationId(u64);

impl RuntimeOperationId {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeEntry {
    pub handle: RuntimeHandle,
    pub backend: RuntimeBackendEntry,
    pub generation: RuntimeGeneration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeBackendEntry {
    pub worker_kind: WorkerKind,
    pub pid: Option<i32>,
}

impl RuntimeBackendEntry {
    fn from_handle(handle: &RuntimeHandle) -> Self {
        Self {
            worker_kind: handle.worker_kind,
            pid: handle.pid,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RuntimeManagerState {
    by_runtime_id: HashMap<Uuid, RuntimeEntry>,
    by_task_attempt: HashMap<(Uuid, i32), Uuid>,
    state_counts: RuntimeStateCounts,
    pending_operation_ids: HashSet<RuntimeOperationId>,
}

impl RuntimeManagerState {
    pub(crate) fn track_operation(&mut self, operation_id: RuntimeOperationId) {
        self.pending_operation_ids.insert(operation_id);
    }

    pub(crate) fn finish_operation(&mut self, operation_id: RuntimeOperationId) {
        self.pending_operation_ids.remove(&operation_id);
    }

    pub(crate) fn apply_handle(&mut self, handle: RuntimeHandle) {
        let generation = self
            .by_runtime_id
            .get(&handle.runtime_id)
            .map(|entry| entry.generation)
            .unwrap_or_else(|| RuntimeGeneration::new(0));
        self.apply_handle_with_generation(handle, generation);
    }

    pub(crate) fn apply_handle_with_generation(
        &mut self,
        handle: RuntimeHandle,
        generation: RuntimeGeneration,
    ) {
        if handle.state == RuntimeState::Exited {
            self.remove_handle(&handle);
            return;
        }

        let key = (handle.task_id, handle.attempt_no);
        if let Some(previous) = self.by_runtime_id.get(&handle.runtime_id) {
            let previous_key = (previous.handle.task_id, previous.handle.attempt_no);
            if previous_key != key
                && self.by_task_attempt.get(&previous_key) == Some(&handle.runtime_id)
            {
                self.by_task_attempt.remove(&previous_key);
            }
        }
        if let Some(previous_runtime_id) = self.by_task_attempt.insert(key, handle.runtime_id) {
            if previous_runtime_id != handle.runtime_id {
                self.by_runtime_id.remove(&previous_runtime_id);
            }
        }

        self.by_runtime_id.insert(
            handle.runtime_id,
            RuntimeEntry {
                backend: RuntimeBackendEntry::from_handle(&handle),
                generation,
                handle,
            },
        );
        self.recompute_counts();
    }

    pub(crate) fn entry(&self, runtime_id: Uuid) -> Option<&RuntimeEntry> {
        self.by_runtime_id.get(&runtime_id)
    }

    pub(crate) fn entry_by_task_attempt(
        &self,
        task_id: Uuid,
        attempt_no: i32,
    ) -> Option<&RuntimeEntry> {
        let runtime_id = self.by_task_attempt.get(&(task_id, attempt_no))?;
        self.by_runtime_id.get(runtime_id)
    }

    #[cfg(test)]
    pub fn active_handles(&self) -> Vec<RuntimeHandle> {
        self.by_runtime_id
            .values()
            .map(|entry| entry.handle.clone())
            .collect()
    }

    fn remove_handle(&mut self, handle: &RuntimeHandle) {
        self.remove_runtime_id(handle.runtime_id);
        let key = (handle.task_id, handle.attempt_no);
        if self.by_task_attempt.get(&key) == Some(&handle.runtime_id) {
            self.by_task_attempt.remove(&key);
            self.recompute_counts();
        }
    }

    pub(crate) fn remove_runtime_id(&mut self, runtime_id: Uuid) {
        let Some(entry) = self.by_runtime_id.remove(&runtime_id) else {
            return;
        };
        let key = (entry.handle.task_id, entry.handle.attempt_no);
        if self.by_task_attempt.get(&key) == Some(&runtime_id) {
            self.by_task_attempt.remove(&key);
        }
        self.recompute_counts();
    }

    fn recompute_counts(&mut self) {
        let mut counts = RuntimeStateCounts::default();
        for entry in self.by_runtime_id.values() {
            match entry.handle.state {
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
        self.state_counts = counts;
    }

    #[cfg(test)]
    pub(crate) fn assert_consistent_with_read_model(&self, read_model: &dyn RuntimeReadModel) {
        let errors = self.consistency_errors(read_model);
        assert!(
            errors.is_empty(),
            "runtime manager state diverged from read model:\n{}",
            errors.join("\n")
        );
    }

    #[cfg(test)]
    pub(crate) fn consistency_errors(&self, read_model: &dyn RuntimeReadModel) -> Vec<String> {
        let mut errors = Vec::new();
        let read_counts = read_model.state_counts();
        if self.state_counts != read_counts {
            errors.push(format!(
                "state counts differ: state {} read model {}",
                format_counts(self.state_counts),
                format_counts(read_counts)
            ));
        }

        let state_by_runtime_id = self
            .active_handles()
            .into_iter()
            .map(|handle| (handle.runtime_id, handle))
            .collect::<HashMap<_, _>>();
        let read_by_runtime_id = read_model
            .active_handles()
            .into_iter()
            .map(|handle| (handle.runtime_id, handle))
            .collect::<HashMap<_, _>>();

        for (runtime_id, state) in &state_by_runtime_id {
            match read_by_runtime_id.get(runtime_id) {
                Some(read) => {
                    if state.state != read.state
                        || state.task_id != read.task_id
                        || state.attempt_no != read.attempt_no
                        || state.worker_kind != read.worker_kind
                    {
                        errors.push(format!(
                            "runtime diff runtime_id={} task_id={} attempt_no={} worker_kind={} manager state={} read model state={} read task_id={} read attempt_no={} read worker_kind={}",
                            runtime_id,
                            state.task_id,
                            state.attempt_no,
                            state.worker_kind,
                            state.state,
                            read.state,
                            read.task_id,
                            read.attempt_no,
                            read.worker_kind
                        ));
                    }
                }
                None => errors.push(format!("extra state {}", describe_handle(state))),
            }

            match read_model.find_by_task_attempt(state.task_id, state.attempt_no) {
                Some(read) if read.runtime_id == *runtime_id => {}
                Some(read) => errors.push(format!(
                    "task index diff task_id={} attempt_no={} state runtime_id={} read model runtime_id={} read model state={}",
                    state.task_id, state.attempt_no, runtime_id, read.runtime_id, read.state
                )),
                None => errors.push(format!(
                    "missing read model task index task_id={} attempt_no={} state runtime_id={} manager state={}",
                    state.task_id, state.attempt_no, runtime_id, state.state
                )),
            }
        }

        for (runtime_id, read) in &read_by_runtime_id {
            if !state_by_runtime_id.contains_key(runtime_id) {
                errors.push(format!("missing state {}", describe_handle(read)));
            }
            match self.entry_by_task_attempt(read.task_id, read.attempt_no) {
                Some(entry) if entry.handle.runtime_id == *runtime_id => {}
                Some(entry) => errors.push(format!(
                    "state task index diff task_id={} attempt_no={} state runtime_id={} read model runtime_id={} read model state={}",
                    read.task_id,
                    read.attempt_no,
                    entry.handle.runtime_id,
                    runtime_id,
                    read.state
                )),
                None => errors.push(format!(
                    "missing state task index task_id={} attempt_no={} read model runtime_id={} read model state={}",
                    read.task_id, read.attempt_no, runtime_id, read.state
                )),
            }
        }

        errors
    }
}

#[cfg(test)]
fn format_counts(counts: RuntimeStateCounts) -> String {
    format!(
        "running={} starting={} stopping={} orphaned={}",
        counts.running, counts.starting, counts.stopping, counts.orphaned
    )
}

#[cfg(test)]
fn describe_handle(handle: &RuntimeHandle) -> String {
    format!(
        "runtime_id={} task_id={} attempt_no={} worker_kind={} state={}",
        handle.runtime_id, handle.task_id, handle.attempt_no, handle.worker_kind, handle.state
    )
}
