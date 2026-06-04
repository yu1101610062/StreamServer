//! 运行时事件通道：负责 Agent 本地运行时的事件、日志批次、进度消息以及 stdout/stderr 读取上报。

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::Duration,
};

use media_domain::RuntimeHandle;
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RuntimeLogKey {
    task_id: Uuid,
    attempt_no: i32,
    stream: String,
}

impl RuntimeLogKey {
    fn from_batch(batch: &RuntimeTaskLogBatch) -> Self {
        Self {
            task_id: batch.task_id,
            attempt_no: batch.attempt_no,
            stream: batch.stream.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeEventSink {
    priority_tx: mpsc::UnboundedSender<RuntimeNotification>,
    log_tx: mpsc::Sender<RuntimeTaskLogBatch>,
    suppressed_logs: Arc<RwLock<HashMap<RuntimeLogKey, usize>>>,
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

pub(crate) async fn read_progress_stream(
    stdout: tokio::process::ChildStdout,
    runtime_id: Uuid,
    task_id: Uuid,
    attempt_no: i32,
    lease_token: String,
    require_stream_online: bool,
    monitor_handle: RuntimeMonitorHandle,
) {
    let mut reader = BufReader::new(stdout).lines();
    let mut current = HashMap::<String, String>::new();

    while let Ok(Some(line)) = reader.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
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
    events: RuntimeEventSink,
) {
    let mut reader = BufReader::new(stderr).lines();
    let mut batch = Vec::new();
    let mut source_line_count = 0usize;

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
