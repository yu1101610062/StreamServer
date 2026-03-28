use std::str::FromStr;

use chrono::{DateTime, Utc};
use media_domain::{
    AgentRegistration, AttemptStatus, CapabilitySnapshot, EventSource, HeartbeatSnapshot, Page,
    StartMode, TaskOperation, TaskSpec, TaskStateError, TaskStatus, TaskType, WorkerKind,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, QueryBuilder, Row, postgres::PgRow};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct TaskRepository {
    pool: PgPool,
}

impl TaskRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn health_check(&self) -> Result<(), RepoError> {
        sqlx::query("select 1").execute(&self.pool).await?;
        Ok(())
    }

    pub async fn create_task(
        &self,
        idempotency_key: &str,
        request_hash: &str,
        requested_spec: TaskSpec,
    ) -> Result<CreateTaskResult, RepoError> {
        let resolved_spec = requested_spec.resolved();
        requested_spec.validate()?;
        resolved_spec.validate()?;

        let tenant_id = resolved_spec.tenant_id().to_string();
        let created_by = resolved_spec.created_by().unwrap_or("system").to_string();
        let status = resolved_spec.initial_status();
        let task_id = Uuid::now_v7();
        let created_at = Utc::now();
        let updated_at = created_at;
        let summary = TaskSummary {
            id: task_id,
            tenant_id: tenant_id.clone(),
            name: resolved_spec.name.clone(),
            task_type: resolved_spec.task_type,
            status,
            template_id: None,
            profile: resolved_spec.profile.clone(),
            priority: resolved_spec.priority,
            assigned_node_id: None,
            current_attempt_no: 0,
            created_at,
            updated_at,
            started_at: None,
            finished_at: None,
        };

        let mut tx = self.pool.begin().await?;

        if let Some(existing) = sqlx::query_as::<_, OperationRequestRow>(
            r#"
            select request_hash, response_body
              from operation_requests
             where tenant_id = $1
               and operation_key = $2
               and method = 'POST'
               and path = '/api/v1/tasks'
            "#,
        )
        .bind(&tenant_id)
        .bind(idempotency_key)
        .fetch_optional(&mut *tx)
        .await?
        {
            if existing.request_hash != request_hash {
                return Err(RepoError::IdempotencyConflict);
            }

            if let Some(body) = existing.response_body {
                let replayed = serde_json::from_value::<TaskSummary>(body)?;
                tx.commit().await?;
                return Ok(CreateTaskResult::Replay(replayed));
            }

            return Err(RepoError::OperationInProgress);
        }

        sqlx::query(
            r#"
            insert into operation_requests (
              id, tenant_id, operation_key, method, path, request_hash, created_at
            ) values ($1, $2, $3, 'POST', '/api/v1/tasks', $4, $5)
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(&tenant_id)
        .bind(idempotency_key)
        .bind(request_hash)
        .bind(created_at)
        .execute(&mut *tx)
        .await?;

        let requested_spec_json = serde_json::to_value(&requested_spec)?;
        let resolved_spec_json = serde_json::to_value(&resolved_spec)?;

        sqlx::query(
            r#"
            insert into tasks (
              id, tenant_id, name, type, status, template_id, profile, idempotency_key,
              priority, requested_spec, resolved_spec, created_by, assigned_node_id,
              current_attempt_no, schedule_start_mode, created_at, updated_at, started_at, finished_at
            ) values (
              $1, $2, $3, $4::task_type, $5::task_status, null, $6, $7,
              $8, $9, $10, $11, null,
              0, $12, $13, $14, null, null
            )
            "#,
        )
        .bind(task_id)
        .bind(&tenant_id)
        .bind(&resolved_spec.name)
        .bind(resolved_spec.task_type.as_str())
        .bind(status.as_str())
        .bind(resolved_spec.profile.as_deref())
        .bind(idempotency_key)
        .bind(i32::from(resolved_spec.priority))
        .bind(requested_spec_json)
        .bind(resolved_spec_json)
        .bind(&created_by)
        .bind(
            resolved_spec
                .schedule
                .start_mode
                .unwrap_or(media_domain::StartMode::Immediate)
                .as_str(),
        )
        .bind(created_at)
        .bind(updated_at)
        .execute(&mut *tx)
        .await?;

        self.insert_event(
            &mut tx,
            task_id,
            None,
            None,
            EventSource::Core,
            "task_created",
            "info",
            json!({
                "status": status,
                "schedule_start_mode": resolved_spec.schedule.start_mode,
            }),
        )
        .await?;

        sqlx::query(
            r#"
            update operation_requests
               set resource_type = 'task',
                   resource_id = $1,
                   response_status = 201,
                   response_body = $2
             where tenant_id = $3
               and operation_key = $4
               and method = 'POST'
               and path = '/api/v1/tasks'
            "#,
        )
        .bind(task_id)
        .bind(serde_json::to_value(&summary)?)
        .bind(&tenant_id)
        .bind(idempotency_key)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(CreateTaskResult::Fresh(summary))
    }

    pub async fn list_tasks(&self, filter: TaskListFilter) -> Result<Page<TaskSummary>, RepoError> {
        let page = filter.page.unwrap_or(1).max(1);
        let page_size = filter.page_size.unwrap_or(20).clamp(1, 100);
        let total = self.count_tasks(&filter).await?;

        let mut builder = QueryBuilder::<Postgres>::new(
            r#"
            select
              id,
              tenant_id,
              name,
              type::text as task_type,
              status::text as status,
              template_id,
              profile,
              priority,
              assigned_node_id,
              current_attempt_no,
              created_at,
              updated_at,
              started_at,
              finished_at
            from tasks
            where 1 = 1
            "#,
        );
        self.apply_filters(&mut builder, &filter);
        builder.push(" order by ");
        builder.push(sort_by_clause(filter.sort_by.as_deref()));
        builder.push(" ");
        builder.push(sort_order_clause(filter.sort_order.as_deref()));
        builder.push(" limit ");
        builder.push_bind(i64::from(page_size));
        builder.push(" offset ");
        builder.push_bind(i64::from((page - 1) * page_size));

        let rows = builder
            .build()
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|row| TaskSummary::from_row(&row))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Page::new(rows, page, page_size, total))
    }

    pub async fn get_task(&self, task_id: Uuid) -> Result<TaskDetail, RepoError> {
        let row = sqlx::query(
            r#"
            select
              id,
              tenant_id,
              name,
              type::text as task_type,
              status::text as status,
              template_id,
              profile,
              priority,
              assigned_node_id,
              current_attempt_no,
              created_at,
              updated_at,
              started_at,
              finished_at,
              requested_spec,
              resolved_spec
            from tasks
            where id = $1
            "#,
        )
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(RepoError::TaskNotFound(task_id))?;

        let requested_spec: Value = row.try_get("requested_spec")?;
        let resolved_spec: Option<Value> = row.try_get("resolved_spec")?;
        let task = TaskSummary::from_row(&row)?;

        let current_attempt = if task.current_attempt_no > 0 {
            sqlx::query(
                r#"
                select
                  id,
                  attempt_no,
                  worker_kind::text as worker_kind,
                  status::text as status,
                  node_id,
                  pid,
                  exit_code,
                  failure_code,
                  failure_reason,
                  started_at,
                  ended_at
                from task_attempts
                where task_id = $1 and attempt_no = $2
                "#,
            )
            .bind(task_id)
            .bind(task.current_attempt_no)
            .fetch_optional(&self.pool)
            .await?
            .map(|row| AttemptSummary::from_row(&row))
            .transpose()?
        } else {
            None
        };

        let recent_events = sqlx::query(
            r#"
            select
              id,
              attempt_no,
              source::text as source,
              event_type,
              event_level,
              payload,
              created_at
            from task_events
            where task_id = $1
            order by created_at desc
            limit 20
            "#,
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| TaskEventSummary::from_row(&row))
        .collect::<Result<Vec<_>, _>>()?;

        Ok(TaskDetail {
            task,
            requested_spec,
            resolved_spec,
            current_attempt,
            recent_events,
        })
    }

    pub async fn get_task_summary(&self, task_id: Uuid) -> Result<TaskSummary, RepoError> {
        self.fetch_task_summary(task_id).await
    }

    pub async fn get_resolved_spec(&self, task_id: Uuid) -> Result<Value, RepoError> {
        let resolved =
            sqlx::query_scalar::<_, Option<Value>>("select resolved_spec from tasks where id = $1")
                .bind(task_id)
                .fetch_optional(&self.pool)
                .await?
                .flatten()
                .ok_or(RepoError::TaskNotFound(task_id))?;

        Ok(resolved)
    }

    pub async fn transition_task(
        &self,
        task_id: Uuid,
        operation: TaskOperation,
    ) -> Result<TaskSummary, RepoError> {
        let current = self.fetch_task_summary(task_id).await?;
        let next_status = current.status.apply_operation(operation)?;
        let updated_at = Utc::now();
        let finished_at = match next_status {
            TaskStatus::Canceled | TaskStatus::Succeeded | TaskStatus::Failed => Some(updated_at),
            _ => None,
        };

        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"
            update tasks
               set status = $1::task_status,
                   updated_at = $2,
                   finished_at = $3
             where id = $4
            "#,
        )
        .bind(next_status.as_str())
        .bind(updated_at)
        .bind(finished_at)
        .bind(task_id)
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
        current.status.apply_operation(TaskOperation::Retry)?;

        let attempt_no = current.current_attempt_no + 1;
        let worker_kind = current.task_type.default_worker_kind();
        let created_at = Utc::now();
        let attempt_id = Uuid::now_v7();

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
            EventSource::User,
            "task_retry_requested",
            "info",
            json!({
                "from": current.status,
                "to": TaskStatus::Queued,
                "attempt_no": attempt_no,
            }),
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

    pub async fn clone_task(
        &self,
        task_id: Uuid,
        overrides: Option<TaskCloneOverride>,
    ) -> Result<TaskSummary, RepoError> {
        let row = sqlx::query(
            r#"
            select
              id,
              tenant_id,
              name,
              type::text as task_type,
              status::text as status,
              template_id,
              profile,
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

        let requested_spec: Value = row.try_get("requested_spec")?;
        let template_id: Option<Uuid> = row.try_get("template_id")?;
        let mut requested_spec: TaskSpec = serde_json::from_value(requested_spec)?;
        if let Some(overrides) = overrides {
            apply_clone_overrides(&mut requested_spec, overrides);
        }
        let resolved_spec = requested_spec.resolved();
        requested_spec.validate()?;
        resolved_spec.validate()?;

        let tenant_id = resolved_spec.tenant_id().to_string();
        let created_by = resolved_spec.created_by().unwrap_or("system").to_string();
        let initial_status = requested_spec.initial_status();
        let new_id = Uuid::now_v7();
        let now = Utc::now();

        let summary = TaskSummary {
            id: new_id,
            tenant_id: tenant_id.clone(),
            name: resolved_spec.name.clone(),
            task_type: resolved_spec.task_type,
            status: initial_status,
            template_id,
            profile: resolved_spec.profile.clone(),
            priority: resolved_spec.priority,
            assigned_node_id: None,
            current_attempt_no: 0,
            created_at: now,
            updated_at: now,
            started_at: None,
            finished_at: None,
        };

        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"
            insert into tasks (
              id, tenant_id, name, type, status, template_id, profile, idempotency_key,
              priority, requested_spec, resolved_spec, created_by, assigned_node_id,
              current_attempt_no, schedule_start_mode, created_at, updated_at, started_at, finished_at
            ) values (
              $1, $2, $3, $4::task_type, $5::task_status, $6, $7, $8,
              $9, $10, $11, $12, null,
              0, $13, $14, $15, null, null
            )
            "#,
        )
        .bind(new_id)
        .bind(&tenant_id)
        .bind(&summary.name)
        .bind(summary.task_type.as_str())
        .bind(summary.status.as_str())
        .bind(summary.template_id)
        .bind(summary.profile.as_deref())
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
              tenant_id,
              name,
              type::text as task_type,
              status::text as status,
              template_id,
              profile,
              priority,
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
              tenant_id,
              name,
              type::text as task_type,
              status::text as status,
              template_id,
              profile,
              priority,
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
        let attempt_no = if current.current_attempt_no > 0 {
            current.current_attempt_no
        } else {
            1
        };
        let worker_kind = current.task_type.default_worker_kind();
        let now = Utc::now();
        let lease_token = Uuid::now_v7().to_string();
        let attempt_id = Uuid::now_v7();

        let updated = sqlx::query(
            r#"
            update task_attempts
               set node_id = $1,
                   worker_kind = $2::worker_kind,
                   status = 'PENDING'::attempt_status,
                   pid = null,
                   exit_code = null,
                   failure_code = null,
                   failure_reason = null,
                   checkpoint_json = null,
                   started_at = null,
                   ended_at = null
             where task_id = $3
               and attempt_no = $4
            "#,
        )
        .bind(node_id)
        .bind(worker_kind.as_str())
        .bind(task_id)
        .bind(attempt_no)
        .execute(&mut *tx)
        .await?;

        if updated.rows_affected() == 0 {
            sqlx::query(
                r#"
                insert into task_attempts (
                  id, task_id, attempt_no, node_id, worker_kind, status,
                  pid, zlm_key, zlm_schema, zlm_vhost, zlm_app, zlm_stream,
                  rtp_port, exit_code, failure_code, failure_reason,
                  checkpoint_json, started_at, ended_at, created_at
                ) values (
                  $1, $2, $3, $4, $5::worker_kind, 'PENDING'::attempt_status,
                  null, null, null, null, null, null,
                  null, null, null, null,
                  null, null, null, $6
                )
                "#,
            )
            .bind(attempt_id)
            .bind(task_id)
            .bind(attempt_no)
            .bind(node_id)
            .bind(worker_kind.as_str())
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

    pub async fn build_stop_command(
        &self,
        task_id: Uuid,
        reason: impl Into<String>,
        grace_period_sec: u32,
        force_after_sec: u32,
    ) -> Result<Option<StopCommand>, RepoError> {
        let task = self.fetch_task_summary(task_id).await?;
        let Some(node_id) = task.assigned_node_id else {
            return Ok(None);
        };
        if task.current_attempt_no <= 0 {
            return Ok(None);
        }

        Ok(Some(StopCommand {
            task_id,
            attempt_no: task.current_attempt_no,
            node_id,
            reason: reason.into(),
            grace_period_sec,
            force_after_sec,
        }))
    }

    pub async fn upsert_node_registration(
        &self,
        registration: &AgentRegistration,
        seen_at: DateTime<Utc>,
    ) -> Result<(), RepoError> {
        sqlx::query(
            r#"
            insert into media_nodes (
              id, node_name, hostname, labels, zlm_api_base, agent_stream_addr,
              network_mode, interfaces, healthy, last_seen_at, created_at, updated_at
            ) values (
              $1, $2, $3, $4, $5, $6,
              $7, $8, true, $9, $10, $10
            )
            on conflict (id) do update
               set node_name = excluded.node_name,
                   hostname = excluded.hostname,
                   labels = excluded.labels,
                   zlm_api_base = excluded.zlm_api_base,
                   agent_stream_addr = excluded.agent_stream_addr,
                   network_mode = excluded.network_mode,
                   interfaces = excluded.interfaces,
                   healthy = true,
                   last_seen_at = excluded.last_seen_at,
                   updated_at = excluded.updated_at
            "#,
        )
        .bind(registration.node_id)
        .bind(&registration.node_name)
        .bind(&registration.hostname)
        .bind(serde_json::to_value(&registration.labels)?)
        .bind(&registration.zlm_api_base)
        .bind(&registration.agent_stream_addr)
        .bind(registration.network_mode.as_str())
        .bind(serde_json::to_value(&registration.interfaces)?)
        .bind(seen_at)
        .bind(Utc::now())
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn record_node_heartbeat(
        &self,
        node_id: Uuid,
        heartbeat: &HeartbeatSnapshot,
    ) -> Result<(), RepoError> {
        let result = sqlx::query(
            r#"
            update media_nodes
               set healthy = true,
                   last_seen_at = $1,
                   updated_at = $2
             where id = $3
            "#,
        )
        .bind(heartbeat.node_time)
        .bind(Utc::now())
        .bind(node_id)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Err(RepoError::NodeNotFound(node_id));
        }

        Ok(())
    }

    pub async fn update_node_health(
        &self,
        node_id: Uuid,
        healthy: bool,
        last_seen_at: Option<DateTime<Utc>>,
    ) -> Result<(), RepoError> {
        let result = sqlx::query(
            r#"
            update media_nodes
               set healthy = $1,
                   last_seen_at = coalesce($2, last_seen_at),
                   updated_at = $3
             where id = $4
            "#,
        )
        .bind(healthy)
        .bind(last_seen_at)
        .bind(Utc::now())
        .bind(node_id)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Err(RepoError::NodeNotFound(node_id));
        }

        Ok(())
    }

    pub async fn upsert_node_capabilities(
        &self,
        node_id: Uuid,
        snapshot: &CapabilitySnapshot,
    ) -> Result<(), RepoError> {
        let result = sqlx::query(
            r#"
            insert into node_capabilities (
              node_id, ffmpeg_protocols, ffmpeg_formats, ffmpeg_encoders,
              ffmpeg_decoders, zlm_api_list, zlm_version, gpu, captured_at
            ) values (
              $1, $2, $3, $4,
              $5, $6, $7, $8, $9
            )
            on conflict (node_id) do update
               set ffmpeg_protocols = excluded.ffmpeg_protocols,
                   ffmpeg_formats = excluded.ffmpeg_formats,
                   ffmpeg_encoders = excluded.ffmpeg_encoders,
                   ffmpeg_decoders = excluded.ffmpeg_decoders,
                   zlm_api_list = excluded.zlm_api_list,
                   zlm_version = excluded.zlm_version,
                   gpu = excluded.gpu,
                   captured_at = excluded.captured_at
            "#,
        )
        .bind(node_id)
        .bind(serde_json::to_value(&snapshot.ffmpeg_protocols)?)
        .bind(serde_json::to_value(&snapshot.ffmpeg_formats)?)
        .bind(serde_json::to_value(&snapshot.ffmpeg_encoders)?)
        .bind(serde_json::to_value(&snapshot.ffmpeg_decoders)?)
        .bind(serde_json::to_value(&snapshot.zlm_api_list)?)
        .bind(snapshot.zlm_version.as_deref())
        .bind(serde_json::to_value(&snapshot.gpu)?)
        .bind(snapshot.captured_at)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Err(RepoError::NodeNotFound(node_id));
        }

        Ok(())
    }

    pub async fn record_agent_task_event(
        &self,
        node_id: Uuid,
        event: AgentTaskEventRecord,
    ) -> Result<(), RepoError> {
        let mut tx = self.pool.begin().await?;
        let now = Utc::now();

        self.insert_event(
            &mut tx,
            event.task_id,
            None,
            Some(event.attempt_no),
            EventSource::Agent,
            &event.event_type,
            &normalize_event_level(&event.event_level),
            json!({
                "node_id": node_id,
                "message": event.message,
                "payload": event.payload,
            }),
        )
        .await?;

        match event.event_type.as_str() {
            "accepted" | "starting" => {
                sqlx::query(
                    r#"
                    update tasks
                       set status = 'STARTING'::task_status,
                           assigned_node_id = $1,
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
            "recovering" => {
                sqlx::query(
                    r#"
                    update tasks
                       set status = 'RECOVERING'::task_status,
                           assigned_node_id = $1,
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
            "running" => {
                sqlx::query(
                    r#"
                    update tasks
                       set status = 'RUNNING'::task_status,
                           assigned_node_id = $1,
                           started_at = coalesce(started_at, $2),
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
                       set status = 'RUNNING'::attempt_status,
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
            "stopping" => {
                sqlx::query(
                    r#"
                    update tasks
                       set status = 'STOPPING'::task_status,
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
            "rejected" => {
                sqlx::query(
                    r#"
                    update tasks
                       set status = 'QUEUED'::task_status,
                           assigned_node_id = null,
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
                       set status = 'FAILED'::attempt_status,
                           node_id = $1,
                           failure_code = 'agent_rejected',
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
            }
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
            "failed" => {
                self.complete_task_attempt(
                    &mut tx,
                    event.task_id,
                    event.attempt_no,
                    node_id,
                    TaskStatus::Failed,
                    AttemptStatus::Failed,
                    Some("agent_failed"),
                    Some(event.message.as_str()),
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
        Ok(())
    }

    pub async fn record_agent_log_batch(
        &self,
        node_id: Uuid,
        batch: TaskLogBatchRecord,
    ) -> Result<(), RepoError> {
        let mut tx = self.pool.begin().await?;
        self.insert_event(
            &mut tx,
            batch.task_id,
            None,
            Some(batch.attempt_no),
            EventSource::Agent,
            "task_log_batch",
            "info",
            json!({
                "node_id": node_id,
                "stream": batch.stream,
                "lines": batch.lines,
            }),
        )
        .await?;
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
        let payload = json!({
            "node_id": node_id,
            "frame": progress.frame,
            "fps": progress.fps,
            "bitrate_kbps": progress.bitrate_kbps,
            "speed": progress.speed,
            "out_time_ms": progress.out_time_ms,
            "dup_frames": progress.dup_frames,
            "drop_frames": progress.drop_frames,
        });

        self.insert_event(
            &mut tx,
            progress.task_id,
            None,
            Some(progress.attempt_no),
            EventSource::Agent,
            "task_progress",
            "info",
            payload.clone(),
        )
        .await?;

        sqlx::query(
            r#"
            update tasks
               set status = 'RUNNING'::task_status,
                   assigned_node_id = $1,
                   started_at = coalesce(started_at, $2),
                   updated_at = $2
             where id = $3
            "#,
        )
        .bind(node_id)
        .bind(now)
        .bind(progress.task_id)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r#"
            update task_attempts
               set status = 'RUNNING'::attempt_status,
                   node_id = $1,
                   started_at = coalesce(started_at, $2),
                   checkpoint_json = $3
             where task_id = $4
               and attempt_no = $5
            "#,
        )
        .bind(node_id)
        .bind(now)
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
        self.insert_event(
            &mut tx,
            snapshot.task_id,
            None,
            Some(snapshot.attempt_no),
            EventSource::Agent,
            "task_snapshot",
            "info",
            json!({
                "node_id": node_id,
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

        self.upsert_stream_binding_from_snapshot(&mut tx, &snapshot)
            .await?;

        tx.commit().await?;
        Ok(())
    }

    async fn upsert_stream_binding_from_snapshot(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
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

        sqlx::query(
            r#"
            insert into stream_bindings (
              id, task_id, attempt_id, schema, vhost, app, stream, zlm_proxy_key, zlm_pusher_key, rtp_stream_id
            )
            values ($1, $2, $3, $4, $5, $6, $7, $8, null, $9)
            on conflict (schema, vhost, app, stream) do update
              set task_id = excluded.task_id,
                  attempt_id = excluded.attempt_id,
                  zlm_proxy_key = excluded.zlm_proxy_key,
                  rtp_stream_id = coalesce(excluded.rtp_stream_id, stream_bindings.rtp_stream_id)
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(snapshot.task_id)
        .bind(attempt_id)
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

    pub async fn resolve_node_id_by_server_id(
        &self,
        server_id: &str,
    ) -> Result<Option<Uuid>, RepoError> {
        let Ok(parsed_uuid) = Uuid::parse_str(server_id.trim()) else {
            return Ok(None);
        };
        let row = sqlx::query("select id from media_nodes where id = $1")
            .bind(parsed_uuid)
            .fetch_optional(&self.pool)
            .await?;

        row.map(|row| row.try_get("id"))
            .transpose()
            .map_err(RepoError::Sqlx)
    }

    pub async fn record_zlm_hook(
        &self,
        server_id: &str,
        hook_name: &str,
        dedup_key: &str,
        payload: Value,
    ) -> Result<bool, RepoError> {
        let mut tx = self.pool.begin().await?;
        let inserted = self
            .insert_hook_event(&mut tx, server_id, hook_name, dedup_key, payload)
            .await?;
        if inserted {
            self.mark_hook_event_processed(&mut tx, dedup_key).await?;
        }
        tx.commit().await?;
        Ok(inserted)
    }

    pub async fn record_zlm_record_file_hook(
        &self,
        server_id: &str,
        hook_name: &str,
        dedup_key: &str,
        payload: Value,
        record: ZlmRecordFileRecord,
    ) -> Result<bool, RepoError> {
        let mut tx = self.pool.begin().await?;
        let inserted = self
            .insert_hook_event(&mut tx, server_id, hook_name, dedup_key, payload)
            .await?;
        if !inserted {
            tx.commit().await?;
            return Ok(false);
        }

        if let Some(binding) = self
            .find_stream_binding_for_hook(&mut tx, &record.vhost, &record.app, &record.stream)
            .await?
        {
            sqlx::query(
                r#"
                insert into record_files (
                  id, task_id, attempt_id, vhost, app, stream, file_path, file_size,
                  time_len, start_time, source, created_at
                ) values (
                  $1, $2, $3, $4, $5, $6, $7, $8,
                  $9, $10, 'hook', $11
                )
                on conflict (file_path) do update
                   set task_id = excluded.task_id,
                       attempt_id = excluded.attempt_id,
                       vhost = excluded.vhost,
                       app = excluded.app,
                       stream = excluded.stream,
                       file_size = excluded.file_size,
                       time_len = excluded.time_len,
                       start_time = excluded.start_time,
                       source = excluded.source
                "#,
            )
            .bind(Uuid::now_v7())
            .bind(binding.task_id)
            .bind(binding.attempt_id)
            .bind(&record.vhost)
            .bind(&record.app)
            .bind(&record.stream)
            .bind(&record.file_path)
            .bind(record.file_size)
            .bind(record.time_len_sec)
            .bind(record.start_time)
            .bind(Utc::now())
            .execute(&mut *tx)
            .await?;

            self.insert_event(
                &mut tx,
                binding.task_id,
                Some(binding.attempt_id),
                Some(binding.attempt_no),
                EventSource::ZlmHook,
                "record_file_created",
                "info",
                json!({
                    "server_id": server_id,
                    "hook_name": hook_name,
                    "record_format": record.record_format,
                    "schema": record.schema,
                    "vhost": record.vhost,
                    "app": record.app,
                    "stream": record.stream,
                    "file_path": record.file_path,
                    "file_name": record.file_name,
                    "folder": record.folder,
                    "url": record.url,
                    "file_size": record.file_size,
                    "time_len": record.time_len_sec,
                    "start_time": record.start_time,
                    "source": "hook",
                }),
            )
            .await?;
        }

        self.mark_hook_event_processed(&mut tx, dedup_key).await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn record_zlm_stream_event_hook(
        &self,
        server_id: &str,
        hook_name: &str,
        dedup_key: &str,
        payload: Value,
        record: ZlmStreamEventRecord,
    ) -> Result<bool, RepoError> {
        let mut tx = self.pool.begin().await?;
        let inserted = self
            .insert_hook_event(&mut tx, server_id, hook_name, dedup_key, payload)
            .await?;
        if !inserted {
            tx.commit().await?;
            return Ok(false);
        }

        if let Some(binding) = self
            .find_stream_binding_for_hook(&mut tx, &record.vhost, &record.app, &record.stream)
            .await?
        {
            self.insert_event(
                &mut tx,
                binding.task_id,
                Some(binding.attempt_id),
                Some(binding.attempt_no),
                EventSource::ZlmHook,
                &record.event_type,
                &record.event_level,
                json!({
                    "server_id": server_id,
                    "hook_name": hook_name,
                    "schema": record.schema,
                    "vhost": record.vhost,
                    "app": record.app,
                    "stream": record.stream,
                    "payload": record.payload,
                }),
            )
            .await?;
        }

        self.mark_hook_event_processed(&mut tx, dedup_key).await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn record_zlm_publish_hook(
        &self,
        server_id: &str,
        hook_name: &str,
        dedup_key: &str,
        node_id: Uuid,
        payload: Value,
        record: ZlmPublishTaskRecord,
    ) -> Result<bool, RepoError> {
        let mut tx = self.pool.begin().await?;
        let inserted = self
            .insert_hook_event(&mut tx, server_id, hook_name, dedup_key, payload)
            .await?;
        if !inserted {
            tx.commit().await?;
            return Ok(false);
        }

        self.insert_event(
            &mut tx,
            record.task_id,
            record.attempt_id,
            Some(record.attempt_no),
            EventSource::ZlmHook,
            "stream_publish_requested",
            "info",
            json!({
                "server_id": server_id,
                "hook_name": hook_name,
                "payload": record.event_payload,
            }),
        )
        .await?;

        if let Some(attempt_id) = record.attempt_id {
            sqlx::query(
                r#"
                insert into stream_bindings (
                  id, task_id, attempt_id, schema, vhost, app, stream, zlm_proxy_key, zlm_pusher_key, rtp_stream_id
                )
                values ($1, $2, $3, $4, $5, $6, $7, null, null, $8)
                on conflict (schema, vhost, app, stream) do update
                  set task_id = excluded.task_id,
                      attempt_id = excluded.attempt_id,
                      rtp_stream_id = coalesce(excluded.rtp_stream_id, stream_bindings.rtp_stream_id)
                "#,
            )
            .bind(Uuid::now_v7())
            .bind(record.task_id)
            .bind(attempt_id)
            .bind(&record.schema)
            .bind(&record.vhost)
            .bind(&record.app)
            .bind(&record.stream)
            .bind(record.rtp_stream_id)
            .execute(&mut *tx)
            .await?;
        }

        if record.promote_running {
            self.promote_task_running(
                &mut tx,
                record.task_id,
                record.attempt_no,
                node_id,
                Utc::now(),
            )
            .await?;
        }

        self.mark_hook_event_processed(&mut tx, dedup_key).await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn record_zlm_lost_task_event_hook(
        &self,
        server_id: &str,
        hook_name: &str,
        dedup_key: &str,
        node_id: Uuid,
        payload: Value,
        record: ZlmTaskEventHookRecord,
        failure_code: &str,
        failure_reason: &str,
    ) -> Result<bool, RepoError> {
        let mut tx = self.pool.begin().await?;
        let inserted = self
            .insert_hook_event(&mut tx, server_id, hook_name, dedup_key, payload)
            .await?;
        if !inserted {
            tx.commit().await?;
            return Ok(false);
        }

        self.insert_event(
            &mut tx,
            record.task_id,
            record.attempt_id,
            record.attempt_no,
            EventSource::ZlmHook,
            &record.event_type,
            &record.event_level,
            json!({
                "server_id": server_id,
                "hook_name": hook_name,
                "payload": record.payload,
            }),
        )
        .await?;

        if let Some(attempt_no) = record.attempt_no {
            self.mark_task_lost(
                &mut tx,
                record.task_id,
                attempt_no,
                node_id,
                failure_code,
                failure_reason,
                Utc::now(),
            )
            .await?;
        }

        self.mark_hook_event_processed(&mut tx, dedup_key).await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn find_task_for_publish_stream(
        &self,
        node_id: Uuid,
        app: &str,
        stream: &str,
    ) -> Result<Option<PublishTaskTarget>, RepoError> {
        let rows = sqlx::query(
            r#"
            select
              t.id,
              t.current_attempt_no,
              ta.id as attempt_id,
              t.resolved_spec
            from tasks t
            left join task_attempts ta
              on ta.task_id = t.id
             and ta.attempt_no = t.current_attempt_no
            where t.assigned_node_id = $1
              and t.current_attempt_no > 0
              and t.status in ('DISPATCHING', 'STARTING', 'RUNNING', 'RECOVERING')
            order by t.updated_at desc
            "#,
        )
        .bind(node_id)
        .fetch_all(&self.pool)
        .await?;

        for row in rows {
            let task_id: Uuid = row.try_get("id")?;
            let attempt_no: i32 = row.try_get("current_attempt_no")?;
            let resolved_spec: Option<Value> = row.try_get("resolved_spec")?;
            let Some(resolved_spec) = resolved_spec else {
                continue;
            };
            let spec = serde_json::from_value::<TaskSpec>(resolved_spec.clone())?;
            if publish_stream_matches(&spec, app, stream)
                || rtp_stream_matches(task_id, attempt_no, &spec, stream)
            {
                return Ok(Some(PublishTaskTarget {
                    task_id,
                    attempt_id: row.try_get("attempt_id")?,
                    attempt_no,
                    resolved_spec,
                }));
            }
        }

        Ok(None)
    }

    pub async fn find_task_for_rtp_stream(
        &self,
        node_id: Uuid,
        stream_id: &str,
    ) -> Result<Option<PublishTaskTarget>, RepoError> {
        let rows = sqlx::query(
            r#"
            select
              t.id,
              t.current_attempt_no,
              ta.id as attempt_id,
              t.resolved_spec
            from tasks t
            left join task_attempts ta
              on ta.task_id = t.id
             and ta.attempt_no = t.current_attempt_no
            where t.assigned_node_id = $1
              and t.current_attempt_no > 0
              and t.status in ('DISPATCHING', 'STARTING', 'RUNNING', 'RECOVERING')
            order by t.updated_at desc
            "#,
        )
        .bind(node_id)
        .fetch_all(&self.pool)
        .await?;

        for row in rows {
            let task_id: Uuid = row.try_get("id")?;
            let attempt_no: i32 = row.try_get("current_attempt_no")?;
            let resolved_spec: Option<Value> = row.try_get("resolved_spec")?;
            let Some(resolved_spec) = resolved_spec else {
                continue;
            };
            let spec = serde_json::from_value::<TaskSpec>(resolved_spec.clone())?;
            if rtp_stream_matches(task_id, attempt_no, &spec, stream_id) {
                return Ok(Some(PublishTaskTarget {
                    task_id,
                    attempt_id: row.try_get("attempt_id")?,
                    attempt_no,
                    resolved_spec,
                }));
            }
        }

        Ok(None)
    }

    async fn fetch_task_summary(&self, task_id: Uuid) -> Result<TaskSummary, RepoError> {
        sqlx::query(
            r#"
            select
              id,
              tenant_id,
              name,
              type::text as task_type,
              status::text as status,
              template_id,
              profile,
              priority,
              assigned_node_id,
              current_attempt_no,
              created_at,
              updated_at,
              started_at,
              finished_at
            from tasks
            where id = $1
            "#,
        )
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await?
        .map(|row| TaskSummary::from_row(&row))
        .transpose()?
        .ok_or(RepoError::TaskNotFound(task_id))
    }

    async fn count_tasks(&self, filter: &TaskListFilter) -> Result<u64, RepoError> {
        let mut builder =
            QueryBuilder::<Postgres>::new("select count(*) as total from tasks where 1 = 1");
        self.apply_filters(&mut builder, filter);

        let row = builder.build().fetch_one(&self.pool).await?;
        let total: i64 = row.try_get("total")?;
        Ok(total as u64)
    }

    fn apply_filters<'a>(
        &self,
        builder: &mut QueryBuilder<'a, Postgres>,
        filter: &'a TaskListFilter,
    ) {
        if let Some(status) = filter.status {
            builder.push(" and status = ");
            builder.push_bind(status.as_str());
            builder.push("::task_status");
        }

        if let Some(task_type) = filter.task_type {
            builder.push(" and type = ");
            builder.push_bind(task_type.as_str());
            builder.push("::task_type");
        }

        if let Some(tenant_id) = filter.tenant_id.as_deref() {
            builder.push(" and tenant_id = ");
            builder.push_bind(tenant_id);
        }

        if let Some(assigned_node_id) = filter.assigned_node_id {
            builder.push(" and assigned_node_id = ");
            builder.push_bind(assigned_node_id);
        }

        if let Some(keyword) = filter.keyword.as_deref() {
            builder.push(" and name ilike ");
            builder.push_bind(format!("%{keyword}%"));
        }

        if let Some(created_from) = filter.created_from {
            builder.push(" and created_at >= ");
            builder.push_bind(created_from);
        }

        if let Some(created_to) = filter.created_to {
            builder.push(" and created_at <= ");
            builder.push_bind(created_to);
        }
    }

    async fn insert_event(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        task_id: Uuid,
        attempt_id: Option<Uuid>,
        attempt_no: Option<i32>,
        source: EventSource,
        event_type: &str,
        event_level: &str,
        payload: Value,
    ) -> Result<(), RepoError> {
        sqlx::query(
            r#"
            insert into task_events (
              id, task_id, attempt_id, attempt_no, source, event_type, event_level,
              dedup_key, payload, created_at
            ) values (
              $1, $2, $3, $4, $5::event_source, $6, $7,
              null, $8, $9
            )
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(task_id)
        .bind(attempt_id)
        .bind(attempt_no)
        .bind(source.as_str())
        .bind(event_type)
        .bind(event_level)
        .bind(payload)
        .bind(Utc::now())
        .execute(&mut **tx)
        .await?;

        Ok(())
    }

    async fn insert_hook_event(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        server_id: &str,
        hook_name: &str,
        dedup_key: &str,
        payload: Value,
    ) -> Result<bool, RepoError> {
        let result = sqlx::query(
            r#"
            insert into hook_events (
              id, server_id, hook_name, dedup_key, payload, received_at, processed_at
            ) values (
              $1, $2, $3, $4, $5, $6, null
            )
            on conflict (dedup_key) do nothing
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(server_id)
        .bind(hook_name)
        .bind(dedup_key)
        .bind(payload)
        .bind(Utc::now())
        .execute(&mut **tx)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    async fn mark_hook_event_processed(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        dedup_key: &str,
    ) -> Result<(), RepoError> {
        sqlx::query(
            r#"
            update hook_events
               set processed_at = coalesce(processed_at, $1)
             where dedup_key = $2
            "#,
        )
        .bind(Utc::now())
        .bind(dedup_key)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    async fn find_stream_binding_for_hook(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        vhost: &str,
        app: &str,
        stream: &str,
    ) -> Result<Option<HookStreamBinding>, RepoError> {
        sqlx::query(
            r#"
            select
              sb.task_id,
              sb.attempt_id,
              ta.attempt_no
            from stream_bindings sb
            join task_attempts ta
              on ta.id = sb.attempt_id
            where sb.vhost = $1
              and sb.app = $2
              and sb.stream = $3
            order by sb.created_at desc
            limit 1
            "#,
        )
        .bind(vhost)
        .bind(app)
        .bind(stream)
        .fetch_optional(&mut **tx)
        .await?
        .map(|row| HookStreamBinding::from_row(&row))
        .transpose()
    }

    async fn delete_task_lease(
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

    async fn complete_task_attempt(
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
                   updated_at = $2,
                   finished_at = $3
             where id = $4
            "#,
        )
        .bind(task_status.as_str())
        .bind(now)
        .bind(now)
        .bind(task_id)
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
        Ok(())
    }

    async fn promote_task_running(
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
                   started_at = coalesce(started_at, $2),
                   updated_at = $2
             where id = $3
               and status in ('DISPATCHING', 'STARTING', 'RUNNING', 'RECOVERING')
            "#,
        )
        .bind(node_id)
        .bind(now)
        .bind(task_id)
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

        Ok(())
    }

    async fn mark_task_lost(
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
                   updated_at = $1,
                   finished_at = $1
             where id = $2
               and status in ('DISPATCHING', 'STARTING', 'RUNNING', 'RECOVERING')
            "#,
        )
        .bind(now)
        .bind(task_id)
        .execute(&mut **tx)
        .await?;

        if updated.rows_affected() == 0 {
            return Ok(());
        }

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
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CreateTaskResult {
    Fresh(TaskSummary),
    Replay(TaskSummary),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskCloneOverride {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub priority: Option<u8>,
    #[serde(default)]
    pub profile: Option<String>,
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
    pub reason: String,
    pub grace_period_sec: u32,
    pub force_after_sec: u32,
}

#[derive(Debug, Clone)]
pub struct AgentTaskEventRecord {
    pub task_id: Uuid,
    pub attempt_no: i32,
    pub event_type: String,
    pub event_level: String,
    pub message: String,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub struct TaskLogBatchRecord {
    pub task_id: Uuid,
    pub attempt_no: i32,
    pub stream: String,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct TaskProgressRecord {
    pub task_id: Uuid,
    pub attempt_no: i32,
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
    pub worker_kind: String,
    pub pid: Option<i32>,
    pub state: String,
    pub command_line: Option<String>,
    pub outputs: Vec<String>,
    pub metadata: Value,
}

#[derive(Debug, Clone)]
pub struct ZlmRecordFileRecord {
    pub record_format: Option<String>,
    pub schema: Option<String>,
    pub vhost: String,
    pub app: String,
    pub stream: String,
    pub file_path: String,
    pub file_size: i64,
    pub time_len_sec: Option<i32>,
    pub start_time: Option<DateTime<Utc>>,
    pub file_name: Option<String>,
    pub folder: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ZlmStreamEventRecord {
    pub schema: Option<String>,
    pub vhost: String,
    pub app: String,
    pub stream: String,
    pub event_type: String,
    pub event_level: String,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub struct ZlmTaskEventHookRecord {
    pub task_id: Uuid,
    pub attempt_id: Option<Uuid>,
    pub attempt_no: Option<i32>,
    pub event_type: String,
    pub event_level: String,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub struct ZlmPublishTaskRecord {
    pub task_id: Uuid,
    pub attempt_id: Option<Uuid>,
    pub attempt_no: i32,
    pub schema: String,
    pub vhost: String,
    pub app: String,
    pub stream: String,
    pub rtp_stream_id: Option<String>,
    pub promote_running: bool,
    pub event_payload: Value,
}

#[derive(Debug, Clone)]
pub struct PublishTaskTarget {
    pub task_id: Uuid,
    pub attempt_id: Option<Uuid>,
    pub attempt_no: i32,
    pub resolved_spec: Value,
}

#[derive(Debug, Clone, Deserialize)]
struct StreamBindingSnapshot {
    #[serde(default)]
    schema: Option<String>,
    vhost: String,
    app: String,
    stream: String,
}

#[derive(Debug, Clone)]
struct HookStreamBinding {
    task_id: Uuid,
    attempt_id: Uuid,
    attempt_no: i32,
}

impl HookStreamBinding {
    fn from_row(row: &PgRow) -> Result<Self, RepoError> {
        Ok(Self {
            task_id: row.try_get("task_id")?,
            attempt_id: row.try_get("attempt_id")?,
            attempt_no: row.try_get("attempt_no")?,
        })
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TaskListFilter {
    #[serde(default)]
    pub status: Option<TaskStatus>,
    #[serde(default, rename = "type")]
    pub task_type: Option<TaskType>,
    #[serde(default)]
    pub tenant_id: Option<String>,
    #[serde(default)]
    pub assigned_node_id: Option<Uuid>,
    #[serde(default)]
    pub keyword: Option<String>,
    #[serde(default)]
    pub created_from: Option<DateTime<Utc>>,
    #[serde(default)]
    pub created_to: Option<DateTime<Utc>>,
    #[serde(default)]
    pub page: Option<u32>,
    #[serde(default)]
    pub page_size: Option<u32>,
    #[serde(default)]
    pub sort_by: Option<String>,
    #[serde(default)]
    pub sort_order: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: Uuid,
    pub tenant_id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub task_type: TaskType,
    pub status: TaskStatus,
    pub template_id: Option<Uuid>,
    pub profile: Option<String>,
    pub priority: u8,
    pub assigned_node_id: Option<Uuid>,
    pub current_attempt_no: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl TaskSummary {
    fn current_attempt_no_value(&self) -> Option<i32> {
        (self.current_attempt_no > 0).then_some(self.current_attempt_no)
    }

    fn from_row(row: &PgRow) -> Result<Self, RepoError> {
        let task_type = TaskType::from_str(row.try_get::<&str, _>("task_type")?)
            .map_err(RepoError::ParseEnum)?;
        let status = TaskStatus::from_str(row.try_get::<&str, _>("status")?)
            .map_err(RepoError::ParseEnum)?;
        let priority = row.try_get::<i32, _>("priority")?;

        Ok(Self {
            id: row.try_get("id")?,
            tenant_id: row.try_get("tenant_id")?,
            name: row.try_get("name")?,
            task_type,
            status,
            template_id: row.try_get("template_id")?,
            profile: row.try_get("profile")?,
            priority: u8::try_from(priority).unwrap_or(50),
            assigned_node_id: row.try_get("assigned_node_id")?,
            current_attempt_no: row.try_get("current_attempt_no")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
            started_at: row.try_get("started_at")?,
            finished_at: row.try_get("finished_at")?,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskDetail {
    pub task: TaskSummary,
    pub requested_spec: Value,
    pub resolved_spec: Option<Value>,
    pub current_attempt: Option<AttemptSummary>,
    pub recent_events: Vec<TaskEventSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AttemptSummary {
    pub id: Uuid,
    pub attempt_no: i32,
    pub worker_kind: WorkerKind,
    pub status: AttemptStatus,
    pub node_id: Option<Uuid>,
    pub pid: Option<i32>,
    pub exit_code: Option<i32>,
    pub failure_code: Option<String>,
    pub failure_reason: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
}

impl AttemptSummary {
    fn from_row(row: &PgRow) -> Result<Self, RepoError> {
        let worker_kind = WorkerKind::from_str(row.try_get::<&str, _>("worker_kind")?)
            .map_err(RepoError::ParseEnum)?;
        let status = AttemptStatus::from_str(row.try_get::<&str, _>("status")?)
            .map_err(RepoError::ParseEnum)?;

        Ok(Self {
            id: row.try_get("id")?,
            attempt_no: row.try_get("attempt_no")?,
            worker_kind,
            status,
            node_id: row.try_get("node_id")?,
            pid: row.try_get("pid")?,
            exit_code: row.try_get("exit_code")?,
            failure_code: row.try_get("failure_code")?,
            failure_reason: row.try_get("failure_reason")?,
            started_at: row.try_get("started_at")?,
            ended_at: row.try_get("ended_at")?,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskEventSummary {
    pub id: Uuid,
    pub attempt_no: Option<i32>,
    pub source: EventSource,
    pub event_type: String,
    pub event_level: String,
    pub payload: Value,
    pub created_at: DateTime<Utc>,
}

impl TaskEventSummary {
    fn from_row(row: &PgRow) -> Result<Self, RepoError> {
        let source = EventSource::from_str(row.try_get::<&str, _>("source")?)
            .map_err(RepoError::ParseEnum)?;

        Ok(Self {
            id: row.try_get("id")?,
            attempt_no: row.try_get("attempt_no")?,
            source,
            event_type: row.try_get("event_type")?,
            event_level: row.try_get("event_level")?,
            payload: row.try_get("payload")?,
            created_at: row.try_get("created_at")?,
        })
    }
}

#[derive(Debug, sqlx::FromRow)]
struct OperationRequestRow {
    request_hash: String,
    response_body: Option<Value>,
}

#[derive(Debug, Error)]
pub enum RepoError {
    #[error("task {0} was not found")]
    TaskNotFound(Uuid),
    #[error("node {0} was not found")]
    NodeNotFound(Uuid),
    #[error("task {0} is missing resolved_spec")]
    TaskMissingResolvedSpec(Uuid),
    #[error("task is not dispatchable from status {0}")]
    TaskNotDispatchable(TaskStatus),
    #[error("idempotency key already exists with different request body")]
    IdempotencyConflict,
    #[error("operation with the same idempotency key is still in progress")]
    OperationInProgress,
    #[error(transparent)]
    TaskState(#[from] TaskStateError),
    #[error(transparent)]
    Validation(#[from] media_domain::TaskValidationError),
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
    #[error(transparent)]
    ParseEnum(#[from] media_domain::task::ParseEnumError),
}

fn sort_by_clause(value: Option<&str>) -> &'static str {
    match value.unwrap_or("created_at") {
        "updated_at" => "updated_at",
        "priority" => "priority",
        "status" => "status",
        "type" => "type",
        _ => "created_at",
    }
}

fn sort_order_clause(value: Option<&str>) -> &'static str {
    match value.unwrap_or("desc") {
        "asc" => "asc",
        _ => "desc",
    }
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
    if let Some(profile) = overrides.profile {
        spec.profile = Some(profile);
    }
    if let Some(created_by) = overrides.common.created_by {
        spec.common.created_by = Some(created_by);
    }
    if let Some(start_mode) = overrides.schedule.start_mode {
        spec.schedule.start_mode = Some(start_mode);
    }
}

fn normalize_event_level(value: &str) -> String {
    match value.trim() {
        "debug" | "info" | "warn" | "error" => value.trim().to_string(),
        _ => "info".to_string(),
    }
}

fn publish_stream_matches(spec: &TaskSpec, app: &str, stream: &str) -> bool {
    let Some(url) = spec.publish.url.as_deref() else {
        return false;
    };
    let Some((publish_app, publish_stream)) = parse_publish_stream_url(url) else {
        return false;
    };
    publish_app == app && publish_stream == stream
}

fn rtp_stream_matches(task_id: Uuid, attempt_no: i32, spec: &TaskSpec, stream_id: &str) -> bool {
    spec.task_type == TaskType::RtpReceive && build_rtp_stream_id(task_id, attempt_no) == stream_id
}

fn parse_publish_stream_url(value: &str) -> Option<(String, String)> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    let remainder = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed);
    let path = remainder.split_once('/').map(|(_, path)| path)?;
    let path = path.split('?').next().unwrap_or(path).trim_matches('/');
    let mut segments = path.split('/');
    let app = segments.next()?.trim();
    let stream = segments.next()?.trim();
    if app.is_empty() || stream.is_empty() {
        None
    } else {
        Some((app.to_string(), stream.to_string()))
    }
}

fn build_rtp_stream_id(task_id: Uuid, attempt_no: i32) -> String {
    format!("{task_id}-{attempt_no}")
}
