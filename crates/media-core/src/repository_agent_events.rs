//! Agent 事件仓储：处理 Agent 上报的任务日志、进度、快照和终态事件，并把事件转换为任务状态变更。

use chrono::Utc;
use media_domain::{AttemptStatus, EventSource, TaskSpec, TaskStatus};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Postgres;
use uuid::Uuid;

use super::{
    DEFAULT_MAX_CONSECUTIVE_FAILURES, OwnershipMode, RepoError, TaskRepository,
    relative_http_url_from_path, retry_enabled_on_disconnect, start_rejected_retry_limit,
    sticky_reconnect_active,
};

impl TaskRepository {
    pub async fn record_agent_task_event(
        &self,
        node_id: Uuid,
        event: AgentTaskEventRecord,
    ) -> Result<(), RepoError> {
        let mut tx = self.pool.begin().await?;
        let now = Utc::now();
        let ownership_mode = if matches!(event.event_type.as_str(), "adopted" | "orphaned") {
            OwnershipMode::AuthorizedAttempt
        } else {
            OwnershipMode::CurrentOwner
        };
        let Some(ownership) = self
            .validate_attempt_ownership(
                &mut tx,
                event.task_id,
                event.attempt_no,
                node_id,
                &event.lease_token,
                "task_event",
                ownership_mode,
            )
            .await?
        else {
            tx.commit().await?;
            return Ok(());
        };
        let sticky_reconnect_active = sticky_reconnect_active(&ownership)?;

        let event_level = normalize_event_level(&event.event_level);
        if should_persist_agent_task_event(&event.event_type, &event_level) {
            self.insert_event(
                &mut tx,
                event.task_id,
                ownership.attempt_id,
                Some(event.attempt_no),
                EventSource::Agent,
                &event.event_type,
                &event_level,
                json!({
                    "node_id": node_id,
                    "lease_token": event.lease_token,
                    "message": event.message,
                    "payload": event.payload,
                }),
            )
            .await?;
        }

        let disk_threshold_failure = event.event_type == "failed"
            && event
                .payload
                .get("reason")
                .and_then(Value::as_str)
                .is_some_and(|reason| reason == "disk_threshold_exceeded");

        let mut retry_after_orphaned = false;
        match event.event_type.as_str() {
            "accepted" | "starting"
                if sticky_reconnect_active && ownership.task_status == TaskStatus::Running => {}
            "accepted" | "starting" => {
                sqlx::query(
                    r#"
                    update tasks
                       set status = 'STARTING'::task_status,
                           assigned_node_id = $1,
                           reclaim_deadline_at = null,
                           updated_at = $2
                     where id = $3
                    "#,
                )
                .bind(node_id)
                .bind(now)
                .bind(event.task_id)
                .execute(&mut *tx)
                .await?;

                sqlx::query(
                    r#"
                    update task_attempts
                       set status = 'STARTING'::attempt_status,
                           node_id = $1,
                           started_at = coalesce(started_at, $2)
                     where task_id = $3
                       and attempt_no = $4
                    "#,
                )
                .bind(node_id)
                .bind(now)
                .bind(event.task_id)
                .bind(event.attempt_no)
                .execute(&mut *tx)
                .await?;
            }
            "recovering" if sticky_reconnect_active => {}
            "recovering" => {
                sqlx::query(
                    r#"
                    update tasks
                       set status = 'RECOVERING'::task_status,
                           assigned_node_id = $1,
                           reclaim_deadline_at = null,
                           updated_at = $2
                     where id = $3
                    "#,
                )
                .bind(node_id)
                .bind(now)
                .bind(event.task_id)
                .execute(&mut *tx)
                .await?;

                sqlx::query(
                    r#"
                    update task_attempts
                       set status = 'STARTING'::attempt_status,
                           node_id = $1,
                           started_at = coalesce(started_at, $2)
                     where task_id = $3
                       and attempt_no = $4
                    "#,
                )
                .bind(node_id)
                .bind(now)
                .bind(event.task_id)
                .bind(event.attempt_no)
                .execute(&mut *tx)
                .await?;
            }
            "adopted" => {
                sqlx::query(
                    r#"
                    update tasks
                       set status = 'RECOVERING'::task_status,
                           assigned_node_id = $1,
                           reclaim_deadline_at = null,
                           updated_at = $2
                     where id = $3
                       and current_attempt_no = $4
                    "#,
                )
                .bind(node_id)
                .bind(now)
                .bind(event.task_id)
                .bind(event.attempt_no)
                .execute(&mut *tx)
                .await?;

                sqlx::query(
                    r#"
                    update task_attempts
                       set status = 'ADOPTED'::attempt_status,
                           node_id = $1,
                           started_at = coalesce(started_at, $2)
                     where task_id = $3
                       and attempt_no = $4
                    "#,
                )
                .bind(node_id)
                .bind(now)
                .bind(event.task_id)
                .bind(event.attempt_no)
                .execute(&mut *tx)
                .await?;
            }
            "orphaned" => {
                if ownership.stop_requested_at.is_some() {
                    sqlx::query(
                        r#"
                        update tasks
                           set status = 'STOPPING'::task_status,
                               assigned_node_id = null,
                               reclaim_deadline_at = null,
                               updated_at = $1
                         where id = $2
                           and current_attempt_no = $3
                           and status in (
                             'STOPPING'::task_status,
                             'LOST'::task_status,
                             'RECLAIMING'::task_status
                           )
                        "#,
                    )
                    .bind(now)
                    .bind(event.task_id)
                    .bind(event.attempt_no)
                    .execute(&mut *tx)
                    .await?;
                    sqlx::query(
                        r#"
                        update task_attempts
                           set status = 'ORPHANED'::attempt_status,
                               node_id = $1
                         where task_id = $2
                           and attempt_no = $3
                        "#,
                    )
                    .bind(node_id)
                    .bind(event.task_id)
                    .bind(event.attempt_no)
                    .execute(&mut *tx)
                    .await?;
                } else if matches!(
                    ownership.task_status,
                    TaskStatus::Dispatching
                        | TaskStatus::Starting
                        | TaskStatus::Running
                        | TaskStatus::Recovering
                        | TaskStatus::Reclaiming
                ) {
                    let resolved_spec = ownership
                        .resolved_spec
                        .clone()
                        .map(serde_json::from_value::<TaskSpec>)
                        .transpose()?;
                    retry_after_orphaned = resolved_spec
                        .as_ref()
                        .is_some_and(retry_enabled_on_disconnect);
                    self.mark_task_lost(
                        &mut tx,
                        event.task_id,
                        event.attempt_no,
                        node_id,
                        "runtime_not_found",
                        "reclaimed runtime was reported missing by the agent",
                        now,
                    )
                    .await?;
                    self.insert_event(
                        &mut tx,
                        event.task_id,
                        ownership.attempt_id,
                        Some(event.attempt_no),
                        EventSource::Core,
                        "task_lost_after_reclaim_orphaned",
                        "warn",
                        json!({
                            "node_id": node_id,
                            "attempt_no": event.attempt_no,
                            "reason": "runtime_not_found",
                            "auto_retry": retry_after_orphaned,
                        }),
                    )
                    .await?;
                } else {
                    sqlx::query(
                        r#"
                        update task_attempts
                           set status = 'ORPHANED'::attempt_status,
                               node_id = $1
                         where task_id = $2
                           and attempt_no = $3
                        "#,
                    )
                    .bind(node_id)
                    .bind(event.task_id)
                    .bind(event.attempt_no)
                    .execute(&mut *tx)
                    .await?;
                }
            }
            "running" => {
                self.promote_task_running(&mut tx, event.task_id, event.attempt_no, node_id, now)
                    .await?;
            }
            "stopping" => {
                sqlx::query(
                    r#"
                    update tasks
                       set status = 'STOPPING'::task_status,
                           reclaim_deadline_at = null,
                           updated_at = $1
                     where id = $2
                    "#,
                )
                .bind(now)
                .bind(event.task_id)
                .execute(&mut *tx)
                .await?;

                sqlx::query(
                    r#"
                    update task_attempts
                       set status = 'STOPPING'::attempt_status
                     where task_id = $1
                       and attempt_no = $2
                    "#,
                )
                .bind(event.task_id)
                .bind(event.attempt_no)
                .execute(&mut *tx)
                .await?;
            }
            "rejected" | "start_rejected" => {
                if ownership.task_status == TaskStatus::Stopping {
                    self.complete_task_attempt(
                        &mut tx,
                        event.task_id,
                        event.attempt_no,
                        node_id,
                        TaskStatus::Canceled,
                        AttemptStatus::Failed,
                        Some("start_rejected_after_stop"),
                        Some(event.message.as_str()),
                        now,
                    )
                    .await?;
                } else {
                    let consecutive_failures = self
                        .consecutive_failed_attempts_before(
                            &mut tx,
                            event.task_id,
                            event.attempt_no - 1,
                        )
                        .await?
                        + 1;
                    let resolved_spec = ownership
                        .resolved_spec
                        .clone()
                        .map(serde_json::from_value::<TaskSpec>)
                        .transpose()?;
                    let retry_limit = resolved_spec
                        .as_ref()
                        .and_then(start_rejected_retry_limit)
                        .unwrap_or(DEFAULT_MAX_CONSECUTIVE_FAILURES);
                    let should_retry = resolved_spec
                        .as_ref()
                        .is_none_or(retry_enabled_on_disconnect)
                        && retry_limit > 0
                        && consecutive_failures < retry_limit;

                    if should_retry {
                        sqlx::query(
                            r#"
                            update tasks
                               set status = 'QUEUED'::task_status,
                                   assigned_node_id = null,
                                   updated_at = $1
                             where id = $2
                               and current_attempt_no = $3
                            "#,
                        )
                        .bind(now)
                        .bind(event.task_id)
                        .bind(event.attempt_no)
                        .execute(&mut *tx)
                        .await?;

                        sqlx::query(
                            r#"
                            update task_attempts
                               set status = 'FAILED'::attempt_status,
                                   node_id = $1,
                                   failure_code = 'agent_start_rejected',
                                   failure_reason = $2,
                                   ended_at = $3
                             where task_id = $4
                               and attempt_no = $5
                            "#,
                        )
                        .bind(node_id)
                        .bind(&event.message)
                        .bind(now)
                        .bind(event.task_id)
                        .bind(event.attempt_no)
                        .execute(&mut *tx)
                        .await?;

                        self.delete_task_lease(&mut tx, event.task_id).await?;
                    } else {
                        let failure_reason = if retry_limit == 0 {
                            format!("{} (automatic retry disabled for this task)", event.message)
                        } else {
                            format!(
                                "{} (consecutive start_rejected failures reached {}/{})",
                                event.message, consecutive_failures, retry_limit
                            )
                        };
                        self.delete_stream_bindings_for_task(&mut tx, event.task_id)
                            .await?;
                        self.complete_task_attempt(
                            &mut tx,
                            event.task_id,
                            event.attempt_no,
                            node_id,
                            TaskStatus::Failed,
                            AttemptStatus::Failed,
                            Some("agent_start_rejected"),
                            Some(&failure_reason),
                            now,
                        )
                        .await?;
                    }
                }
            }
            "stop_rejected" => {
                sqlx::query(
                    r#"
                    update tasks
                       set status = 'STOPPING'::task_status,
                           reclaim_deadline_at = null,
                           updated_at = $1
                     where id = $2
                       and current_attempt_no = $3
                    "#,
                )
                .bind(now)
                .bind(event.task_id)
                .bind(event.attempt_no)
                .execute(&mut *tx)
                .await?;
            }
            "succeeded" if sticky_reconnect_active => {}
            "succeeded" => {
                self.complete_task_attempt(
                    &mut tx,
                    event.task_id,
                    event.attempt_no,
                    node_id,
                    TaskStatus::Succeeded,
                    AttemptStatus::Succeeded,
                    None,
                    None,
                    now,
                )
                .await?;
            }
            "failed" if sticky_reconnect_active && !disk_threshold_failure => {}
            "failed" => {
                let (failure_code, failure_reason) = if disk_threshold_failure {
                    ("disk_threshold_exceeded", "disk_threshold_exceeded")
                } else {
                    ("agent_failed", event.message.as_str())
                };
                self.complete_task_attempt(
                    &mut tx,
                    event.task_id,
                    event.attempt_no,
                    node_id,
                    TaskStatus::Failed,
                    AttemptStatus::Failed,
                    Some(failure_code),
                    Some(failure_reason),
                    now,
                )
                .await?;
            }
            "canceled" => {
                self.complete_task_attempt(
                    &mut tx,
                    event.task_id,
                    event.attempt_no,
                    node_id,
                    TaskStatus::Canceled,
                    AttemptStatus::Failed,
                    Some("canceled"),
                    Some(event.message.as_str()),
                    now,
                )
                .await?;
            }
            _ => {}
        }

        tx.commit().await?;
        if retry_after_orphaned {
            let current = self.fetch_task_summary(event.task_id).await?;
            if current.status == TaskStatus::Lost {
                self.enqueue_retry(
                    current,
                    EventSource::Core,
                    "task_retry_after_reclaim_orphaned",
                    json!({
                        "reason": "runtime_not_found",
                        "auto_retry": true,
                    }),
                )
                .await?;
            }
        }
        Ok(())
    }

    pub async fn record_agent_log_batch(
        &self,
        node_id: Uuid,
        batch: TaskLogBatchRecord,
    ) -> Result<(), RepoError> {
        let mut tx = self.pool.begin().await?;
        let Some(_ownership) = self
            .validate_attempt_ownership(
                &mut tx,
                batch.task_id,
                batch.attempt_no,
                node_id,
                &batch.lease_token,
                "task_log_batch",
                OwnershipMode::CurrentOwner,
            )
            .await?
        else {
            tx.commit().await?;
            return Ok(());
        };
        tx.commit().await?;
        Ok(())
    }

    pub async fn record_agent_progress(
        &self,
        node_id: Uuid,
        progress: TaskProgressRecord,
    ) -> Result<(), RepoError> {
        let mut tx = self.pool.begin().await?;
        let now = Utc::now();
        let Some(_ownership) = self
            .validate_attempt_ownership(
                &mut tx,
                progress.task_id,
                progress.attempt_no,
                node_id,
                &progress.lease_token,
                "task_progress",
                OwnershipMode::CurrentOwner,
            )
            .await?
        else {
            tx.commit().await?;
            return Ok(());
        };
        let payload = json!({
            "node_id": node_id,
            "lease_token": progress.lease_token,
            "frame": progress.frame,
            "fps": progress.fps,
            "bitrate_kbps": progress.bitrate_kbps,
            "speed": progress.speed,
            "out_time_ms": progress.out_time_ms,
            "dup_frames": progress.dup_frames,
            "drop_frames": progress.drop_frames,
        });

        self.promote_task_running(&mut tx, progress.task_id, progress.attempt_no, node_id, now)
            .await?;

        sqlx::query(
            r#"
            update task_attempts
               set checkpoint_json = $1
             where task_id = $2
               and attempt_no = $3
            "#,
        )
        .bind(payload)
        .bind(progress.task_id)
        .bind(progress.attempt_no)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    pub async fn record_agent_snapshot(
        &self,
        node_id: Uuid,
        snapshot: TaskSnapshotRecord,
    ) -> Result<(), RepoError> {
        let mut tx = self.pool.begin().await?;
        let now = Utc::now();
        let Some(ownership) = self
            .validate_attempt_ownership(
                &mut tx,
                snapshot.task_id,
                snapshot.attempt_no,
                node_id,
                &snapshot.lease_token,
                "task_snapshot",
                OwnershipMode::AuthorizedAttempt,
            )
            .await?
        else {
            tx.commit().await?;
            return Ok(());
        };
        if snapshot.state.eq_ignore_ascii_case("exited") {
            self.insert_event(
                &mut tx,
                snapshot.task_id,
                ownership.attempt_id,
                Some(snapshot.attempt_no),
                EventSource::Agent,
                "task_snapshot",
                "info",
                json!({
                    "node_id": node_id,
                    "lease_token": snapshot.lease_token,
                    "runtime_id": snapshot.runtime_id,
                    "worker_kind": snapshot.worker_kind,
                    "pid": snapshot.pid,
                    "state": snapshot.state,
                    "command_line": snapshot.command_line,
                    "outputs": snapshot.outputs,
                    "metadata": snapshot.metadata,
                }),
            )
            .await?;
        }

        sqlx::query(
            r#"
            update task_attempts
               set node_id = $1,
                   pid = $2
             where task_id = $3
               and attempt_no = $4
            "#,
        )
        .bind(node_id)
        .bind(snapshot.pid)
        .bind(snapshot.task_id)
        .bind(snapshot.attempt_no)
        .execute(&mut *tx)
        .await?;

        self.upsert_stream_binding_from_snapshot(&mut tx, node_id, &snapshot)
            .await?;
        self.upsert_file_artifacts_from_snapshot(&mut tx, node_id, &snapshot)
            .await?;

        if snapshot.state.eq_ignore_ascii_case("exited") {
            self.reconcile_exited_snapshot(
                &mut tx,
                &ownership,
                snapshot.task_id,
                snapshot.attempt_no,
                node_id,
                &snapshot.metadata,
                now,
            )
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    async fn upsert_stream_binding_from_snapshot(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        node_id: Uuid,
        snapshot: &TaskSnapshotRecord,
    ) -> Result<(), RepoError> {
        let Some(binding) = snapshot
            .metadata
            .get("stream_binding")
            .cloned()
            .and_then(|value| serde_json::from_value::<StreamBindingSnapshot>(value).ok())
        else {
            return Ok(());
        };
        let Some(schema) = binding.schema else {
            return Ok(());
        };
        let attempt_id: Option<Uuid> = sqlx::query_scalar(
            r#"
            select id
              from task_attempts
             where task_id = $1
               and attempt_no = $2
            "#,
        )
        .bind(snapshot.task_id)
        .bind(snapshot.attempt_no)
        .fetch_optional(&mut **tx)
        .await?;
        let Some(attempt_id) = attempt_id else {
            return Ok(());
        };
        let zlm_proxy_key = snapshot
            .metadata
            .get("zlm_proxy_key")
            .and_then(Value::as_str)
            .map(str::to_string);
        let rtp_stream_id = snapshot
            .metadata
            .get("rtp_stream_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let server_id = snapshot
            .metadata
            .get("zlm_server_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let Some(server_id) = server_id else {
            self.insert_event(
                tx,
                snapshot.task_id,
                Some(attempt_id),
                Some(snapshot.attempt_no),
                EventSource::Core,
                "stream_binding_snapshot_missing_server_id",
                "warn",
                json!({
                    "node_id": node_id,
                    "attempt_no": snapshot.attempt_no,
                    "runtime_id": snapshot.runtime_id,
                }),
            )
            .await?;
            return Ok(());
        };

        sqlx::query(
            r#"
            insert into stream_bindings (
              id, task_id, attempt_id, server_id, node_id, schema, vhost, app, stream,
              zlm_proxy_key, zlm_pusher_key, rtp_stream_id
            )
            values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, null, $11)
            on conflict (server_id, schema, vhost, app, stream) do update
              set task_id = excluded.task_id,
                  attempt_id = excluded.attempt_id,
                  node_id = excluded.node_id,
                  zlm_proxy_key = excluded.zlm_proxy_key,
                  rtp_stream_id = coalesce(excluded.rtp_stream_id, stream_bindings.rtp_stream_id)
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(snapshot.task_id)
        .bind(attempt_id)
        .bind(server_id)
        .bind(node_id)
        .bind(schema)
        .bind(binding.vhost)
        .bind(binding.app)
        .bind(binding.stream)
        .bind(zlm_proxy_key)
        .bind(rtp_stream_id)
        .execute(&mut **tx)
        .await?;

        Ok(())
    }

    async fn upsert_file_artifacts_from_snapshot(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        node_id: Uuid,
        snapshot: &TaskSnapshotRecord,
    ) -> Result<(), RepoError> {
        let Some(attempt_id) = sqlx::query_scalar::<_, Uuid>(
            r#"
            select id
              from task_attempts
             where task_id = $1
               and attempt_no = $2
            "#,
        )
        .bind(snapshot.task_id)
        .bind(snapshot.attempt_no)
        .fetch_optional(&mut **tx)
        .await?
        else {
            return Ok(());
        };

        if let Some(metadata) = snapshot
            .metadata
            .get("transcode_artifact")
            .cloned()
            .and_then(|value| serde_json::from_value::<FileArtifactMetadata>(value).ok())
        {
            self.upsert_file_artifact_row(tx, snapshot.task_id, attempt_id, node_id, metadata)
                .await?;
            self.enqueue_artifact_update_callback_if_needed(
                tx,
                snapshot.task_id,
                snapshot.attempt_no,
            )
            .await?;
        }

        if let Some(metadata) = snapshot
            .metadata
            .get("bridge_artifact")
            .cloned()
            .and_then(|value| serde_json::from_value::<FileArtifactMetadata>(value).ok())
        {
            self.upsert_file_artifact_row(tx, snapshot.task_id, attempt_id, node_id, metadata)
                .await?;
            self.enqueue_artifact_update_callback_if_needed(
                tx,
                snapshot.task_id,
                snapshot.attempt_no,
            )
            .await?;
        }

        if let Some(records) = snapshot
            .metadata
            .get("stream_ingest_record_artifacts")
            .cloned()
            .and_then(|value| serde_json::from_value::<Vec<FileArtifactMetadata>>(value).ok())
        {
            for metadata in records {
                self.upsert_file_artifact_row(tx, snapshot.task_id, attempt_id, node_id, metadata)
                    .await?;
            }
            self.enqueue_artifact_update_callback_if_needed(
                tx,
                snapshot.task_id,
                snapshot.attempt_no,
            )
            .await?;
        }

        Ok(())
    }

    async fn upsert_file_artifact_row(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        task_id: Uuid,
        attempt_id: Uuid,
        node_id: Uuid,
        metadata: FileArtifactMetadata,
    ) -> Result<(), RepoError> {
        let http_url = relative_http_url_from_path(metadata.file_path.as_str())?;

        sqlx::query(
            r#"
            insert into transcode_artifacts (
              id, task_id, attempt_id, node_id, file_name, file_path, http_url, file_size, created_at
            ) values (
              $1, $2, $3, $4, $5, $6, $7, $8, $9
            )
            on conflict (file_path) do update
               set task_id = excluded.task_id,
                   attempt_id = excluded.attempt_id,
                   node_id = excluded.node_id,
                   file_name = excluded.file_name,
                   http_url = excluded.http_url,
                   file_size = excluded.file_size
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(task_id)
        .bind(attempt_id)
        .bind(node_id)
        .bind(&metadata.file_name)
        .bind(&metadata.file_path)
        .bind(&http_url)
        .bind(metadata.file_size)
        .bind(Utc::now())
        .execute(&mut **tx)
        .await?;

        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct AgentTaskEventRecord {
    pub task_id: Uuid,
    pub attempt_no: i32,
    pub lease_token: String,
    pub event_type: String,
    pub event_level: String,
    pub message: String,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub struct TaskLogBatchRecord {
    pub task_id: Uuid,
    pub attempt_no: i32,
    pub lease_token: String,
    pub stream: String,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct TaskProgressRecord {
    pub task_id: Uuid,
    pub attempt_no: i32,
    pub lease_token: String,
    pub frame: u64,
    pub fps: f64,
    pub bitrate_kbps: f64,
    pub speed: f64,
    pub out_time_ms: u64,
    pub dup_frames: u64,
    pub drop_frames: u64,
}

#[derive(Debug, Clone)]
pub struct TaskSnapshotRecord {
    pub runtime_id: Uuid,
    pub task_id: Uuid,
    pub attempt_no: i32,
    pub lease_token: String,
    pub worker_kind: String,
    pub pid: Option<i32>,
    pub state: String,
    pub command_line: Option<String>,
    pub outputs: Vec<String>,
    pub metadata: Value,
}

#[derive(Debug, Clone, Deserialize)]
struct StreamBindingSnapshot {
    #[serde(default)]
    schema: Option<String>,
    vhost: String,
    app: String,
    stream: String,
}

#[derive(Debug, Clone, Deserialize)]
struct FileArtifactMetadata {
    file_name: String,
    file_path: String,
    file_size: i64,
}

fn normalize_event_level(value: &str) -> String {
    match value.trim() {
        "debug" | "info" | "warn" | "error" => value.trim().to_string(),
        _ => "info".to_string(),
    }
}

pub(super) fn should_persist_agent_task_event(event_type: &str, event_level: &str) -> bool {
    if event_level == "error" {
        return true;
    }
    !matches!(
        event_type,
        "source_reconnecting" | "stream_cleanup" | "stream_lookup_miss"
    )
}
