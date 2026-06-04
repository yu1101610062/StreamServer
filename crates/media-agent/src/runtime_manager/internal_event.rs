use std::{path::PathBuf, process::ExitStatus};

use media_domain::RuntimeHandle;
use serde_json::Value;
use uuid::Uuid;

use crate::{
    runtime::{RuntimeNotification, RuntimeTaskProgress, SuccessCheck},
    runtime_process::ProcessIdentity,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RuntimeGeneration(u64);

impl RuntimeGeneration {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    pub(crate) const fn value(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RuntimeMonitorSnapshot {
    pub(crate) handle: RuntimeHandle,
    pub(crate) stop_requested: bool,
    pub(crate) companion_processes: Vec<ProcessIdentity>,
}

#[derive(Debug, Clone)]
pub(crate) struct RuntimeMonitorCommit {
    pub(crate) runtime_id: Uuid,
    pub(crate) generation: RuntimeGeneration,
    pub(crate) handle: RuntimeHandle,
    pub(crate) persist: Option<RuntimePersistRequest>,
    pub(crate) notifications: Vec<RuntimeNotification>,
    pub(crate) remove_runtime_entry: bool,
    pub(crate) remove_backend: bool,
    pub(crate) mark_stop_requested: Option<bool>,
    pub(crate) suppress_companion_events: Option<bool>,
    pub(crate) remove_companion_pid: Option<i32>,
}

impl RuntimeMonitorCommit {
    pub(crate) fn new(handle: RuntimeHandle, generation: RuntimeGeneration) -> Self {
        Self {
            runtime_id: handle.runtime_id,
            generation,
            handle,
            persist: None,
            notifications: Vec::new(),
            remove_runtime_entry: false,
            remove_backend: false,
            mark_stop_requested: None,
            suppress_companion_events: None,
            remove_companion_pid: None,
        }
    }

    pub(crate) fn with_persist(mut self, work_dir: PathBuf, success_check: SuccessCheck) -> Self {
        self.persist = Some(RuntimePersistRequest {
            work_dir,
            success_check,
        });
        self
    }

    pub(crate) fn with_notifications(mut self, notifications: Vec<RuntimeNotification>) -> Self {
        self.notifications = notifications;
        self
    }

    pub(crate) fn terminal(mut self) -> Self {
        self.remove_runtime_entry = true;
        self.remove_backend = true;
        self
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RuntimePersistRequest {
    pub(crate) work_dir: PathBuf,
    pub(crate) success_check: SuccessCheck,
}

#[derive(Debug)]
pub(crate) enum RuntimeInternalEvent {
    ProcessExited(ProcessExitedEvent),
    StartupProbeSucceeded(RuntimeMonitorCommit),
    StartupProbeFailed(RuntimeMonitorCommit),
    LiveRelayOffline(RuntimeMonitorCommit),
    RtpServerMissing(RuntimeMonitorCommit),
    RecordDurationReached(RecordDurationReachedEvent),
    ProgressObserved(ProgressObservedEvent),
    #[allow(dead_code)]
    PersistenceFailed(PersistenceFailedEvent),
    CompanionProcessExited(CompanionProcessExitedEvent),
    ApplyMonitorCommit(RuntimeMonitorCommit),
}

impl RuntimeInternalEvent {
    pub(crate) fn runtime_id(&self) -> Uuid {
        match self {
            Self::ProcessExited(event) => event.runtime_id,
            Self::StartupProbeSucceeded(commit)
            | Self::StartupProbeFailed(commit)
            | Self::LiveRelayOffline(commit)
            | Self::RtpServerMissing(commit)
            | Self::ApplyMonitorCommit(commit) => commit.runtime_id,
            Self::RecordDurationReached(event) => event.runtime_id,
            Self::ProgressObserved(event) => event.runtime_id,
            Self::PersistenceFailed(event) => event.runtime_id,
            Self::CompanionProcessExited(event) => event.runtime_id,
        }
    }

    pub(crate) fn generation(&self) -> RuntimeGeneration {
        match self {
            Self::ProcessExited(event) => event.generation,
            Self::StartupProbeSucceeded(commit)
            | Self::StartupProbeFailed(commit)
            | Self::LiveRelayOffline(commit)
            | Self::RtpServerMissing(commit)
            | Self::ApplyMonitorCommit(commit) => commit.generation,
            Self::RecordDurationReached(event) => event.generation,
            Self::ProgressObserved(event) => event.generation,
            Self::PersistenceFailed(event) => event.generation,
            Self::CompanionProcessExited(event) => event.generation,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ProcessExitedEvent {
    pub(crate) runtime_id: Uuid,
    pub(crate) generation: RuntimeGeneration,
    pub(crate) work_dir: PathBuf,
    pub(crate) output_target: String,
    pub(crate) success_check: SuccessCheck,
    pub(crate) status: Result<ExitStatus, String>,
    pub(crate) was_stopped: bool,
}

#[derive(Debug)]
pub(crate) struct ProgressObservedEvent {
    pub(crate) runtime_id: Uuid,
    pub(crate) generation: RuntimeGeneration,
    pub(crate) progress: RuntimeTaskProgress,
}

#[derive(Debug)]
pub(crate) struct RecordDurationReachedEvent {
    pub(crate) runtime_id: Uuid,
    pub(crate) generation: RuntimeGeneration,
}

#[derive(Debug)]
pub(crate) struct PersistenceFailedEvent {
    pub(crate) runtime_id: Uuid,
    pub(crate) generation: RuntimeGeneration,
    pub(crate) error: String,
}

#[derive(Debug)]
pub(crate) struct CompanionProcessExitedEvent {
    pub(crate) runtime_id: Uuid,
    pub(crate) generation: RuntimeGeneration,
    pub(crate) companion_pid: i32,
    pub(crate) task_id: Uuid,
    pub(crate) attempt_no: i32,
    pub(crate) work_dir: PathBuf,
    pub(crate) success_check: SuccessCheck,
    pub(crate) succeeded: bool,
    pub(crate) error: Option<String>,
    pub(crate) exit_payload: Value,
}
