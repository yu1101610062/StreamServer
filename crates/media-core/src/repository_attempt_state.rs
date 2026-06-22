//! Attempt 状态辅助：封装任务尝试的归属校验、状态推进、终态归档和重试判定逻辑。

use std::str::FromStr;

use chrono::{DateTime, Utc};
use media_domain::{AttemptStatus, EventSource, RecoveryPolicy, TaskSpec, TaskStatus};
use serde_json::{Value, json};
use sqlx::{Postgres, Row};
use uuid::Uuid;

use super::{RepoError, TaskRepository};

impl TaskRepository {
    pub(super) async fn delete_task_lease(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        task_id: Uuid,
    ) -> Result<(), RepoError> {
        sqlx::query("delete from task_leases where task_id = $1")
            .bind(task_id)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    pub(super) async fn delete_stream_bindings_for_task(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        task_id: Uuid,
    ) -> Result<(), RepoError> {
        sqlx::query("delete from stream_bindings where task_id = $1")
            .bind(task_id)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    pub(super) async fn validate_attempt_ownership(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        task_id: Uuid,
        attempt_no: i32,
        node_id: Uuid,
        lease_token: &str,
        message_kind: &str,
        mode: OwnershipMode,
    ) -> Result<Option<AttemptOwnership>, RepoError> {
        let task_row = sqlx::query(
            r#"
            select
              t.status::text as task_status,
              t.current_attempt_no,
              t.assigned_node_id,
              t.resolved_spec
            from tasks t
            where t.id = $1
            for update
            "#,
        )
        .bind(task_id)
        .fetch_optional(&mut **tx)
        .await?;

        let Some(task_row) = task_row else {
            return Ok(None);
        };

        let attempt_row = sqlx::query(
            r#"
            select
              ta.id as attempt_id,
              ta.node_id as attempt_node_id,
              nullif(ta.lease_token, '') as current_lease_token,
              ta.stop_requested_at,
              ta.desired_terminal_status::text as desired_terminal_status
            from task_attempts ta
            where ta.task_id = $1
              and ta.attempt_no = $2
            for update
            "#,
        )
        .bind(task_id)
        .bind(attempt_no)
        .fetch_optional(&mut **tx)
        .await?;

        let task_status = TaskStatus::from_str(&task_row.try_get::<String, _>("task_status")?)?;
        let current_attempt_no: i32 = task_row.try_get("current_attempt_no")?;
        let assigned_node_id: Option<Uuid> = task_row.try_get("assigned_node_id")?;
        let resolved_spec: Option<Value> = task_row.try_get("resolved_spec")?;
        let attempt_id = attempt_row
            .as_ref()
            .and_then(|row| row.try_get::<Option<Uuid>, _>("attempt_id").ok())
            .flatten();
        let attempt_node_id = attempt_row
            .as_ref()
            .and_then(|row| row.try_get::<Option<Uuid>, _>("attempt_node_id").ok())
            .flatten();
        let current_lease_token = attempt_row
            .as_ref()
            .and_then(|row| row.try_get::<Option<String>, _>("current_lease_token").ok())
            .flatten();
        let stop_requested_at = attempt_row
            .as_ref()
            .and_then(|row| {
                row.try_get::<Option<DateTime<Utc>>, _>("stop_requested_at")
                    .ok()
            })
            .flatten();
        let desired_terminal_status = attempt_row
            .as_ref()
            .and_then(|row| {
                row.try_get::<Option<String>, _>("desired_terminal_status")
                    .ok()
            })
            .flatten()
            .map(|value| TaskStatus::from_str(&value))
            .transpose()?;

        let base_owned = current_attempt_no == attempt_no
            && attempt_node_id == Some(node_id)
            && current_lease_token.as_deref() == Some(lease_token);
        let owned = match mode {
            OwnershipMode::CurrentOwner => base_owned && assigned_node_id == Some(node_id),
            OwnershipMode::AuthorizedAttempt => base_owned,
        };
        if owned {
            return Ok(Some(AttemptOwnership {
                attempt_id,
                task_status,
                resolved_spec,
                stop_requested_at,
                desired_terminal_status,
            }));
        }

        self.insert_event(
            tx,
            task_id,
            attempt_id,
            Some(attempt_no),
            EventSource::Core,
            "stale_agent_message",
            "warn",
            json!({
                "message_kind": message_kind,
                "incoming_node_id": node_id,
                "incoming_attempt_no": attempt_no,
                "incoming_lease_token": lease_token,
                "current_attempt_no": current_attempt_no,
                "assigned_node_id": assigned_node_id,
                "attempt_node_id": attempt_node_id,
                "current_lease_token": current_lease_token,
                "ownership_mode": match mode {
                    OwnershipMode::CurrentOwner => "current_owner",
                    OwnershipMode::AuthorizedAttempt => "authorized_attempt",
                },
            }),
        )
        .await?;

        Ok(None)
    }

    pub(super) async fn reconcile_exited_snapshot(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        ownership: &AttemptOwnership,
        task_id: Uuid,
        attempt_no: i32,
        node_id: Uuid,
        metadata: &Value,
        now: DateTime<Utc>,
    ) -> Result<(), RepoError> {
        if matches!(
            ownership.task_status,
            TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Canceled | TaskStatus::Lost
        ) {
            return Ok(());
        }

        let explicit_success = metadata
            .get("completion_reason")
            .and_then(Value::as_str)
            .is_some_and(|value| value == "record_duration_reached")
            || metadata.get("transcode_artifact").is_some()
            || metadata.get("bridge_artifact").is_some()
            || metadata
                .get("stream_ingest_record_artifacts")
                .and_then(Value::as_array)
                .is_some_and(|value| !value.is_empty());

        let disk_threshold_stop = metadata
            .get("stop")
            .and_then(|value| value.get("reason"))
            .and_then(Value::as_str)
            .is_some_and(|reason| reason == "disk_threshold_exceeded");
        if disk_threshold_stop {
            self.complete_task_attempt(
                tx,
                task_id,
                attempt_no,
                node_id,
                TaskStatus::Failed,
                AttemptStatus::Failed,
                Some("disk_threshold_exceeded"),
                Some("disk_threshold_exceeded"),
                now,
            )
            .await?;
            return Ok(());
        }

        if ownership.task_status == TaskStatus::Stopping || ownership.stop_requested_at.is_some() {
            self.complete_task_attempt(
                tx,
                task_id,
                attempt_no,
                node_id,
                ownership
                    .desired_terminal_status
                    .unwrap_or(TaskStatus::Canceled),
                AttemptStatus::Failed,
                Some("snapshot_exited_after_stop"),
                Some("runtime exited while stopping"),
                now,
            )
            .await?;
            return Ok(());
        }

        if explicit_success {
            self.complete_task_attempt(
                tx,
                task_id,
                attempt_no,
                node_id,
                TaskStatus::Succeeded,
                AttemptStatus::Succeeded,
                None,
                None,
                now,
            )
            .await?;
            return Ok(());
        }

        if sticky_reconnect_active(ownership)? {
            return Ok(());
        }

        let failure_reason = metadata
            .get("recording_fatal_error")
            .and_then(Value::as_str)
            .or_else(|| {
                metadata
                    .get("startup_timeout")
                    .and_then(Value::as_bool)
                    .filter(|value| *value)
                    .map(|_| "runtime exited after startup timeout")
            })
            .unwrap_or("runtime exited without a terminal task event");

        if metadata
            .get("startup_timeout")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            self.delete_stream_bindings_for_task(tx, task_id).await?;
        }

        self.complete_task_attempt(
            tx,
            task_id,
            attempt_no,
            node_id,
            TaskStatus::Failed,
            AttemptStatus::Failed,
            Some("snapshot_exited"),
            Some(failure_reason),
            now,
        )
        .await
    }

    pub(super) async fn consecutive_failed_attempts_before(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        task_id: Uuid,
        max_attempt_no: i32,
    ) -> Result<u32, RepoError> {
        if max_attempt_no <= 0 {
            return Ok(0);
        }

        let statuses = sqlx::query_scalar::<_, String>(
            r#"
            select status::text
              from task_attempts
             where task_id = $1
               and attempt_no <= $2
             order by attempt_no desc
            "#,
        )
        .bind(task_id)
        .bind(max_attempt_no)
        .fetch_all(&mut **tx)
        .await?;

        let mut consecutive = 0_u32;
        for status in statuses {
            if AttemptStatus::from_str(&status)? == AttemptStatus::Failed {
                consecutive += 1;
            } else {
                break;
            }
        }
        Ok(consecutive)
    }

    pub(super) async fn complete_task_attempt(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        task_id: Uuid,
        attempt_no: i32,
        node_id: Uuid,
        task_status: TaskStatus,
        attempt_status: AttemptStatus,
        failure_code: Option<&str>,
        failure_reason: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<(), RepoError> {
        sqlx::query(
            r#"
            update tasks
               set status = $1::task_status,
                   assigned_node_id = null,
                   reclaim_deadline_at = null,
                   updated_at = $2,
                   finished_at = $3
             where id = $4
               and current_attempt_no = $5
            "#,
        )
        .bind(task_status.as_str())
        .bind(now)
        .bind(now)
        .bind(task_id)
        .bind(attempt_no)
        .execute(&mut **tx)
        .await?;

        sqlx::query(
            r#"
            update task_attempts
               set status = $1::attempt_status,
                   node_id = $2,
                   failure_code = $3,
                   failure_reason = $4,
                   ended_at = $5
             where task_id = $6
               and attempt_no = $7
            "#,
        )
        .bind(attempt_status.as_str())
        .bind(node_id)
        .bind(failure_code)
        .bind(failure_reason)
        .bind(now)
        .bind(task_id)
        .bind(attempt_no)
        .execute(&mut **tx)
        .await?;

        self.delete_task_lease(tx, task_id).await?;
        let deliver_after = self
            .terminal_state_callback_deliver_after(tx, task_id, now)
            .await?;
        self.enqueue_task_completed_callback(
            tx,
            task_id,
            attempt_no,
            "terminal_state",
            deliver_after,
        )
        .await?;
        Ok(())
    }

    pub(super) async fn promote_task_running(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        task_id: Uuid,
        attempt_no: i32,
        node_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<(), RepoError> {
        sqlx::query(
            r#"
            update tasks
               set status = 'RUNNING'::task_status,
                   assigned_node_id = $1,
                   reclaim_deadline_at = null,
                   started_at = coalesce(started_at, $2),
                   updated_at = $2
             where id = $3
               and current_attempt_no = $4
               and status in (
                 'DISPATCHING',
                 'STARTING',
                 'RUNNING',
                 'RECOVERING',
                 'RECLAIMING'
               )
            "#,
        )
        .bind(node_id)
        .bind(now)
        .bind(task_id)
        .bind(attempt_no)
        .execute(&mut **tx)
        .await?;

        sqlx::query(
            r#"
            update task_attempts
               set status = 'RUNNING'::attempt_status,
                   node_id = $1,
                   started_at = coalesce(started_at, $2)
             where task_id = $3
               and attempt_no = $4
               and status in ('PENDING', 'STARTING', 'RUNNING', 'ADOPTED', 'ORPHANED')
            "#,
        )
        .bind(node_id)
        .bind(now)
        .bind(task_id)
        .bind(attempt_no)
        .execute(&mut **tx)
        .await?;

        self.enqueue_task_status_callback_if_needed(tx, task_id, attempt_no, "running", now)
            .await?;

        Ok(())
    }

    pub(super) async fn mark_task_lost(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        task_id: Uuid,
        attempt_no: i32,
        node_id: Uuid,
        failure_code: &str,
        failure_reason: &str,
        now: DateTime<Utc>,
    ) -> Result<(), RepoError> {
        let updated = sqlx::query(
            r#"
            update tasks
               set status = 'LOST'::task_status,
                   assigned_node_id = null,
                   reclaim_deadline_at = null,
                   updated_at = $1,
                   finished_at = $1
             where id = $2
               and current_attempt_no = $3
               and status in (
                 'DISPATCHING',
                 'STARTING',
                 'RUNNING',
                 'STOPPING',
                 'RECOVERING',
                 'RECLAIMING'
               )
            "#,
        )
        .bind(now)
        .bind(task_id)
        .bind(attempt_no)
        .execute(&mut **tx)
        .await?;

        if updated.rows_affected() == 0 {
            return Ok(());
        }

        self.delete_stream_bindings_for_task(tx, task_id).await?;

        sqlx::query(
            r#"
            update task_attempts
               set status = 'FAILED'::attempt_status,
                   node_id = $1,
                   failure_code = $2,
                   failure_reason = $3,
                   ended_at = $4
             where task_id = $5
               and attempt_no = $6
               and status in ('PENDING', 'STARTING', 'RUNNING', 'ADOPTED', 'ORPHANED')
            "#,
        )
        .bind(node_id)
        .bind(failure_code)
        .bind(failure_reason)
        .bind(now)
        .bind(task_id)
        .bind(attempt_no)
        .execute(&mut **tx)
        .await?;

        self.delete_task_lease(tx, task_id).await?;
        let deliver_after = self
            .terminal_state_callback_deliver_after(tx, task_id, now)
            .await?;
        self.enqueue_task_completed_callback(
            tx,
            task_id,
            attempt_no,
            "terminal_state",
            deliver_after,
        )
        .await?;
        Ok(())
    }
}

pub(super) fn retry_enabled_on_disconnect(spec: &TaskSpec) -> bool {
    !matches!(
        spec.recovery
            .policy
            .unwrap_or(RecoveryPolicy::default_for(spec.task_type)),
        RecoveryPolicy::Never
    )
}

pub(super) fn sticky_reconnect_from_spec_value(value: Option<&Value>) -> Result<bool, RepoError> {
    Ok(value
        .cloned()
        .map(serde_json::from_value::<TaskSpec>)
        .transpose()?
        .is_some_and(|spec| spec.stream_ingest_uses_sticky_reconnect()))
}

pub(super) fn sticky_reconnect_active(ownership: &AttemptOwnership) -> Result<bool, RepoError> {
    Ok(
        sticky_reconnect_from_spec_value(ownership.resolved_spec.as_ref())?
            && ownership.task_status != TaskStatus::Stopping
            && ownership.stop_requested_at.is_none(),
    )
}

pub(super) fn start_rejected_retry_limit(spec: &TaskSpec) -> Option<u32> {
    if !retry_enabled_on_disconnect(spec) {
        return Some(0);
    }
    Some(
        spec.recovery
            .max_consecutive_failures
            .unwrap_or(DEFAULT_MAX_CONSECUTIVE_FAILURES),
    )
}

pub(super) const DEFAULT_MAX_CONSECUTIVE_FAILURES: u32 = 3;

#[derive(Debug, Clone)]
pub(super) struct AttemptOwnership {
    pub(super) attempt_id: Option<Uuid>,
    pub(super) task_status: TaskStatus,
    pub(super) resolved_spec: Option<Value>,
    pub(super) stop_requested_at: Option<DateTime<Utc>>,
    pub(super) desired_terminal_status: Option<TaskStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OwnershipMode {
    CurrentOwner,
    AuthorizedAttempt,
}
