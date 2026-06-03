use std::{
    collections::HashSet,
    sync::{Arc, Mutex as StdMutex},
};

use uuid::Uuid;

use crate::runtime::ExecutorError;

pub(crate) struct RecordingControlGuard {
    active: Arc<StdMutex<HashSet<Uuid>>>,
    runtime_id: Uuid,
}

impl RecordingControlGuard {
    pub(crate) fn acquire(
        active: Arc<StdMutex<HashSet<Uuid>>>,
        runtime_id: Uuid,
    ) -> Result<Self, ExecutorError> {
        let mut active_controls = active.lock().expect("recording controls lock poisoned");
        if !active_controls.insert(runtime_id) {
            return Err(ExecutorError::InvalidRequest(
                "recording control is already in progress for this runtime".to_string(),
            ));
        }
        drop(active_controls);
        Ok(Self { active, runtime_id })
    }
}

impl Drop for RecordingControlGuard {
    fn drop(&mut self) {
        self.active
            .lock()
            .expect("recording controls lock poisoned")
            .remove(&self.runtime_id);
    }
}
