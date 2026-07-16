//! 任务基础仓储：负责创建、预览、列表、详情、删除和 resolved_spec 查询等任务基础操作。

use chrono::Utc;
use media_domain::{EventSource, Page, TaskSpec, TaskStatus};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder, Row};
use uuid::Uuid;

use super::{
    AttemptSummary, CallbackDeliverySummary, RepoError, TaskDetail, TaskEventSummary,
    TaskListFilter, TaskRepository, TaskSummary, task_summary_transcode_mode,
};

impl TaskRepository {
    pub async fn create_task(
        &self,
        idempotency_key: &str,
        request_hash: &str,
        requested_spec: TaskSpec,
    ) -> Result<CreateTaskResult, RepoError> {
        let resolved = self.resolve_requested_task(&requested_spec).await?;
        let requested_spec = resolved.requested_spec;
        let resolved_spec = resolved.resolved_spec;

        let created_by = resolved_spec.created_by().unwrap_or("system").to_string();
        let status = resolved_spec.initial_status();
        let task_id = Uuid::now_v7();
        let created_at = Utc::now();
        let updated_at = created_at;
        let summary = TaskSummary {
            id: task_id,
            name: resolved_spec.name.clone(),
            task_type: resolved_spec.task_type,
            status,
            priority: resolved_spec.priority,
            created_by: created_by.clone(),
            assigned_node_id: None,
            current_attempt_no: 0,
            created_at,
            updated_at,
            started_at: None,
            finished_at: None,
            transcode_mode: task_summary_transcode_mode(&resolved_spec).map(str::to_string),
        };

        let mut tx = self.pool.begin().await?;

        if let Some(existing) = sqlx::query_as::<_, OperationRequestRow>(
            r#"
            select request_hash, response_body
              from operation_requests
             where operation_key = $1
               and method = 'POST'
               and path = '/api/v1/tasks'
            "#,
        )
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
              id, operation_key, method, path, request_hash, created_at
            ) values ($1, $2, 'POST', '/api/v1/tasks', $3, $4)
            "#,
        )
        .bind(Uuid::now_v7())
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
        .bind(task_id)
        .bind(&resolved_spec.name)
        .bind(resolved_spec.task_type.as_str())
        .bind(status.as_str())
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
             where operation_key = $3
               and method = 'POST'
               and path = '/api/v1/tasks'
            "#,
        )
        .bind(task_id)
        .bind(serde_json::to_value(&summary)?)
        .bind(idempotency_key)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(CreateTaskResult::Fresh(summary))
    }

    pub async fn preview_task_spec(
        &self,
        requested_spec: TaskSpec,
    ) -> Result<TaskPreview, RepoError> {
        let resolved = self.resolve_requested_task(&requested_spec).await?;
        Ok(TaskPreview {
            requested_spec: serde_json::to_value(&resolved.requested_spec)?,
            resolved_spec: serde_json::to_value(&resolved.resolved_spec)?,
        })
    }

    pub async fn list_tasks(&self, filter: TaskListFilter) -> Result<Page<TaskSummary>, RepoError> {
        let page = filter.page.unwrap_or(1).max(1);
        let page_size = filter.page_size.unwrap_or(20).clamp(1, 100);
        let total = self.count_tasks(&filter).await?;

        let mut builder = QueryBuilder::<Postgres>::new(
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
            where 1 = 1
            "#,
        );
        apply_task_filters(&mut builder, &filter);
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
              task_events.id,
              task_events.attempt_no,
              task_events.source::text as source,
              task_events.event_type,
              task_events.event_level,
              task_events.payload,
              task_events.created_at,
              n.output_mount_relative_prefix_mp4,
              n.output_mount_relative_prefix_hls
            from task_events
            left join task_attempts ta on ta.id = task_events.attempt_id
            left join media_nodes n on n.id = ta.node_id
            where task_events.task_id = $1
            order by task_events.created_at desc
            limit 20
            "#,
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| TaskEventSummary::from_row(&row))
        .collect::<Result<Vec<_>, _>>()?;

        let callback_delivery = sqlx::query(
            r#"
            select
              callback_url,
              event_type,
              reason,
              status,
              delivery_attempts,
              last_http_status,
              last_error,
              delivered_at,
              updated_at
            from task_callback_outbox
            where task_id = $1
            order by created_at desc, id desc
            limit 1
            "#,
        )
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await?
        .map(|row| CallbackDeliverySummary::from_row(&row))
        .transpose()?;

        let records = self.list_task_record_files(task_id).await?;
        let file_artifacts = self.list_task_file_artifacts(task_id).await?;

        Ok(TaskDetail {
            task,
            requested_spec,
            resolved_spec,
            current_attempt,
            recent_events,
            callback_delivery,
            records,
            file_artifacts,
        })
    }

    pub async fn get_task_attempt(
        &self,
        task_id: Uuid,
        attempt_no: i32,
    ) -> Result<Option<AttemptSummary>, RepoError> {
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
            where task_id = $1
              and attempt_no = $2
            "#,
        )
        .bind(task_id)
        .bind(attempt_no)
        .fetch_optional(&self.pool)
        .await?
        .map(|row| AttemptSummary::from_row(&row))
        .transpose()
    }

    pub async fn get_task_summary(&self, task_id: Uuid) -> Result<TaskSummary, RepoError> {
        self.fetch_task_summary(task_id).await
    }

    pub async fn prepare_task_delete(&self, task_id: Uuid) -> Result<TaskSummary, RepoError> {
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
        let task = TaskSummary::from_row(&row)?;
        let has_task_lease = sqlx::query_scalar::<_, bool>(
            "select exists(select 1 from task_leases where task_id = $1)",
        )
        .bind(task_id)
        .fetch_one(&mut *tx)
        .await?;
        let lost_delete_allowed =
            task.status == TaskStatus::Lost && task.assigned_node_id.is_none() && !has_task_lease;
        if !task_status_allows_delete(task.status) && !lost_delete_allowed {
            return Err(RepoError::TaskDeleteForbidden(task.status));
        }

        if matches!(
            task.status,
            TaskStatus::Created | TaskStatus::Validating | TaskStatus::Queued
        ) {
            let now = Utc::now();
            sqlx::query(
                r#"
                update tasks
                   set status = 'CANCELED'::task_status,
                       updated_at = $1,
                       finished_at = $1
                 where id = $2
                "#,
            )
            .bind(now)
            .bind(task_id)
            .execute(&mut *tx)
            .await?;
            self.insert_event(
                &mut tx,
                task_id,
                None,
                task.current_attempt_no_value(),
                EventSource::User,
                "task_delete_prepared",
                "info",
                json!({
                    "from": task.status,
                    "to": TaskStatus::Canceled,
                }),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(task)
    }

    pub async fn delete_task(&self, task_id: Uuid) -> Result<TaskSummary, RepoError> {
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
        let task = TaskSummary::from_row(&row)?;
        let has_task_lease = sqlx::query_scalar::<_, bool>(
            r#"
            select exists(select 1 from task_leases where task_id = $1)
            "#,
        )
        .bind(task_id)
        .fetch_one(&mut *tx)
        .await?;
        let lost_delete_allowed =
            task.status == TaskStatus::Lost && task.assigned_node_id.is_none() && !has_task_lease;

        if !task_status_allows_delete(task.status) && !lost_delete_allowed {
            return Err(RepoError::TaskDeleteForbidden(task.status));
        }

        sqlx::query("delete from task_checkpoints where task_id = $1")
            .bind(task_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("delete from stream_bindings where task_id = $1")
            .bind(task_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("delete from task_events where task_id = $1")
            .bind(task_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("delete from task_callback_outbox where task_id = $1")
            .bind(task_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("delete from record_files where task_id = $1")
            .bind(task_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("delete from transcode_artifacts where task_id = $1")
            .bind(task_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("delete from task_leases where task_id = $1")
            .bind(task_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("delete from task_attempts where task_id = $1")
            .bind(task_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("delete from tasks where id = $1")
            .bind(task_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        Ok(task)
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

    pub async fn update_queued_resolved_spec(
        &self,
        task_id: Uuid,
        resolved_spec: &TaskSpec,
    ) -> Result<(), RepoError> {
        let result = sqlx::query(
            r#"
            update tasks
               set resolved_spec = $2,
                   updated_at = $3
             where id = $1
               and status = 'QUEUED'::task_status
            "#,
        )
        .bind(task_id)
        .bind(serde_json::to_value(resolved_spec)?)
        .bind(Utc::now())
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 1 {
            Ok(())
        } else {
            Err(RepoError::TaskNotDispatchable(
                self.get_task_summary(task_id).await?.status,
            ))
        }
    }

    pub(super) async fn fetch_task_summary(&self, task_id: Uuid) -> Result<TaskSummary, RepoError> {
        sqlx::query(
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
        apply_task_filters(&mut builder, filter);

        let row = builder.build().fetch_one(&self.pool).await?;
        let total: i64 = row.try_get("total")?;
        Ok(total as u64)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskPreview {
    pub requested_spec: Value,
    pub resolved_spec: Value,
}

pub enum CreateTaskResult {
    Fresh(TaskSummary),
    Replay(TaskSummary),
}

#[derive(Debug, sqlx::FromRow)]
struct OperationRequestRow {
    request_hash: String,
    response_body: Option<Value>,
}

fn apply_task_filters<'a>(builder: &mut QueryBuilder<'a, Postgres>, filter: &'a TaskListFilter) {
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

fn task_status_allows_delete(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Created
            | TaskStatus::Validating
            | TaskStatus::Queued
            | TaskStatus::Succeeded
            | TaskStatus::Failed
            | TaskStatus::Canceled
    )
}
