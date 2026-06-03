//! 运行时状态持久化：负责 runtime.json/runtime.pid/runtime.cmd 的写入、扫描、终态重放和清理。

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use media_domain::{RuntimeHandle, RuntimeState};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    runtime::{ExecutorError, SuccessCheck, classify_adopted_exit, runtime_lease_token},
    runtime_events::{RuntimeTaskEvent, TerminalRuntimeReplay, runtime_session_epoch},
    runtime_registry::LocalRuntimeRegistry,
};

pub(crate) const RUNTIME_STATE_FILE: &str = "runtime.json";
pub(crate) const RUNTIME_PID_FILE: &str = "runtime.pid";
pub(crate) const RUNTIME_COMMAND_FILE: &str = "runtime.cmd";

static ATOMIC_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PersistedRuntimeState {
    pub(crate) handle: RuntimeHandle,
    pub(crate) work_dir: PathBuf,
    pub(crate) success_check: SuccessCheck,
}

pub(crate) fn persist_runtime_state(
    work_dir: &Path,
    handle: &RuntimeHandle,
    success_check: &SuccessCheck,
) -> Result<(), ExecutorError> {
    fs::create_dir_all(work_dir).map_err(|error| {
        ExecutorError::ProcessSpawn(format!(
            "failed to prepare runtime dir {}: {error}",
            work_dir.display()
        ))
    })?;

    let state = PersistedRuntimeState {
        handle: handle.clone(),
        work_dir: work_dir.to_path_buf(),
        success_check: success_check.clone(),
    };
    let state_json = serde_json::to_vec_pretty(&state)
        .map_err(|error| ExecutorError::ProcessSpawn(error.to_string()))?;
    atomic_write(&work_dir.join(RUNTIME_STATE_FILE), &state_json).map_err(|error| {
        ExecutorError::ProcessSpawn(format!(
            "failed to write runtime state {}: {error}",
            work_dir.join(RUNTIME_STATE_FILE).display()
        ))
    })?;

    let pid_path = work_dir.join(RUNTIME_PID_FILE);
    if let Some(pid) = handle.pid {
        atomic_write(&pid_path, pid.to_string().as_bytes()).map_err(|error| {
            ExecutorError::ProcessSpawn(format!(
                "failed to write runtime pid {}: {error}",
                pid_path.display()
            ))
        })?;
    } else {
        let _ = fs::remove_file(&pid_path);
    }

    let command_path = work_dir.join(RUNTIME_COMMAND_FILE);
    if let Some(command_line) = handle.command_line.as_deref() {
        atomic_write(&command_path, command_line.as_bytes()).map_err(|error| {
            ExecutorError::ProcessSpawn(format!(
                "failed to write runtime command {}: {error}",
                command_path.display()
            ))
        })?;
    } else {
        let _ = fs::remove_file(&command_path);
    }

    Ok(())
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("target path has no file name: {}", path.display()),
        )
    })?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    let mut tmp_name = file_name.to_os_string();
    tmp_name.push(format!(
        ".tmp.{}.{}",
        std::process::id(),
        ATOMIC_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp_path = parent.join(tmp_name);

    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp_path, path)?;
        if let Ok(parent_dir) = fs::File::open(parent) {
            let _ = parent_dir.sync_all();
        }
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }

    result
}

pub(crate) fn success_check_from_handle(handle: &RuntimeHandle) -> SuccessCheck {
    let local_outputs = handle
        .outputs
        .iter()
        .filter(|output| !output.contains("://"))
        .map(PathBuf::from)
        .collect::<Vec<_>>();

    match local_outputs.as_slice() {
        [] => SuccessCheck::ProcessExit,
        [path] => SuccessCheck::FileExists(path.clone()),
        _ => SuccessCheck::FilesExist(local_outputs),
    }
}

pub(crate) fn scan_persisted_runtimes(work_root: &str) -> Vec<PersistedRuntimeState> {
    scan_runtime_states(work_root, |state| {
        !matches!(
            state.handle.state,
            RuntimeState::Exited | RuntimeState::Pending
        )
    })
}

fn scan_exited_persisted_runtimes(work_root: &str) -> Vec<PersistedRuntimeState> {
    scan_runtime_states(work_root, |state| {
        state.handle.state == RuntimeState::Exited
    })
}

fn scan_runtime_states(
    work_root: &str,
    include: impl Fn(&PersistedRuntimeState) -> bool,
) -> Vec<PersistedRuntimeState> {
    let root = Path::new(work_root);
    if !root.exists() {
        return Vec::new();
    }

    let mut pending = vec![root.to_path_buf()];
    let mut states = Vec::new();
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            if path.file_name().and_then(|name| name.to_str()) != Some(RUNTIME_STATE_FILE) {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            let Ok(state) = serde_json::from_slice::<PersistedRuntimeState>(&bytes) else {
                continue;
            };
            if include(&state) {
                states.push(state);
            }
        }
    }
    states
}

fn stop_requested_from_persisted_handle(handle: &RuntimeHandle) -> bool {
    handle
        .metadata
        .get("stop")
        .map(|value| !value.is_null())
        .unwrap_or(false)
}

fn classify_replayed_exit(
    handle: &RuntimeHandle,
    success_check: &SuccessCheck,
) -> (&'static str, &'static str, String, Value) {
    let (event_type, event_level, message, mut payload) = classify_adopted_exit(
        handle,
        success_check,
        stop_requested_from_persisted_handle(handle),
    );
    if let Some(object) = payload.as_object_mut() {
        object.remove("orphaned");
        object.insert("replayed".to_string(), json!(true));
    }
    (event_type, event_level, message, payload)
}

pub fn collect_terminal_runtime_replays(
    work_root: &str,
    registry: &LocalRuntimeRegistry,
) -> Vec<TerminalRuntimeReplay> {
    scan_exited_persisted_runtimes(work_root)
        .into_iter()
        .filter(|state| stop_requested_from_persisted_handle(&state.handle))
        .filter(|state| {
            registry
                .find_by_task_attempt(state.handle.task_id, state.handle.attempt_no)
                .is_none()
        })
        .map(|state| {
            let (event_type, event_level, message, payload) =
                classify_replayed_exit(&state.handle, &state.success_check);
            TerminalRuntimeReplay {
                handle: state.handle.clone(),
                event: RuntimeTaskEvent {
                    task_id: state.handle.task_id,
                    attempt_no: state.handle.attempt_no,
                    lease_token: runtime_lease_token(&state.handle).unwrap_or_default(),
                    session_epoch: runtime_session_epoch(&state.handle),
                    event_type: event_type.to_string(),
                    event_level: event_level.to_string(),
                    message,
                    payload,
                },
            }
        })
        .collect()
}

pub fn cleanup_persisted_runtime_state(work_root: &str, task_id: Uuid, attempt_no: i32) {
    let work_dir = Path::new(work_root)
        .join(task_id.to_string())
        .join(format!("attempt-{attempt_no}"));
    let _ = fs::remove_file(work_dir.join(RUNTIME_STATE_FILE));
    let _ = fs::remove_file(work_dir.join(RUNTIME_PID_FILE));
    let _ = fs::remove_file(work_dir.join(RUNTIME_COMMAND_FILE));
}

pub fn is_terminal_runtime_event(event_type: &str) -> bool {
    matches!(event_type, "canceled" | "failed" | "succeeded")
}
