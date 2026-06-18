//! 运行时事件通道：负责 Agent 本地运行时的事件、日志批次、进度消息以及 stdout/stderr 读取上报。

use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::{Duration, SystemTime},
};

use media_domain::RuntimeHandle;
use serde::Serialize;
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::mpsc,
    time::timeout,
};
use uuid::Uuid;

use crate::runtime_manager::{ProgressObservedEvent, RuntimeInternalEvent, RuntimeMonitorHandle};

const LOG_BATCH_FLUSH_INTERVAL: Duration = Duration::from_millis(250);
const MAX_LOG_BATCH_LINES: usize = 64;
pub(crate) const MAX_LOG_BATCH_BYTES: usize = 512 * 1024;
const LOG_LINE_TRUNCATED_MARKER: &str = " ... [truncated]";
const SECONDS_PER_DAY: u64 = 86_400;

#[derive(Debug, Clone)]
pub enum RuntimeNotification {
    TaskEvent(RuntimeTaskEvent),
    TaskLogBatch(RuntimeTaskLogBatch),
    TaskProgress(RuntimeTaskProgress),
    TaskSnapshot(RuntimeHandle),
}

#[derive(Debug, Clone)]
pub struct RuntimeTaskEvent {
    pub task_id: Uuid,
    pub attempt_no: i32,
    pub lease_token: String,
    pub session_epoch: u64,
    pub event_type: String,
    pub event_level: String,
    pub message: String,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub struct RuntimeTaskLogBatch {
    pub task_id: Uuid,
    pub attempt_no: i32,
    pub lease_token: String,
    pub session_epoch: u64,
    pub stream: String,
    pub lines: Vec<String>,
    pub source_line_count: usize,
}

#[derive(Debug, Clone)]
pub struct RuntimeTaskProgress {
    pub task_id: Uuid,
    pub attempt_no: i32,
    pub lease_token: String,
    pub session_epoch: u64,
    pub frame: u64,
    pub fps: f64,
    pub bitrate_kbps: f64,
    pub speed: f64,
    pub out_time_ms: u64,
    pub dup_frames: u64,
    pub drop_frames: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeDiagnosticLog {
    pub stream: String,
    pub path: String,
    pub size_bytes: u64,
    pub line_count: u64,
    pub tail: String,
    pub file_truncated: bool,
    pub tail_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RuntimeLogKey {
    task_id: Uuid,
    attempt_no: i32,
    stream: String,
}

impl RuntimeLogKey {
    fn new(task_id: Uuid, attempt_no: i32, stream: impl Into<String>) -> Self {
        Self {
            task_id,
            attempt_no,
            stream: stream.into(),
        }
    }

    fn from_batch(batch: &RuntimeTaskLogBatch) -> Self {
        Self {
            task_id: batch.task_id,
            attempt_no: batch.attempt_no,
            stream: batch.stream.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct RuntimeDiagnosticLogState {
    stream: String,
    path: String,
    size_bytes: u64,
    line_count: u64,
    tail: Vec<u8>,
    tail_truncated: bool,
    file_truncated: bool,
}

impl RuntimeDiagnosticLogState {
    fn new(stream: String, path: PathBuf) -> Self {
        Self {
            stream,
            path: path.display().to_string(),
            size_bytes: 0,
            line_count: 0,
            tail: Vec::new(),
            tail_truncated: false,
            file_truncated: false,
        }
    }

    fn observe(&mut self, line: &str, bytes_written: u64, tail_bytes: usize, file_truncated: bool) {
        self.size_bytes = self.size_bytes.saturating_add(bytes_written);
        self.line_count = self.line_count.saturating_add(1);
        self.file_truncated |= file_truncated;
        if tail_bytes == 0 {
            self.tail.clear();
            self.tail_truncated = true;
            return;
        }

        self.tail.extend_from_slice(line.as_bytes());
        self.tail.push(b'\n');
        if self.tail.len() > tail_bytes {
            let overflow = self.tail.len() - tail_bytes;
            let mut split = overflow;
            while split < self.tail.len() && !std::str::from_utf8(&self.tail[split..]).is_ok() {
                split += 1;
            }
            self.tail.drain(..split.min(self.tail.len()));
            self.tail_truncated = true;
        }
    }

    fn snapshot(&self) -> RuntimeDiagnosticLog {
        RuntimeDiagnosticLog {
            stream: self.stream.clone(),
            path: self.path.clone(),
            size_bytes: self.size_bytes,
            line_count: self.line_count,
            tail: String::from_utf8_lossy(&self.tail).to_string(),
            file_truncated: self.file_truncated,
            tail_truncated: self.tail_truncated,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeEventSink {
    priority_tx: mpsc::UnboundedSender<RuntimeNotification>,
    log_tx: mpsc::Sender<RuntimeTaskLogBatch>,
    suppressed_logs: Arc<RwLock<HashMap<RuntimeLogKey, usize>>>,
    diagnostic_logs: Arc<RwLock<HashMap<RuntimeLogKey, RuntimeDiagnosticLogState>>>,
}

impl RuntimeEventSink {
    pub fn new(
        priority_tx: mpsc::UnboundedSender<RuntimeNotification>,
        log_tx: mpsc::Sender<RuntimeTaskLogBatch>,
    ) -> Self {
        Self {
            priority_tx,
            log_tx,
            suppressed_logs: Arc::new(RwLock::new(HashMap::new())),
            diagnostic_logs: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn send(&self, notification: RuntimeNotification) -> Result<(), ()> {
        match notification {
            RuntimeNotification::TaskLogBatch(batch) => self.send_log_batch(batch),
            notification => self.priority_tx.send(notification).map_err(|_| ()),
        }
    }

    fn send_log_batch(&self, mut batch: RuntimeTaskLogBatch) -> Result<(), ()> {
        let key = RuntimeLogKey::from_batch(&batch);
        let suppressed = self
            .suppressed_logs
            .write()
            .expect("suppressed logs lock poisoned")
            .remove(&key)
            .unwrap_or(0);
        if suppressed > 0 {
            batch.lines.insert(
                0,
                format!("suppressed {suppressed} {} log lines", batch.stream),
            );
        }

        let batches = bounded_log_batches(batch);
        let mut delivered_suppressed_notice = suppressed == 0;
        for (index, batch) in batches.iter().cloned().enumerate() {
            match self.log_tx.try_send(batch) {
                Ok(()) => {
                    if index == 0 {
                        delivered_suppressed_notice = true;
                    }
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(batch)) => {
                    let mut unsent = batch.source_line_count
                        + batches
                            .iter()
                            .skip(index + 1)
                            .map(|batch| batch.source_line_count)
                            .sum::<usize>();
                    if !delivered_suppressed_notice {
                        unsent += suppressed;
                    }
                    let mut suppressed_logs = self
                        .suppressed_logs
                        .write()
                        .expect("suppressed logs lock poisoned");
                    *suppressed_logs.entry(key).or_insert(0) += unsent;
                    return Ok(());
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return Err(()),
            }
        }
        Ok(())
    }

    pub fn register_diagnostic_log(
        &self,
        task_id: Uuid,
        attempt_no: i32,
        stream: &str,
        path: PathBuf,
    ) {
        let mut logs = self
            .diagnostic_logs
            .write()
            .expect("diagnostic logs lock poisoned");
        logs.entry(RuntimeLogKey::new(task_id, attempt_no, stream))
            .or_insert_with(|| RuntimeDiagnosticLogState::new(stream.to_string(), path));
    }

    pub fn observe_diagnostic_log_line(
        &self,
        task_id: Uuid,
        attempt_no: i32,
        stream: &str,
        line: &str,
        bytes_written: u64,
        tail_bytes: usize,
        file_truncated: bool,
    ) {
        let mut logs = self
            .diagnostic_logs
            .write()
            .expect("diagnostic logs lock poisoned");
        if let Some(log) = logs.get_mut(&RuntimeLogKey::new(task_id, attempt_no, stream)) {
            log.observe(line, bytes_written, tail_bytes, file_truncated);
        }
    }

    pub fn diagnostic_logs(&self, task_id: Uuid, attempt_no: i32) -> Vec<RuntimeDiagnosticLog> {
        let logs = self
            .diagnostic_logs
            .read()
            .expect("diagnostic logs lock poisoned");
        let mut snapshots = logs
            .iter()
            .filter(|(key, _)| key.task_id == task_id && key.attempt_no == attempt_no)
            .map(|(_, log)| log.snapshot())
            .collect::<Vec<_>>();
        snapshots.sort_by(|left, right| left.stream.cmp(&right.stream));
        snapshots
    }
}

pub(crate) fn bounded_log_batches(batch: RuntimeTaskLogBatch) -> Vec<RuntimeTaskLogBatch> {
    let RuntimeTaskLogBatch {
        task_id,
        attempt_no,
        lease_token,
        session_epoch,
        stream,
        lines,
        source_line_count,
    } = batch;

    let line_count = lines.len();
    let synthetic_prefix_lines = line_count.saturating_sub(source_line_count);
    let extra_source_lines = source_line_count.saturating_sub(line_count);
    let mut batches = Vec::new();
    let mut current_lines = Vec::new();
    let mut current_source_line_count = 0usize;
    let mut current_bytes = 0usize;

    for (index, line) in lines.into_iter().enumerate() {
        let line = truncate_log_line(line);
        let line_bytes = log_line_wire_bytes(&line);
        let line_source_count = if index < synthetic_prefix_lines {
            0
        } else {
            1 + usize::from(index == synthetic_prefix_lines) * extra_source_lines
        };

        if !current_lines.is_empty() && current_bytes + line_bytes > MAX_LOG_BATCH_BYTES {
            batches.push(RuntimeTaskLogBatch {
                task_id,
                attempt_no,
                lease_token: lease_token.clone(),
                session_epoch,
                stream: stream.clone(),
                lines: std::mem::take(&mut current_lines),
                source_line_count: current_source_line_count,
            });
            current_source_line_count = 0;
            current_bytes = 0;
        }

        current_bytes += line_bytes;
        current_source_line_count += line_source_count;
        current_lines.push(line);
    }

    if !current_lines.is_empty() {
        batches.push(RuntimeTaskLogBatch {
            task_id,
            attempt_no,
            lease_token,
            session_epoch,
            stream,
            lines: current_lines,
            source_line_count: current_source_line_count,
        });
    }

    batches
}

fn truncate_log_line(line: String) -> String {
    if log_line_wire_bytes(&line) <= MAX_LOG_BATCH_BYTES {
        return line;
    }

    let max_content_bytes = MAX_LOG_BATCH_BYTES
        .saturating_sub(LOG_LINE_TRUNCATED_MARKER.len())
        .saturating_sub(1);
    let mut end = max_content_bytes.min(line.len());
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &line[..end], LOG_LINE_TRUNCATED_MARKER)
}

fn log_line_wire_bytes(line: &str) -> usize {
    line.len().saturating_add(1)
}

#[derive(Debug, Clone)]
pub struct TerminalRuntimeReplay {
    pub handle: RuntimeHandle,
    pub event: RuntimeTaskEvent,
}

struct RuntimeLogFile {
    file: Option<std::fs::File>,
    written: u64,
    max_file_bytes: u64,
    truncated: bool,
}

impl RuntimeLogFile {
    fn open(path: &Path, max_file_bytes: u64) -> Self {
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)
            .map_err(|error| {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "failed to open runtime diagnostic log file"
                );
                error
            })
            .ok();
        Self {
            file,
            written: 0,
            max_file_bytes,
            truncated: false,
        }
    }

    fn append_line(&mut self, line: &str) -> (u64, bool) {
        let Some(file) = self.file.as_mut() else {
            self.truncated = true;
            return (0, true);
        };
        if self.max_file_bytes == 0 || self.written >= self.max_file_bytes {
            self.truncated = true;
            return (0, true);
        }

        let line_bytes = line.as_bytes();
        let requested = line_bytes.len().saturating_add(1);
        let remaining = self.max_file_bytes.saturating_sub(self.written) as usize;
        let to_write = requested.min(remaining);
        if to_write == 0 {
            self.truncated = true;
            return (0, true);
        }

        let write_result = if to_write <= line_bytes.len() {
            file.write_all(&line_bytes[..to_write])
        } else {
            file.write_all(line_bytes)
                .and_then(|_| file.write_all(b"\n"))
        };
        match write_result {
            Ok(()) => {
                self.written = self.written.saturating_add(to_write as u64);
                if to_write < requested {
                    self.truncated = true;
                }
                (to_write as u64, self.truncated)
            }
            Err(error) => {
                tracing::warn!(error = %error, "failed to write runtime diagnostic log line");
                self.file = None;
                self.truncated = true;
                (0, true)
            }
        }
    }
}

fn open_runtime_log_file(
    events: &RuntimeEventSink,
    task_id: Uuid,
    attempt_no: i32,
    work_dir: &Path,
    stream: &str,
    max_file_bytes: u64,
) -> RuntimeLogFile {
    let log_dir = work_dir.join("logs");
    if let Err(error) = fs::create_dir_all(&log_dir) {
        tracing::warn!(
            path = %log_dir.display(),
            error = %error,
            "failed to create runtime diagnostic log directory"
        );
    }
    let path = log_dir.join(format!("{stream}.log"));
    events.register_diagnostic_log(task_id, attempt_no, stream, path.clone());
    RuntimeLogFile::open(&path, max_file_bytes)
}

pub(crate) async fn read_progress_stream(
    stdout: tokio::process::ChildStdout,
    runtime_id: Uuid,
    task_id: Uuid,
    attempt_no: i32,
    lease_token: String,
    require_stream_online: bool,
    work_dir: PathBuf,
    max_file_bytes: u64,
    tail_bytes: usize,
    events: RuntimeEventSink,
    monitor_handle: RuntimeMonitorHandle,
) {
    let mut reader = BufReader::new(stdout).lines();
    let mut current = HashMap::<String, String>::new();
    let mut log_file = open_runtime_log_file(
        &events,
        task_id,
        attempt_no,
        &work_dir,
        "stdout",
        max_file_bytes,
    );

    while let Ok(Some(line)) = reader.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        let (bytes_written, file_truncated) = log_file.append_line(&line);
        events.observe_diagnostic_log_line(
            task_id,
            attempt_no,
            "stdout",
            &line,
            bytes_written,
            tail_bytes,
            file_truncated,
        );
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        current.insert(key.to_string(), value.to_string());
        if key == "progress" {
            let Some(snapshot) = monitor_handle.snapshot().await else {
                return;
            };
            let stream_online = snapshot
                .handle
                .metadata
                .get("stream_online")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if require_stream_online && !stream_online {
                current.clear();
                continue;
            }
            let session_epoch = runtime_session_epoch(&snapshot.handle);
            let progress = RuntimeTaskProgress {
                task_id,
                attempt_no,
                lease_token: lease_token.clone(),
                session_epoch,
                frame: parse_u64(current.get("frame")),
                fps: parse_f64(current.get("fps")),
                bitrate_kbps: parse_bitrate_kbps(current.get("bitrate")),
                speed: parse_speed(current.get("speed")),
                out_time_ms: parse_u64(current.get("out_time_ms")),
                dup_frames: parse_u64(current.get("dup_frames")),
                drop_frames: parse_u64(current.get("drop_frames")),
            };
            monitor_handle
                .send_event(RuntimeInternalEvent::ProgressObserved(
                    ProgressObservedEvent {
                        runtime_id,
                        generation: monitor_handle.generation(),
                        progress,
                    },
                ))
                .await;
            current.clear();
        }
    }
}

fn flush_log_batch(
    task_id: Uuid,
    attempt_no: i32,
    lease_token: &str,
    session_epoch: u64,
    stream: &str,
    batch: &mut Vec<(String, usize)>,
    source_line_count: &mut usize,
    events: &RuntimeEventSink,
) {
    if batch.is_empty() {
        return;
    }

    let lines = batch
        .drain(..)
        .map(|(line, count)| match count {
            0 | 1 => line,
            count => format!("{line} (repeated {count} times)"),
        })
        .collect::<Vec<_>>();
    let emitted_line_count = *source_line_count;
    *source_line_count = 0;

    let _ = events.send(RuntimeNotification::TaskLogBatch(RuntimeTaskLogBatch {
        task_id,
        attempt_no,
        lease_token: lease_token.to_string(),
        session_epoch,
        stream: stream.to_string(),
        lines,
        source_line_count: emitted_line_count,
    }));
}

pub(crate) async fn read_log_stream(
    stderr: tokio::process::ChildStderr,
    task_id: Uuid,
    attempt_no: i32,
    lease_token: String,
    session_epoch: u64,
    stream: String,
    work_dir: PathBuf,
    max_file_bytes: u64,
    tail_bytes: usize,
    events: RuntimeEventSink,
) {
    let mut reader = BufReader::new(stderr).lines();
    let mut batch = Vec::new();
    let mut source_line_count = 0usize;
    let mut log_file = open_runtime_log_file(
        &events,
        task_id,
        attempt_no,
        &work_dir,
        &stream,
        max_file_bytes,
    );

    'outer: loop {
        let next_line = if batch.is_empty() {
            reader.next_line().await
        } else {
            match timeout(LOG_BATCH_FLUSH_INTERVAL, reader.next_line()).await {
                Ok(result) => result,
                Err(_) => {
                    flush_log_batch(
                        task_id,
                        attempt_no,
                        &lease_token,
                        session_epoch,
                        &stream,
                        &mut batch,
                        &mut source_line_count,
                        &events,
                    );
                    continue;
                }
            }
        };

        let Ok(line) = next_line else {
            break;
        };
        let Some(line) = line else {
            break;
        };
        let line = line.trim_end().to_string();
        if line.is_empty() {
            continue;
        }

        let (bytes_written, file_truncated) = log_file.append_line(&line);
        events.observe_diagnostic_log_line(
            task_id,
            attempt_no,
            &stream,
            &line,
            bytes_written,
            tail_bytes,
            file_truncated,
        );
        source_line_count += 1;
        if let Some((last_line, count)) = batch.last_mut() {
            if *last_line == line {
                *count += 1;
            } else {
                batch.push((line, 1));
            }
        } else {
            batch.push((line, 1));
        }

        if batch.len() >= MAX_LOG_BATCH_LINES || source_line_count >= MAX_LOG_BATCH_LINES {
            flush_log_batch(
                task_id,
                attempt_no,
                &lease_token,
                session_epoch,
                &stream,
                &mut batch,
                &mut source_line_count,
                &events,
            );
            continue 'outer;
        }
    }

    flush_log_batch(
        task_id,
        attempt_no,
        &lease_token,
        session_epoch,
        &stream,
        &mut batch,
        &mut source_line_count,
        &events,
    );
}

pub(crate) fn runtime_session_epoch(handle: &RuntimeHandle) -> u64 {
    handle
        .metadata
        .get("session_epoch")
        .and_then(Value::as_u64)
        .unwrap_or_default()
}

pub(crate) fn cleanup_expired_runtime_logs(work_root: &str, retention_days: u64) -> usize {
    let retention = Duration::from_secs(retention_days.saturating_mul(SECONDS_PER_DAY));
    let Some(cutoff) = SystemTime::now().checked_sub(retention) else {
        return 0;
    };
    let mut removed = 0usize;
    let Ok(task_dirs) = fs::read_dir(work_root) else {
        return 0;
    };

    for task_dir in task_dirs.flatten() {
        let Ok(attempt_dirs) = fs::read_dir(task_dir.path()) else {
            continue;
        };
        for attempt_dir in attempt_dirs.flatten() {
            let logs_dir = attempt_dir.path().join("logs");
            let Ok(files) = fs::read_dir(&logs_dir) else {
                continue;
            };
            for file in files.flatten() {
                let path = file.path();
                if !path.is_file() {
                    continue;
                }
                let Ok(metadata) = file.metadata() else {
                    continue;
                };
                let Ok(modified) = metadata.modified() else {
                    continue;
                };
                if modified <= cutoff && fs::remove_file(&path).is_ok() {
                    removed += 1;
                }
            }
            let _ = fs::remove_dir(&logs_dir);
        }
    }

    removed
}

fn parse_u64(value: Option<&String>) -> u64 {
    value
        .and_then(|value| value.parse().ok())
        .unwrap_or_default()
}

fn parse_f64(value: Option<&String>) -> f64 {
    value
        .and_then(|value| value.parse().ok())
        .unwrap_or_default()
}

fn parse_speed(value: Option<&String>) -> f64 {
    value
        .map(|value| value.trim_end_matches('x'))
        .and_then(|value| value.parse().ok())
        .unwrap_or_default()
}

fn parse_bitrate_kbps(value: Option<&String>) -> f64 {
    let Some(value) = value else {
        return 0.0;
    };
    let value = value.trim();
    if let Some(value) = value.strip_suffix("kbits/s") {
        return value.trim().parse().unwrap_or_default();
    }
    if let Some(value) = value.strip_suffix("bits/s") {
        let bits: f64 = value.trim().parse().unwrap_or_default();
        return bits / 1000.0;
    }
    value.parse().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_sink() -> RuntimeEventSink {
        let (priority_tx, _priority_rx) = mpsc::unbounded_channel();
        let (log_tx, _log_rx) = mpsc::channel(1);
        RuntimeEventSink::new(priority_tx, log_tx)
    }

    #[test]
    fn diagnostic_log_tail_is_bounded() {
        let sink = test_sink();
        let task_id = Uuid::now_v7();
        sink.register_diagnostic_log(
            task_id,
            1,
            "stderr",
            PathBuf::from("/tmp/streamserver-test-stderr.log"),
        );

        sink.observe_diagnostic_log_line(task_id, 1, "stderr", "abcdef", 7, 4, false);
        let logs = sink.diagnostic_logs(task_id, 1);

        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].tail.as_bytes().len(), 4);
        assert!(logs[0].tail_truncated);
        assert!(!logs[0].file_truncated);
        assert_eq!(logs[0].size_bytes, 7);
        assert_eq!(logs[0].line_count, 1);
    }

    #[test]
    fn runtime_log_file_marks_file_truncated_after_max_bytes() {
        let path =
            std::env::temp_dir().join(format!("streamserver-runtime-log-{}.log", Uuid::now_v7()));
        let mut file = RuntimeLogFile::open(&path, 5);

        let (bytes_written, file_truncated) = file.append_line("abcdef");

        assert_eq!(bytes_written, 5);
        assert!(file_truncated);
        assert_eq!(
            std::fs::metadata(&path)
                .expect("log file should exist")
                .len(),
            5
        );

        let _ = std::fs::remove_file(path);
    }
}
