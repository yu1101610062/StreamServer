use std::{collections::HashMap, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use reqwest::{
    Client, StatusCode,
    header::{CONTENT_TYPE, HeaderMap, HeaderValue},
};
use serde::Serialize;
use sha2::Sha256;
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval},
};
use tracing::warn;
use uuid::Uuid;

use media_domain::{AttemptStatus, TaskStatus, TaskType, WorkerKind};

use crate::repository::{
    AttemptSummary, CallbackOutboxJob, NodeSummary, RecordFileSummary, StreamListFilter,
    StreamSummary, TaskDetail, TaskEventSummary, TaskRepository, TranscodeArtifactSummary,
};

const CALLBACK_TICK: Duration = Duration::from_secs(2);
const CALLBACK_BATCH_LIMIT: u32 = 16;
const RESPONSE_BODY_LIMIT: usize = 4096;

#[derive(Debug, Clone)]
pub struct CallbackConfig {
    pub timeout: Duration,
    pub max_attempts: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    pub shared_secret: Option<String>,
}

pub fn spawn(
    repository: Arc<TaskRepository>,
    client: Client,
    config: CallbackConfig,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = interval(CALLBACK_TICK);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if let Err(error) = run_once(&repository, &client, &config).await {
                        warn!(error = %error, "callback dispatcher tick failed");
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    })
}

async fn run_once(
    repository: &TaskRepository,
    client: &Client,
    config: &CallbackConfig,
) -> anyhow::Result<()> {
    let jobs = repository
        .list_due_callback_jobs(Utc::now(), CALLBACK_BATCH_LIMIT)
        .await?;
    for job in jobs {
        if let Err(error) = deliver_job(repository, client, config, job).await {
            warn!(error = %error, "callback delivery failed");
        }
    }
    Ok(())
}

async fn deliver_job(
    repository: &TaskRepository,
    client: &Client,
    config: &CallbackConfig,
    job: CallbackOutboxJob,
) -> anyhow::Result<()> {
    let detail = repository.get_task(job.task_id).await?;
    if !is_terminal(detail.task.status) {
        repository
            .mark_callback_dead(
                &job,
                None,
                None,
                "task is no longer in a terminal state",
                Utc::now(),
            )
            .await?;
        return Ok(());
    }

    let attempt = repository
        .get_task_attempt(job.task_id, job.attempt_no)
        .await?
        .unwrap_or_else(|| synthetic_attempt(&detail, &job));

    let nodes = repository.list_nodes().await?;
    let node_lookup = nodes
        .into_iter()
        .map(|node| (node.id, node))
        .collect::<HashMap<_, _>>();
    let streams = repository
        .list_streams(StreamListFilter {
            schema: None,
            app: None,
            stream: None,
            task_id: Some(job.task_id),
            node_id: None,
            has_viewer: None,
        })
        .await?;
    let records = repository.list_task_record_files(job.task_id).await?;
    let transcode_artifacts = repository
        .list_task_transcode_artifacts(job.task_id)
        .await?;

    let payload = TaskCompletedCallbackPayload {
        event_id: job.id,
        event_type: "task.completed".to_string(),
        reason: job.reason.clone(),
        event_time: Utc::now(),
        task: TaskCallbackTask::from_detail(&detail),
        attempt: TaskCallbackAttempt::from_summary(&attempt),
        streams: streams
            .into_iter()
            .map(|stream| TaskCallbackStream::from_summary(stream, &node_lookup))
            .collect(),
        records: records
            .into_iter()
            .map(TaskCallbackRecord::from_summary)
            .collect(),
        transcode_artifacts: transcode_artifacts
            .into_iter()
            .map(TaskCallbackTranscodeArtifact::from_summary)
            .collect(),
        latest_event: select_latest_business_event(&detail.recent_events)
            .map(TaskCallbackLatestEvent::from_summary),
    };
    let body = serde_json::to_vec(&payload)?;
    let headers = build_headers(&job, &body, config.shared_secret.as_deref())?;

    let response = client
        .post(job.callback_url.as_str())
        .headers(headers)
        .timeout(config.timeout)
        .body(body)
        .send()
        .await;
    let now = Utc::now();

    match response {
        Ok(response) => {
            let status = response.status();
            let response_text = response.text().await.unwrap_or_default();
            let response_body = truncate_text(&response_text);
            if status.is_success() {
                repository
                    .mark_callback_delivered(&job, status.as_u16() as i32, response_body, now)
                    .await?;
                return Ok(());
            }

            let should_retry = status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
            if should_retry && job.delivery_attempts + 1 < config.max_attempts {
                let retry_at = now
                    + chrono::Duration::from_std(backoff_for_attempt(
                        config.initial_backoff,
                        config.max_backoff,
                        job.delivery_attempts,
                    ))
                    .unwrap_or_else(|_| chrono::Duration::milliseconds(0));
                repository
                    .schedule_callback_retry(
                        &job,
                        Some(status.as_u16() as i32),
                        response_body,
                        format!("callback endpoint returned HTTP {status}"),
                        retry_at,
                        now,
                    )
                    .await?;
            } else {
                repository
                    .mark_callback_dead(
                        &job,
                        Some(status.as_u16() as i32),
                        response_body,
                        format!("callback endpoint returned HTTP {status}"),
                        now,
                    )
                    .await?;
            }
        }
        Err(error) => {
            if job.delivery_attempts + 1 < config.max_attempts {
                let retry_at = now
                    + chrono::Duration::from_std(backoff_for_attempt(
                        config.initial_backoff,
                        config.max_backoff,
                        job.delivery_attempts,
                    ))
                    .unwrap_or_else(|_| chrono::Duration::milliseconds(0));
                repository
                    .schedule_callback_retry(
                        &job,
                        None,
                        None,
                        format!("callback request failed: {error}"),
                        retry_at,
                        now,
                    )
                    .await?;
            } else {
                repository
                    .mark_callback_dead(
                        &job,
                        None,
                        None,
                        format!("callback request failed: {error}"),
                        now,
                    )
                    .await?;
            }
        }
    }

    Ok(())
}

fn build_headers(
    job: &CallbackOutboxJob,
    body: &[u8],
    shared_secret: Option<&str>,
) -> anyhow::Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        "X-StreamServer-Event",
        HeaderValue::from_static("task.completed"),
    );
    headers.insert(
        "X-StreamServer-Event-Id",
        HeaderValue::from_str(job.id.to_string().as_str())?,
    );
    headers.insert(
        "X-StreamServer-Task-Id",
        HeaderValue::from_str(job.task_id.to_string().as_str())?,
    );
    headers.insert(
        "X-StreamServer-Attempt-No",
        HeaderValue::from_str(job.attempt_no.to_string().as_str())?,
    );

    if let Some(secret) = shared_secret.filter(|value| !value.trim().is_empty()) {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())?;
        mac.update(body);
        let signature = format!("sha256={}", hex_string(&mac.finalize().into_bytes()));
        headers.insert(
            "X-StreamServer-Signature",
            HeaderValue::from_str(signature.as_str())?,
        );
    }

    Ok(headers)
}

fn backoff_for_attempt(
    initial_backoff: Duration,
    max_backoff: Duration,
    delivery_attempts: u32,
) -> Duration {
    let exponent = delivery_attempts.min(20);
    let multiplier = 1u128 << exponent;
    let millis = (initial_backoff.as_millis())
        .saturating_mul(multiplier)
        .min(max_backoff.as_millis());
    Duration::from_millis(millis as u64)
}

fn truncate_text(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut truncated = trimmed
        .chars()
        .take(RESPONSE_BODY_LIMIT)
        .collect::<String>();
    if trimmed.chars().count() > RESPONSE_BODY_LIMIT {
        truncated.push_str("…");
    }
    Some(truncated)
}

fn hex_string(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 0x0F) as usize] as char);
    }
    result
}

fn is_terminal(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Canceled | TaskStatus::Lost
    )
}

fn select_latest_business_event(events: &[TaskEventSummary]) -> Option<TaskEventSummary> {
    events
        .iter()
        .find(|event| !event.event_type.starts_with("callback_"))
        .cloned()
        .or_else(|| events.first().cloned())
}

fn synthetic_attempt(detail: &TaskDetail, job: &CallbackOutboxJob) -> AttemptSummary {
    AttemptSummary {
        id: Uuid::nil(),
        attempt_no: job.attempt_no,
        worker_kind: detail.task.task_type.default_worker_kind(),
        status: AttemptStatus::Failed,
        node_id: detail.task.assigned_node_id,
        pid: None,
        exit_code: None,
        failure_code: (detail.task.status == TaskStatus::Canceled)
            .then_some("canceled".to_string()),
        failure_reason: (detail.task.status == TaskStatus::Canceled)
            .then_some("task canceled before an attempt snapshot was recorded".to_string()),
        started_at: detail.task.started_at,
        ended_at: detail.task.finished_at,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskCompletedCallbackPayload {
    pub event_id: Uuid,
    pub event_type: String,
    pub reason: String,
    pub event_time: DateTime<Utc>,
    pub task: TaskCallbackTask,
    pub attempt: TaskCallbackAttempt,
    pub streams: Vec<TaskCallbackStream>,
    pub records: Vec<TaskCallbackRecord>,
    pub transcode_artifacts: Vec<TaskCallbackTranscodeArtifact>,
    pub latest_event: Option<TaskCallbackLatestEvent>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskCallbackTask {
    pub id: Uuid,
    pub name: String,
    #[serde(rename = "type")]
    pub task_type: TaskType,
    pub status: TaskStatus,
    pub priority: u8,
    pub created_by: String,
    pub assigned_node_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl TaskCallbackTask {
    fn from_detail(detail: &TaskDetail) -> Self {
        Self {
            id: detail.task.id,
            name: detail.task.name.clone(),
            task_type: detail.task.task_type,
            status: detail.task.status,
            priority: detail.task.priority,
            created_by: detail.task.created_by.clone(),
            assigned_node_id: detail.task.assigned_node_id,
            created_at: detail.task.created_at,
            started_at: detail.task.started_at,
            finished_at: detail.task.finished_at,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskCallbackAttempt {
    pub id: Uuid,
    pub no: i32,
    pub status: AttemptStatus,
    pub node_id: Option<Uuid>,
    pub worker_kind: WorkerKind,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub failure_code: Option<String>,
    pub failure_reason: Option<String>,
}

impl TaskCallbackAttempt {
    fn from_summary(summary: &AttemptSummary) -> Self {
        Self {
            id: summary.id,
            no: summary.attempt_no,
            status: summary.status,
            node_id: summary.node_id,
            worker_kind: summary.worker_kind,
            started_at: summary.started_at,
            ended_at: summary.ended_at,
            failure_code: summary.failure_code.clone(),
            failure_reason: summary.failure_reason.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskCallbackStream {
    pub schema: String,
    pub vhost: String,
    pub app: String,
    pub stream: String,
    pub play_urls: Vec<String>,
    pub rtp_stream_id: Option<String>,
}

impl TaskCallbackStream {
    fn from_summary(summary: StreamSummary, node_lookup: &HashMap<Uuid, NodeSummary>) -> Self {
        let play_urls = summary
            .node_id
            .and_then(|node_id| node_lookup.get(&node_id))
            .map(|node| {
                if summary.play_urls.is_empty() {
                    crate::build_fallback_play_urls(
                        &node.agent_stream_addr,
                        &summary.schema,
                        &summary.app,
                        &summary.stream,
                    )
                } else {
                    summary.play_urls.clone()
                }
            })
            .unwrap_or_default();
        Self {
            schema: summary.schema,
            vhost: summary.vhost,
            app: summary.app,
            stream: summary.stream,
            play_urls,
            rtp_stream_id: summary.rtp_stream_id,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskCallbackRecord {
    pub id: Uuid,
    pub file_path: String,
    pub http_url: Option<String>,
    pub file_size: i64,
    pub time_len: Option<i32>,
    pub start_time: Option<DateTime<Utc>>,
    pub source: String,
}

impl TaskCallbackRecord {
    fn from_summary(summary: RecordFileSummary) -> Self {
        Self {
            id: summary.id,
            file_path: summary.file_path,
            http_url: summary.http_url,
            file_size: summary.file_size,
            time_len: summary.time_len,
            start_time: summary.start_time,
            source: summary.source,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskCallbackTranscodeArtifact {
    pub id: Uuid,
    pub file_name: String,
    pub file_path: String,
    pub http_url: String,
    pub file_size: i64,
    pub created_at: DateTime<Utc>,
}

impl TaskCallbackTranscodeArtifact {
    fn from_summary(summary: TranscodeArtifactSummary) -> Self {
        Self {
            id: summary.id,
            file_name: summary.file_name,
            file_path: summary.file_path,
            http_url: summary.http_url,
            file_size: summary.file_size,
            created_at: summary.created_at,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskCallbackLatestEvent {
    pub event_type: String,
    pub event_level: String,
    pub message: String,
    pub created_at: DateTime<Utc>,
}

impl TaskCallbackLatestEvent {
    fn from_summary(summary: TaskEventSummary) -> Self {
        let message = summary
            .payload
            .get("message")
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .or_else(|| {
                summary
                    .payload
                    .get("failure_reason")
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_default();
        Self {
            event_type: summary.event_type,
            event_level: summary.event_level,
            message,
            created_at: summary.created_at,
        }
    }
}
