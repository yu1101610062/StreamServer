//! 任务生命周期仓储：处理启动、停止、取消、重试、克隆和重新入队等用户操作。

use chrono::Utc;
use media_domain::{AttemptStatus, EventSource, StartMode, TaskOperation, TaskSpec, TaskStatus};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;

use super::{AttemptSummary, RepoError, TaskRepository, TaskSummary, task_summary_transcode_mode};

impl TaskRepository {
    pub async fn transition_task(
        &self,
        task_id: Uuid,
        operation: TaskOperation,
    ) -> Result<TaskSummary, RepoError> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            r#"
            select
              id,
              name,
              type::text as task_type,
              status::text as status,
              priority,
              created_by,
              assigned_node_id,
              current_attempt_no,
              created_at,
              updated_at,
              started_at,
              finished_at
            from tasks
            where id = $1
            for update
            "#,
        )
        .bind(task_id)
        .fetch_optional(&mut *tx)
        .await?;
        let row = row.ok_or(RepoError::TaskNotFound(task_id))?;
        let current = TaskSummary::from_row(&row)?;
        let next_status = current.status.apply_operation(operation)?;
        let updated_at = Utc::now();
        let finished_at = match next_status {
            TaskStatus::Canceled | TaskStatus::Succeeded | TaskStatus::Failed => Some(updated_at),
            _ => None,
        };

        sqlx::query(
            r#"
            update tasks
               set status = $1::task_status,
                   updated_at = $2,
                   finished_at = $3
             where id = $4
               and status = $5::task_status
            "#,
        )
        .bind(next_status.as_str())
        .bind(updated_at)
        .bind(finished_at)
        .bind(task_id)
        .bind(current.status.as_str())
        .execute(&mut *tx)
        .await?;

        self.insert_event(
            &mut tx,
            task_id,
            None,
            current.current_attempt_no_value(),
            EventSource::User,
            operation_event_name(operation),
            "info",
            json!({
                "from": current.status,
                "to": next_status,
            }),
        )
        .await?;

        tx.commit().await?;

        Ok(TaskSummary {
            status: next_status,
            updated_at,
            finished_at,
            ..current
        })
    }

    pub async fn retry_task(&self, task_id: Uuid) -> Result<AttemptSummary, RepoError> {
        let current = self.fetch_task_summary(task_id).await?;
        self.enqueue_retry(
            current,
            EventSource::User,
            "task_retry_requested",
            json!({}),
        )
        .await
    }

    pub(super) async fn enqueue_retry(
        &self,
        current: TaskSummary,
        event_source: EventSource,
        event_type: &str,
        extra_payload: Value,
    ) -> Result<AttemptSummary, RepoError> {
        current.status.apply_operation(TaskOperation::Retry)?;

        let task_id = current.id;
        let attempt_no = current.current_attempt_no + 1;
        let worker_kind = current.task_type.default_worker_kind();
        let created_at = Utc::now();
        let attempt_id = Uuid::now_v7();
        let mut payload = json!({
            "from": current.status,
            "to": TaskStatus::Queued,
            "attempt_no": attempt_no,
        });
        if let (Some(target), Some(extra)) = (payload.as_object_mut(), extra_payload.as_object()) {
            for (key, value) in extra {
                target.insert(key.clone(), value.clone());
            }
        }

        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"
            insert into task_attempts (
              id, task_id, attempt_no, node_id, worker_kind, status,
              pid, zlm_key, zlm_schema, zlm_vhost, zlm_app, zlm_stream,
              rtp_port, exit_code, failure_code, failure_reason,
              checkpoint_json, started_at, ended_at, created_at
            ) values (
              $1, $2, $3, null, $4::worker_kind, $5::attempt_status,
              null, null, null, null, null, null,
              null, null, null, null,
              null, null, null, $6
            )
            "#,
        )
        .bind(attempt_id)
        .bind(task_id)
        .bind(attempt_no)
        .bind(worker_kind.as_str())
        .bind(AttemptStatus::Pending.as_str())
        .bind(created_at)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r#"
            update tasks
               set status = 'QUEUED'::task_status,
                   assigned_node_id = null,
                   current_attempt_no = $1,
                   updated_at = $2,
                   finished_at = null
             where id = $3
            "#,
        )
        .bind(attempt_no)
        .bind(created_at)
        .bind(task_id)
        .execute(&mut *tx)
        .await?;

        self.insert_event(
            &mut tx,
            task_id,
            Some(attempt_id),
            Some(attempt_no),
            event_source,
            event_type,
            "info",
            payload,
        )
        .await?;

        tx.commit().await?;

        Ok(AttemptSummary {
            id: attempt_id,
            attempt_no,
            worker_kind,
            status: AttemptStatus::Pending,
            node_id: None,
            pid: None,
            exit_code: None,
            failure_code: None,
            failure_reason: None,
            started_at: None,
            ended_at: None,
        })
    }

    pub async fn preferred_retry_node_after_disconnect(
        &self,
        task_id: Uuid,
    ) -> Result<Option<Uuid>, RepoError> {
        let row = sqlx::query(
            r#"
            select previous_attempt.node_id
              from tasks t
              join task_attempts current_attempt
                on current_attempt.task_id = t.id
               and current_attempt.attempt_no = t.current_attempt_no
              join task_attempts previous_attempt
                on previous_attempt.task_id = t.id
               and previous_attempt.attempt_no = t.current_attempt_no - 1
             where t.id = $1
               and t.status = 'QUEUED'::task_status
               and current_attempt.status = 'PENDING'::attempt_status
               and previous_attempt.failure_code = 'node_disconnected'
             limit 1
            "#,
        )
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.and_then(|row| row.try_get::<Option<Uuid>, _>("node_id").ok().flatten()))
    }

    pub async fn clone_task(
        &self,
        task_id: Uuid,
        overrides: Option<TaskCloneOverride>,
    ) -> Result<TaskSummary, RepoError> {
        let row = sqlx::query(
            r#"
            select
              id,
              name,
              type::text as task_type,
              status::text as status,
              priority,
              assigned_node_id,
              current_attempt_no,
              created_at,
              updated_at,
              started_at,
              finished_at,
              requested_spec,
              resolved_spec,
              created_by
            from tasks
            where id = $1
            "#,
        )
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(RepoError::TaskNotFound(task_id))?;

        let source = TaskSummary::from_row(&row)?;
        source.status.apply_operation(TaskOperation::Clone)?;

        let requested_spec_value: Value = row.try_get("requested_spec")?;
        let mut requested_spec: TaskSpec = serde_json::from_value(requested_spec_value)?;
        if let Some(overrides) = overrides {
            apply_clone_overrides(&mut requested_spec, overrides);
        }
        let resolved = self.resolve_requested_task(&requested_spec).await?;
        let requested_spec = resolved.requested_spec;
        let resolved_spec = resolved.resolved_spec;

        let created_by = resolved_spec.created_by().unwrap_or("system").to_string();
        let initial_status = requested_spec.initial_status();
        let new_id = Uuid::now_v7();
        let now = Utc::now();

        let summary = TaskSummary {
            id: new_id,
            name: resolved_spec.name.clone(),
            task_type: resolved_spec.task_type,
            status: initial_status,
            priority: resolved_spec.priority,
            created_by: created_by.clone(),
            assigned_node_id: None,
            current_attempt_no: 0,
            created_at: now,
            updated_at: now,
            started_at: None,
            finished_at: None,
            transcode_mode: task_summary_transcode_mode(&resolved_spec).map(str::to_string),
        };

        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"
            insert into tasks (
              id, name, type, status, idempotency_key,
              priority, requested_spec, resolved_spec, created_by, assigned_node_id,
              current_attempt_no, schedule_start_mode, created_at, updated_at, started_at, finished_at
            ) values (
              $1, $2, $3::task_type, $4::task_status, $5,
              $6, $7, $8, $9, null,
              0, $10, $11, $12, null, null
            )
            "#,
        )
        .bind(new_id)
        .bind(&summary.name)
        .bind(summary.task_type.as_str())
        .bind(summary.status.as_str())
        .bind(format!("clone-{new_id}"))
        .bind(i32::from(summary.priority))
        .bind(serde_json::to_value(&requested_spec)?)
        .bind(serde_json::to_value(&resolved_spec)?)
        .bind(&created_by)
        .bind(
            requested_spec
                .schedule
                .start_mode
                .unwrap_or(media_domain::StartMode::Immediate)
                .as_str(),
        )
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        self.insert_event(
            &mut tx,
            new_id,
            None,
            None,
            EventSource::User,
            "task_cloned",
            "info",
            json!({ "source_task_id": task_id }),
        )
        .await?;

        tx.commit().await?;
        Ok(summary)
    }

    pub async fn ensure_task_queued(&self, task_id: Uuid) -> Result<TaskSummary, RepoError> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            r#"
            select
              id,
              name,
              type::text as task_type,
              status::text as status,
              priority,
              created_by,
              assigned_node_id,
              current_attempt_no,
              created_at,
              updated_at,
              started_at,
              finished_at
            from tasks
            where id = $1
            for update
            "#,
        )
        .bind(task_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(RepoError::TaskNotFound(task_id))?;

        let current = TaskSummary::from_row(&row)?;
        let mut summary = current.clone();

        match current.status {
            TaskStatus::Validating => {
                let updated_at = Utc::now();
                sqlx::query(
                    r#"
                    update tasks
                       set status = 'QUEUED'::task_status,
                           updated_at = $1
                     where id = $2
                    "#,
                )
                .bind(updated_at)
                .bind(task_id)
                .execute(&mut *tx)
                .await?;

                self.insert_event(
                    &mut tx,
                    task_id,
                    None,
                    current.current_attempt_no_value(),
                    EventSource::Core,
                    "task_queued",
                    "info",
                    json!({
                        "from": current.status,
                        "to": TaskStatus::Queued,
                    }),
                )
                .await?;

                summary.status = TaskStatus::Queued;
                summary.updated_at = updated_at;
            }
            TaskStatus::Queued => {}
            other => return Err(RepoError::TaskNotDispatchable(other)),
        }

        tx.commit().await?;
        Ok(summary)
    }

    pub async fn fail_queued_task(
        &self,
        task_id: Uuid,
        failure_code: &str,
        failure_reason: &str,
    ) -> Result<TaskSummary, RepoError> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            r#"
            select
              id,
              name,
              type::text as task_type,
              status::text as status,
              priority,
              created_by,
              assigned_node_id,
              current_attempt_no,
              created_at,
              updated_at,
              started_at,
              finished_at
            from tasks
            where id = $1
            for update
            "#,
        )
        .bind(task_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(RepoError::TaskNotFound(task_id))?;

        let current = TaskSummary::from_row(&row)?;
        if current.status != TaskStatus::Queued {
            return Err(RepoError::TaskNotDispatchable(current.status));
        }

        current.status.ensure_transition(TaskStatus::Failed)?;

        let now = Utc::now();
        let attempt_no = if current.current_attempt_no > 0 {
            current.current_attempt_no
        } else {
            1
        };

        if current.current_attempt_no > 0 {
            let result = sqlx::query(
                r#"
                update task_attempts
                   set status = 'FAILED'::attempt_status,
                       node_id = null,
                       failure_code = $1,
                       failure_reason = $2,
                       ended_at = $3
                 where task_id = $4
                   and attempt_no = $5
                   and status = 'PENDING'::attempt_status
                "#,
            )
            .bind(failure_code)
            .bind(failure_reason)
            .bind(now)
            .bind(task_id)
            .bind(attempt_no)
            .execute(&mut *tx)
            .await?;
            if result.rows_affected() != 1 {
                return Err(RepoError::TaskAttemptInvariant {
                    task_id,
                    attempt_no,
                    detail: "queued task is missing current pending attempt".to_string(),
                });
            }
        } else {
            sqlx::query(
                r#"
                insert into task_attempts (
                  id, task_id, attempt_no, node_id, worker_kind, status,
                  pid, zlm_key, zlm_schema, zlm_vhost, zlm_app, zlm_stream,
                  rtp_port, exit_code, failure_code, failure_reason,
                  checkpoint_json, started_at, ended_at, created_at
                ) values (
                  $1, $2, $3, null, $4::worker_kind, 'FAILED'::attempt_status,
                  null, null, null, null, null, null,
                  null, null, $5, $6,
                  null, null, $7, $7
                )
                "#,
            )
            .bind(Uuid::now_v7())
            .bind(task_id)
            .bind(attempt_no)
            .bind(current.task_type.default_worker_kind().as_str())
            .bind(failure_code)
            .bind(failure_reason)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }

        sqlx::query(
            r#"
            update tasks
               set status = 'FAILED'::task_status,
                   assigned_node_id = null,
                   current_attempt_no = $1,
                   updated_at = $2,
                   finished_at = $2
             where id = $3
            "#,
        )
        .bind(attempt_no)
        .bind(now)
        .bind(task_id)
        .execute(&mut *tx)
        .await?;

        self.insert_event(
            &mut tx,
            task_id,
            None,
            Some(attempt_no),
            EventSource::Core,
            "task_failed",
            "error",
            json!({
                "from": current.status,
                "to": TaskStatus::Failed,
                "attempt_no": attempt_no,
                "failure_code": failure_code,
                "failure_reason": failure_reason,
            }),
        )
        .await?;

        let deliver_after = self
            .terminal_state_callback_deliver_after(&mut tx, task_id, now)
            .await?;
        self.enqueue_task_completed_callback(
            &mut tx,
            task_id,
            attempt_no,
            "terminal_state",
            deliver_after,
        )
        .await?;

        tx.commit().await?;

        Ok(TaskSummary {
            status: TaskStatus::Failed,
            current_attempt_no: attempt_no,
            updated_at: now,
            finished_at: Some(now),
            ..current
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskCloneOverride {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub priority: Option<u8>,
    #[serde(default)]
    pub common: TaskCloneCommonOverride,
    #[serde(default)]
    pub schedule: TaskCloneScheduleOverride,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskCloneCommonOverride {
    #[serde(default)]
    pub created_by: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskCloneScheduleOverride {
    #[serde(default)]
    pub start_mode: Option<StartMode>,
}

fn operation_event_name(operation: TaskOperation) -> &'static str {
    match operation {
        TaskOperation::Start => "task_start_requested",
        TaskOperation::Stop => "task_stop_requested",
        TaskOperation::Cancel => "task_cancel_requested",
        TaskOperation::Retry => "task_retry_requested",
        TaskOperation::Clone => "task_clone_requested",
    }
}

fn apply_clone_overrides(spec: &mut TaskSpec, overrides: TaskCloneOverride) {
    if let Some(name) = overrides.name {
        spec.name = name;
    }
    if let Some(priority) = overrides.priority {
        spec.priority = priority;
    }
    if let Some(created_by) = overrides.common.created_by {
        spec.common.created_by = Some(created_by);
    }
    if let Some(start_mode) = overrides.schedule.start_mode {
        spec.schedule.start_mode = Some(start_mode);
    }
}
