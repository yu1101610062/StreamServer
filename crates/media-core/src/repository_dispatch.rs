//! 调度仓储：负责领取任务、停止/回收运行时、录像控制命令以及调度侧的状态协调。

use std::str::FromStr;

use chrono::{DateTime, Utc};
use media_domain::{AttemptStatus, EventSource, TaskSpec, TaskStatus, TaskType, WorkerKind};
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;

use super::{
    RepoError, TaskRepository, TaskSummary, retry_enabled_on_disconnect, validation_error,
};

impl TaskRepository {
    pub async fn prepare_task_dispatch(
        &self,
        task_id: Uuid,
        node_id: Uuid,
        holder: &str,
    ) -> Result<DispatchCommand, RepoError> {
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
              finished_at,
              resolved_spec
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

        let resolved_spec = row
            .try_get::<Option<Value>, _>("resolved_spec")?
            .ok_or(RepoError::TaskMissingResolvedSpec(task_id))?;
        let now = Utc::now();
        let lease_token = Uuid::now_v7().to_string();
        let pending_attempt = if current.current_attempt_no > 0 {
            sqlx::query(
                r#"
                select id, worker_kind::text as worker_kind, status::text as status
                  from task_attempts
                 where task_id = $1
                   and attempt_no = $2
                 for update
                "#,
            )
            .bind(task_id)
            .bind(current.current_attempt_no)
            .fetch_optional(&mut *tx)
            .await?
        } else {
            None
        };
        let reuse_pending_attempt = pending_attempt.as_ref().is_some_and(|row| {
            row.try_get::<String, _>("status").ok().as_deref() == Some("PENDING")
        });
        let attempt_no = if reuse_pending_attempt {
            current.current_attempt_no
        } else {
            current.current_attempt_no.max(0) + 1
        };
        let worker_kind = if reuse_pending_attempt {
            let worker_kind = pending_attempt
                .as_ref()
                .expect("pending attempt is present when reuse is enabled")
                .try_get::<String, _>("worker_kind")?;
            WorkerKind::from_str(&worker_kind)?
        } else {
            current.task_type.default_worker_kind()
        };

        if reuse_pending_attempt {
            sqlx::query(
                r#"
                update task_attempts
                   set node_id = $1,
                       lease_token = $2,
                       failure_code = null,
                       failure_reason = null,
                       ended_at = null
                 where task_id = $3
                   and attempt_no = $4
                   and status = 'PENDING'::attempt_status
                "#,
            )
            .bind(node_id)
            .bind(&lease_token)
            .bind(task_id)
            .bind(attempt_no)
            .execute(&mut *tx)
            .await?;
        } else {
            sqlx::query(
                r#"
                insert into task_attempts (
                  id, task_id, attempt_no, node_id, worker_kind, status, lease_token,
                  pid, zlm_key, zlm_schema, zlm_vhost, zlm_app, zlm_stream,
                  rtp_port, exit_code, failure_code, failure_reason,
                  checkpoint_json, started_at, ended_at, created_at
                ) values (
                  $1, $2, $3, $4, $5::worker_kind, 'PENDING'::attempt_status, $6,
                  null, null, null, null, null, null,
                  null, null, null, null,
                  null, null, null, $7
                )
                "#,
            )
            .bind(Uuid::now_v7())
            .bind(task_id)
            .bind(attempt_no)
            .bind(node_id)
            .bind(worker_kind.as_str())
            .bind(&lease_token)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }

        sqlx::query(
            r#"
            insert into task_leases (task_id, holder, lease_token, node_id, expires_at, updated_at)
            values ($1, $2, $3, $4, $5, $6)
            on conflict (task_id) do update
               set holder = excluded.holder,
                   lease_token = excluded.lease_token,
                   node_id = excluded.node_id,
                   expires_at = excluded.expires_at,
                   updated_at = excluded.updated_at
            "#,
        )
        .bind(task_id)
        .bind(holder)
        .bind(&lease_token)
        .bind(node_id)
        .bind(now + chrono::Duration::seconds(60))
        .bind(now)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r#"
            update tasks
               set status = 'DISPATCHING'::task_status,
                   assigned_node_id = $1,
                   current_attempt_no = $2,
                   updated_at = $3
             where id = $4
            "#,
        )
        .bind(node_id)
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
            "task_dispatched",
            "info",
            json!({
                "node_id": node_id,
                "attempt_no": attempt_no,
                "lease_token": lease_token,
            }),
        )
        .await?;

        tx.commit().await?;

        Ok(DispatchCommand {
            task_id,
            attempt_no,
            node_id,
            task_type: current.task_type,
            resolved_spec,
            lease_token,
        })
    }

    pub async fn rollback_task_dispatch(
        &self,
        task_id: Uuid,
        attempt_no: i32,
        node_id: Uuid,
        reason: &str,
    ) -> Result<(), RepoError> {
        let mut tx = self.pool.begin().await?;
        let now = Utc::now();

        sqlx::query(
            r#"
            update tasks
               set status = 'QUEUED'::task_status,
                   assigned_node_id = null,
                   updated_at = $1
             where id = $2
               and current_attempt_no = $3
               and status = 'DISPATCHING'::task_status
            "#,
        )
        .bind(now)
        .bind(task_id)
        .bind(attempt_no)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r#"
            update task_attempts
               set status = 'FAILED'::attempt_status,
                   node_id = $1,
                   failure_code = 'dispatch_send_failed',
                   failure_reason = $2,
                   ended_at = $3
             where task_id = $4
               and attempt_no = $5
               and status = 'PENDING'::attempt_status
            "#,
        )
        .bind(node_id)
        .bind(reason)
        .bind(now)
        .bind(task_id)
        .bind(attempt_no)
        .execute(&mut *tx)
        .await?;

        self.delete_task_lease(&mut tx, task_id).await?;
        self.insert_event(
            &mut tx,
            task_id,
            None,
            Some(attempt_no),
            EventSource::Core,
            "task_dispatch_failed",
            "warn",
            json!({
                "node_id": node_id,
                "attempt_no": attempt_no,
                "reason": reason,
                "requeued": true,
            }),
        )
        .await?;

        tx.commit().await?;
        Ok(())
    }

    pub async fn build_stop_command(
        &self,
        task_id: Uuid,
        reason: impl Into<String>,
        grace_period_sec: u32,
        force_after_sec: u32,
    ) -> Result<Option<StopCommand>, RepoError> {
        let reason = reason.into();
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            r#"
            select assigned_node_id, current_attempt_no
              from tasks
             where id = $1
             for update
            "#,
        )
        .bind(task_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(RepoError::TaskNotFound(task_id))?;

        let node_id: Option<Uuid> = row.try_get("assigned_node_id")?;
        let attempt_no: i32 = row.try_get("current_attempt_no")?;
        let Some(node_id) = node_id else {
            return Ok(None);
        };
        if attempt_no <= 0 {
            return Ok(None);
        }

        let lease_token = sqlx::query_scalar::<_, Option<String>>(
            r#"
            select nullif(lease_token, '')
              from task_attempts
             where task_id = $1
               and attempt_no = $2
             for update
            "#,
        )
        .bind(task_id)
        .bind(attempt_no)
        .fetch_optional(&mut *tx)
        .await?
        .flatten()
        .ok_or_else(|| validation_error("lease_token", "current attempt is missing lease_token"))?;

        let now = Utc::now();
        sqlx::query(
            r#"
            update task_attempts
               set stop_requested_at = coalesce(stop_requested_at, $1),
                   stop_reason = $2,
                   desired_terminal_status = coalesce(desired_terminal_status, 'CANCELED'::task_status),
                   status = 'STOPPING'::attempt_status
             where task_id = $3
               and attempt_no = $4
            "#,
        )
        .bind(now)
        .bind(&reason)
        .bind(task_id)
        .bind(attempt_no)
        .execute(&mut *tx)
        .await?;

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
        .bind(task_id)
        .bind(attempt_no)
        .execute(&mut *tx)
        .await?;

        self.insert_event(
            &mut tx,
            task_id,
            None,
            Some(attempt_no),
            EventSource::Core,
            "task_stop_intent_persisted",
            "info",
            json!({
                "attempt_no": attempt_no,
                "node_id": node_id,
                "reason": reason,
                "grace_period_sec": grace_period_sec,
                "force_after_sec": force_after_sec,
            }),
        )
        .await?;

        tx.commit().await?;

        Ok(Some(StopCommand {
            task_id,
            attempt_no,
            node_id,
            lease_token,
            reason,
            grace_period_sec,
            force_after_sec,
        }))
    }

    pub async fn build_recording_control_command(
        &self,
        task_id: Uuid,
    ) -> Result<RecordingControlCommand, RepoError> {
        let row = sqlx::query(
            r#"
            select
              t.status::text as task_status,
              t.assigned_node_id,
              t.current_attempt_no,
              t.resolved_spec,
              nullif(ta.lease_token, '') as lease_token,
              sb.id as stream_binding_id
            from tasks t
            left join task_attempts ta
              on ta.task_id = t.id
             and ta.attempt_no = t.current_attempt_no
            left join stream_bindings sb
              on sb.attempt_id = ta.id
            where t.id = $1
            order by sb.created_at desc
            limit 1
            "#,
        )
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(RepoError::TaskNotFound(task_id))?;

        let status = TaskStatus::from_str(&row.try_get::<String, _>("task_status")?)?;
        if status != TaskStatus::Running {
            return Err(RepoError::RecordingControlUnsupported(format!(
                "task must be RUNNING to control recording, current status is {status}"
            )));
        }
        let node_id: Option<Uuid> = row.try_get("assigned_node_id")?;
        let Some(node_id) = node_id else {
            return Err(RepoError::RecordingControlUnsupported(
                "task has no assigned media node".to_string(),
            ));
        };
        let attempt_no: i32 = row.try_get("current_attempt_no")?;
        if attempt_no <= 0 {
            return Err(RepoError::RecordingControlUnsupported(
                "task has no active attempt".to_string(),
            ));
        }
        let lease_token: Option<String> = row.try_get("lease_token")?;
        let Some(lease_token) = lease_token else {
            return Err(validation_error(
                "lease_token",
                "current attempt is missing lease_token",
            ));
        };
        let resolved_spec: Value = row
            .try_get::<Option<Value>, _>("resolved_spec")?
            .ok_or(RepoError::TaskMissingResolvedSpec(task_id))?;
        let spec = serde_json::from_value::<TaskSpec>(resolved_spec.clone())?;
        if !spec.supports_runtime_recording_control() {
            return Err(RepoError::RecordingControlUnsupported(
                "only realtime stream_ingest tasks support runtime recording control".to_string(),
            ));
        }
        let stream_binding_id: Option<Uuid> = row.try_get("stream_binding_id")?;
        if stream_binding_id.is_none() {
            return Err(RepoError::RecordingControlUnsupported(
                "current attempt has no ZLM stream binding".to_string(),
            ));
        }

        Ok(RecordingControlCommand {
            task_id,
            attempt_no,
            node_id,
            lease_token,
        })
    }

    pub async fn list_reclaim_runtimes(
        &self,
        node_id: Uuid,
    ) -> Result<Vec<ReclaimRuntimeCommand>, RepoError> {
        sqlx::query(
            r#"
            select
              t.id as task_id,
              ta.attempt_no,
              nullif(ta.lease_token, '') as lease_token,
              ta.worker_kind::text as worker_kind
            from tasks t
            join task_attempts ta
              on ta.task_id = t.id
             and ta.attempt_no = t.current_attempt_no
            where ta.node_id = $1
              and t.status in (
                'DISPATCHING'::task_status,
                'STARTING'::task_status,
                'RUNNING'::task_status,
                'STOPPING'::task_status,
                'RECOVERING'::task_status,
                'RECLAIMING'::task_status
              )
              and coalesce(ta.lease_token, '') <> ''
            order by t.updated_at asc, t.id asc
            "#,
        )
        .bind(node_id)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| {
            Ok(ReclaimRuntimeCommand {
                task_id: row.try_get("task_id")?,
                attempt_no: row.try_get("attempt_no")?,
                lease_token: row.try_get::<String, _>("lease_token")?,
                worker_kind: WorkerKind::from_str(&row.try_get::<String, _>("worker_kind")?)?,
            })
        })
        .collect()
    }

    pub async fn list_reclaiming_tasks(&self) -> Result<Vec<ReclaimingTaskReconcile>, RepoError> {
        sqlx::query(
            r#"
            select
              t.id as task_id,
              t.current_attempt_no as attempt_no,
              ta.node_id,
              ta.status::text as attempt_status,
              t.reclaim_deadline_at
            from tasks t
            join task_attempts ta
              on ta.task_id = t.id
             and ta.attempt_no = t.current_attempt_no
            where t.status = 'RECLAIMING'::task_status
              and t.reclaim_deadline_at is not null
              and ta.node_id is not null
            order by t.reclaim_deadline_at asc, t.updated_at asc
            "#,
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| {
            Ok(ReclaimingTaskReconcile {
                task_id: row.try_get("task_id")?,
                attempt_no: row.try_get("attempt_no")?,
                node_id: row.try_get("node_id")?,
                attempt_status: AttemptStatus::from_str(
                    &row.try_get::<String, _>("attempt_status")?,
                )?,
                reclaim_deadline_at: row.try_get("reclaim_deadline_at")?,
            })
        })
        .collect()
    }

    pub async fn finalize_reclaim_timeout(
        &self,
        candidate: &ReclaimingTaskReconcile,
    ) -> Result<bool, RepoError> {
        self.finalize_reclaiming_task(
            candidate,
            "node_disconnected",
            "control-plane session did not recover before reclaim deadline",
            "task_lost_after_reclaim_timeout",
            "reclaim_timeout",
        )
        .await
    }

    pub async fn finalize_reclaim_orphaned(
        &self,
        candidate: &ReclaimingTaskReconcile,
    ) -> Result<bool, RepoError> {
        self.finalize_reclaiming_task(
            candidate,
            "runtime_not_found",
            "reclaimed runtime was reported missing by the agent",
            "task_lost_after_reclaim_orphaned",
            "runtime_not_found",
        )
        .await
    }

    pub async fn attempt_has_stop_intent(
        &self,
        task_id: Uuid,
        attempt_no: i32,
    ) -> Result<bool, RepoError> {
        sqlx::query_scalar(
            r#"
            select exists (
              select 1
                from task_attempts
               where task_id = $1
                 and attempt_no = $2
                 and stop_requested_at is not null
            )
            "#,
        )
        .bind(task_id)
        .bind(attempt_no)
        .fetch_one(&self.pool)
        .await
        .map_err(Into::into)
    }

    pub async fn list_stopping_reconcile_tasks(
        &self,
    ) -> Result<Vec<StoppingTaskReconcile>, RepoError> {
        sqlx::query(
            r#"
            select
              t.id as task_id,
              t.current_attempt_no as attempt_no,
              ta.node_id,
              ta.status::text as attempt_status,
              ta.stop_requested_at,
              coalesce(ta.desired_terminal_status::text, 'CANCELED') as desired_terminal_status
            from tasks t
            join task_attempts ta
              on ta.task_id = t.id
             and ta.attempt_no = t.current_attempt_no
            where t.status = 'STOPPING'::task_status
              and ta.stop_requested_at is not null
              and ta.node_id is not null
            order by ta.stop_requested_at asc, t.updated_at asc
            "#,
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| {
            Ok(StoppingTaskReconcile {
                task_id: row.try_get("task_id")?,
                attempt_no: row.try_get("attempt_no")?,
                node_id: row.try_get("node_id")?,
                attempt_status: AttemptStatus::from_str(
                    &row.try_get::<String, _>("attempt_status")?,
                )?,
                stop_requested_at: row.try_get("stop_requested_at")?,
                desired_terminal_status: TaskStatus::from_str(
                    &row.try_get::<String, _>("desired_terminal_status")?,
                )?,
            })
        })
        .collect()
    }

    pub async fn complete_stopping_task(
        &self,
        candidate: &StoppingTaskReconcile,
    ) -> Result<bool, RepoError> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            r#"
            select status::text as task_status, current_attempt_no
              from tasks
             where id = $1
             for update
            "#,
        )
        .bind(candidate.task_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Ok(false);
        };
        let task_status = TaskStatus::from_str(&row.try_get::<String, _>("task_status")?)?;
        let current_attempt_no: i32 = row.try_get("current_attempt_no")?;
        if current_attempt_no != candidate.attempt_no || task_status != TaskStatus::Stopping {
            return Ok(false);
        }

        self.complete_task_attempt(
            &mut tx,
            candidate.task_id,
            candidate.attempt_no,
            candidate.node_id,
            candidate.desired_terminal_status,
            AttemptStatus::Failed,
            Some("stop_reconcile_completed"),
            Some("runtime missing after stop request"),
            Utc::now(),
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn mark_stopping_timeout(
        &self,
        candidate: &StoppingTaskReconcile,
    ) -> Result<bool, RepoError> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            r#"
            select status::text as task_status, current_attempt_no
              from tasks
             where id = $1
             for update
            "#,
        )
        .bind(candidate.task_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Ok(false);
        };
        let task_status = TaskStatus::from_str(&row.try_get::<String, _>("task_status")?)?;
        let current_attempt_no: i32 = row.try_get("current_attempt_no")?;
        if current_attempt_no != candidate.attempt_no || task_status != TaskStatus::Stopping {
            return Ok(false);
        }

        self.mark_task_lost(
            &mut tx,
            candidate.task_id,
            candidate.attempt_no,
            candidate.node_id,
            "stop_timeout",
            "stop request did not converge before deadline",
            Utc::now(),
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    async fn finalize_reclaiming_task(
        &self,
        candidate: &ReclaimingTaskReconcile,
        failure_code: &str,
        failure_reason: &str,
        event_type: &str,
        reason: &str,
    ) -> Result<bool, RepoError> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            r#"
            select
              t.status::text as task_status,
              t.current_attempt_no,
              t.resolved_spec,
              ta.stop_requested_at
            from tasks t
            join task_attempts ta
              on ta.task_id = t.id
             and ta.attempt_no = t.current_attempt_no
            where t.id = $1
            for update
            "#,
        )
        .bind(candidate.task_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Ok(false);
        };
        let task_status = TaskStatus::from_str(&row.try_get::<String, _>("task_status")?)?;
        let current_attempt_no: i32 = row.try_get("current_attempt_no")?;
        if current_attempt_no != candidate.attempt_no || task_status != TaskStatus::Reclaiming {
            return Ok(false);
        }

        let resolved_spec: Option<Value> = row.try_get("resolved_spec")?;
        let stop_requested_at: Option<DateTime<Utc>> = row.try_get("stop_requested_at")?;
        let should_retry = stop_requested_at.is_none()
            && candidate.attempt_status != AttemptStatus::Stopping
            && resolved_spec
                .as_ref()
                .and_then(|value| serde_json::from_value::<TaskSpec>(value.clone()).ok())
                .is_some_and(|spec| retry_enabled_on_disconnect(&spec));
        let now = Utc::now();

        self.mark_task_lost(
            &mut tx,
            candidate.task_id,
            candidate.attempt_no,
            candidate.node_id,
            failure_code,
            failure_reason,
            now,
        )
        .await?;
        self.insert_event(
            &mut tx,
            candidate.task_id,
            None,
            Some(candidate.attempt_no),
            EventSource::Core,
            event_type,
            "warn",
            json!({
                "node_id": candidate.node_id,
                "attempt_no": candidate.attempt_no,
                "reason": reason,
                "auto_retry": should_retry,
            }),
        )
        .await?;
        tx.commit().await?;

        if should_retry {
            let current = self.fetch_task_summary(candidate.task_id).await?;
            if current.status == TaskStatus::Lost {
                self.enqueue_retry(
                    current,
                    EventSource::Core,
                    "task_retry_after_node_disconnect",
                    json!({
                        "reason": reason,
                        "auto_retry": true,
                    }),
                )
                .await?;
            }
        }

        Ok(true)
    }
}

#[derive(Debug, Clone)]
pub struct DispatchCommand {
    pub task_id: Uuid,
    pub attempt_no: i32,
    pub node_id: Uuid,
    pub task_type: TaskType,
    pub resolved_spec: Value,
    pub lease_token: String,
}

#[derive(Debug, Clone)]
pub struct StopCommand {
    pub task_id: Uuid,
    pub attempt_no: i32,
    pub node_id: Uuid,
    pub lease_token: String,
    pub reason: String,
    pub grace_period_sec: u32,
    pub force_after_sec: u32,
}

#[derive(Debug, Clone)]
pub struct RecordingControlCommand {
    pub task_id: Uuid,
    pub attempt_no: i32,
    pub node_id: Uuid,
    pub lease_token: String,
}

#[derive(Debug, Clone)]
pub struct ReclaimRuntimeCommand {
    pub task_id: Uuid,
    pub attempt_no: i32,
    pub lease_token: String,
    pub worker_kind: WorkerKind,
}

#[derive(Debug, Clone)]
pub struct ReclaimingTaskReconcile {
    pub task_id: Uuid,
    pub attempt_no: i32,
    pub node_id: Uuid,
    pub attempt_status: AttemptStatus,
    pub reclaim_deadline_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct StoppingTaskReconcile {
    pub task_id: Uuid,
    pub attempt_no: i32,
    pub node_id: Uuid,
    pub attempt_status: AttemptStatus,
    pub stop_requested_at: DateTime<Utc>,
    pub desired_terminal_status: TaskStatus,
}
