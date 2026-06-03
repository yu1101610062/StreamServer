//! 调度计划仓储：负责到期任务、cron 计划、计划派生任务和计划状态事件。

use chrono::{DateTime, Utc};
use media_domain::{EventSource, StartMode, TaskSpec};
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;

use super::{RepoError, TaskRepository, TaskSummary, task_summary_transcode_mode};

impl TaskRepository {
    pub async fn list_due_at_tasks(&self, now: DateTime<Utc>) -> Result<Vec<Uuid>, RepoError> {
        Ok(sqlx::query_scalar(
            r#"
            select id
              from tasks
             where (
                    schedule_start_mode = 'immediate'
                and status = 'VALIDATING'::task_status
                and resolved_spec is not null
             )
                or (
                    schedule_start_mode = 'at'
                and status = 'VALIDATING'::task_status
                and resolved_spec is not null
                and nullif(resolved_spec->'schedule'->>'start_at', '') is not null
                and (resolved_spec->'schedule'->>'start_at')::timestamptz <= $1
             )
                or (
                    status = 'QUEUED'::task_status
                and resolved_spec is not null
                and schedule_start_mode <> 'cron'
             )
             order by created_at asc
            "#,
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn list_cron_schedules(&self) -> Result<Vec<CronScheduleEntry>, RepoError> {
        sqlx::query(
            r#"
            select
              t.id,
              t.requested_spec,
              t.created_at,
              (
                select payload->>'scheduled_for'
                  from task_events te
                 where te.task_id = t.id
                   and te.event_type = 'cron_task_triggered'
                 order by te.created_at desc, te.id desc
                 limit 1
              ) as last_scheduled_for
            from tasks t
            where t.schedule_start_mode = 'cron'
              and t.status in ('VALIDATING', 'CREATED', 'QUEUED')
            order by t.created_at asc
            "#,
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| {
            let last_scheduled_for = row
                .try_get::<Option<String>, _>("last_scheduled_for")?
                .as_deref()
                .map(DateTime::parse_from_rfc3339)
                .transpose()
                .map_err(|error| {
                    RepoError::Serde(serde_json::Error::io(std::io::Error::other(error)))
                })?
                .map(|value| value.with_timezone(&Utc));

            Ok(CronScheduleEntry {
                task_id: row.try_get("id")?,
                requested_spec: row.try_get("requested_spec")?,
                created_at: row.try_get("created_at")?,
                last_scheduled_for,
            })
        })
        .collect()
    }

    pub async fn trigger_cron_task(
        &self,
        parent_task_id: Uuid,
        scheduled_for: DateTime<Utc>,
    ) -> Result<Option<TaskSummary>, RepoError> {
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
              requested_spec
            from tasks
            where id = $1
            for update
            "#,
        )
        .bind(parent_task_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(RepoError::TaskNotFound(parent_task_id))?;

        let already_triggered: bool = sqlx::query_scalar(
            r#"
            select exists (
              select 1
                from task_events
               where task_id = $1
                 and event_type = 'cron_task_triggered'
                 and payload->>'scheduled_for' = $2
            )
            "#,
        )
        .bind(parent_task_id)
        .bind(scheduled_for.to_rfc3339())
        .fetch_one(&mut *tx)
        .await?;
        if already_triggered {
            tx.commit().await?;
            return Ok(None);
        }

        let _parent = TaskSummary::from_row(&row)?;
        let mut requested_spec: TaskSpec = serde_json::from_value(row.try_get("requested_spec")?)?;
        requested_spec.schedule.start_mode = Some(StartMode::Immediate);
        requested_spec.schedule.start_at = None;
        requested_spec.schedule.cron = None;

        let resolved = self.resolve_requested_task(&requested_spec).await?;
        let requested_spec = resolved.requested_spec;
        let resolved_spec = resolved.resolved_spec;
        let created_by = resolved_spec
            .created_by()
            .unwrap_or("scheduler")
            .to_string();
        let now = Utc::now();
        let task_id = Uuid::now_v7();
        let summary = TaskSummary {
            id: task_id,
            name: resolved_spec.name.clone(),
            task_type: resolved_spec.task_type,
            status: resolved_spec.initial_status(),
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

        sqlx::query(
            r#"
            insert into tasks (
              id, name, type, status, idempotency_key,
              priority, requested_spec, resolved_spec, created_by, assigned_node_id,
              current_attempt_no, schedule_start_mode, created_at, updated_at, started_at, finished_at
            ) values (
              $1, $2, $3::task_type, $4::task_status, $5,
              $6, $7, $8, $9, null,
              0, 'immediate', $10, $10, null, null
            )
            "#,
        )
        .bind(task_id)
        .bind(&summary.name)
        .bind(summary.task_type.as_str())
        .bind(summary.status.as_str())
        .bind(format!("cron-{parent_task_id}-{}", scheduled_for.timestamp()))
        .bind(i32::from(summary.priority))
        .bind(serde_json::to_value(&requested_spec)?)
        .bind(serde_json::to_value(&resolved_spec)?)
        .bind(&created_by)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        self.insert_event(
            &mut tx,
            parent_task_id,
            None,
            None,
            EventSource::Scheduler,
            "cron_task_triggered",
            "info",
            json!({
                "scheduled_for": scheduled_for,
                "spawned_task_id": task_id,
            }),
        )
        .await?;

        self.insert_event(
            &mut tx,
            task_id,
            None,
            None,
            EventSource::Scheduler,
            "task_created",
            "info",
            json!({
                "status": summary.status,
                "schedule_start_mode": StartMode::Immediate,
                "scheduled_for": scheduled_for,
                "parent_task_id": parent_task_id,
            }),
        )
        .await?;

        tx.commit().await?;
        Ok(Some(summary))
    }
}

#[derive(Debug, Clone)]
pub struct CronScheduleEntry {
    pub task_id: Uuid,
    pub requested_spec: Value,
    pub created_at: DateTime<Utc>,
    pub last_scheduled_for: Option<DateTime<Utc>>,
}
