use std::str::FromStr;

use chrono::{DateTime, Utc};
use media_domain::{
    AgentRegistration, AttemptStatus, CapabilitySnapshot, EventSource, HeartbeatSnapshot, Page,
    StartMode, TaskOperation, TaskSpec, TaskStateError, TaskStatus, TaskType, TaskValidationError,
    ValidationIssue, WorkerKind,
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
        let resolved = self.resolve_requested_task(&requested_spec).await?;
        let requested_spec = resolved.requested_spec;
        let resolved_spec = resolved.resolved_spec;
        let template_id = resolved.template_id;

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
            template_id,
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
              $1, $2, $3, $4::task_type, $5::task_status, $6, $7, $8,
              $9, $10, $11, $12, null,
              0, $13, $14, $15, null, null
            )
            "#,
        )
        .bind(task_id)
        .bind(&tenant_id)
        .bind(&resolved_spec.name)
        .bind(resolved_spec.task_type.as_str())
        .bind(status.as_str())
        .bind(template_id)
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

    pub async fn list_task_events(
        &self,
        task_id: Uuid,
        filter: TaskEventFilter,
    ) -> Result<Page<TaskEventSummary>, RepoError> {
        let page = filter.page.unwrap_or(1).max(1);
        let page_size = filter.page_size.unwrap_or(20).clamp(1, 200);
        let total = self.count_task_events(task_id, &filter).await?;

        let mut builder = QueryBuilder::<Postgres>::new(
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
            where task_id = "#,
        );
        builder.push_bind(task_id);
        apply_task_event_filters(&mut builder, &filter);
        builder.push(" order by created_at desc, id desc limit ");
        builder.push_bind(i64::from(page_size));
        builder.push(" offset ");
        builder.push_bind(i64::from((page - 1) * page_size));

        let rows = builder
            .build()
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|row| TaskEventSummary::from_row(&row))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Page::new(rows, page, page_size, total))
    }

    pub async fn list_task_logs(
        &self,
        task_id: Uuid,
        filter: TaskLogFilter,
    ) -> Result<TaskLogResponse, RepoError> {
        let task = self.fetch_task_summary(task_id).await?;
        let attempt_no = filter.attempt_no.unwrap_or(task.current_attempt_no).max(0);
        if attempt_no <= 0 {
            return Ok(TaskLogResponse {
                attempt_no,
                next_cursor: None,
                lines: Vec::new(),
            });
        }

        let stream_filter = filter.stream.as_deref().unwrap_or("merged").trim();
        let cursor_ts = parse_log_cursor(filter.cursor.as_deref())?;
        let limit = filter.limit.unwrap_or(200).clamp(1, 500) as usize;

        let mut builder = QueryBuilder::<Postgres>::new(
            r#"
            select created_at, payload
              from task_events
             where task_id = "#,
        );
        builder.push_bind(task_id);
        builder.push(" and attempt_no = ");
        builder.push_bind(attempt_no);
        builder.push(" and event_type = 'task_log_batch'");
        if let Some(cursor_ts) = cursor_ts {
            builder.push(" and created_at < ");
            builder.push_bind(cursor_ts);
        }
        builder.push(" order by created_at desc, id desc limit ");
        builder.push_bind(200_i64);

        let rows = builder.build().fetch_all(&self.pool).await?;
        let mut lines = Vec::new();
        let mut last_cursor = None;

        for row in rows {
            let created_at: DateTime<Utc> = row.try_get("created_at")?;
            let payload: Value = row.try_get("payload")?;
            let stream = payload
                .get("stream")
                .and_then(Value::as_str)
                .unwrap_or("stderr")
                .to_string();
            if stream_filter != "merged" && stream_filter != stream {
                continue;
            }
            let batch_lines = payload
                .get("lines")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for line in batch_lines {
                if let Some(line) = line.as_str() {
                    lines.push(TaskLogLine {
                        ts: created_at,
                        stream: stream.clone(),
                        line: line.to_string(),
                    });
                    last_cursor = Some(log_cursor_string(created_at));
                    if lines.len() >= limit {
                        return Ok(TaskLogResponse {
                            attempt_no,
                            next_cursor: last_cursor,
                            lines,
                        });
                    }
                }
            }
        }

        Ok(TaskLogResponse {
            attempt_no,
            next_cursor: last_cursor,
            lines,
        })
    }

    pub async fn create_template(
        &self,
        request: TemplateCreateRequest,
        created_by: &str,
    ) -> Result<TaskTemplateDetail, RepoError> {
        if request.name.trim().is_empty() {
            return Err(validation_error("name", "must not be empty"));
        }

        let now = Utc::now();
        let template_id = Uuid::now_v7();
        let default_spec = normalize_template_default_spec(
            request.task_type,
            request.profile.as_deref(),
            request.default_spec,
        )?;
        sqlx::query(
            r#"
            insert into task_templates (
              id, name, type, profile, default_spec, enabled, created_by, created_at, updated_at
            ) values (
              $1, $2, $3::task_type, $4, $5, $6, $7, $8, $8
            )
            "#,
        )
        .bind(template_id)
        .bind(request.name.trim())
        .bind(request.task_type.as_str())
        .bind(request.profile.as_deref())
        .bind(&default_spec)
        .bind(request.enabled)
        .bind(created_by)
        .bind(now)
        .execute(&self.pool)
        .await?;

        self.get_template(template_id).await
    }

    pub async fn list_templates(
        &self,
        filter: TemplateListFilter,
    ) -> Result<Vec<TaskTemplateSummary>, RepoError> {
        let mut builder = QueryBuilder::<Postgres>::new(
            r#"
            select
              id,
              name,
              type::text as task_type,
              profile,
              enabled,
              created_by,
              created_at,
              updated_at
            from task_templates
            where 1 = 1
            "#,
        );
        if let Some(task_type) = filter.task_type {
            builder.push(" and type = ");
            builder.push_bind(task_type.as_str());
            builder.push("::task_type");
        }
        if let Some(keyword) = filter
            .keyword
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let pattern = format!("%{keyword}%");
            builder.push(" and (name ilike ");
            builder.push_bind(pattern.clone());
            builder.push(" or coalesce(profile, '') ilike ");
            builder.push_bind(pattern);
            builder.push(")");
        }
        builder.push(" order by updated_at desc, name asc");

        builder
            .build()
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|row| TaskTemplateSummary::from_row(&row))
            .collect()
    }

    pub async fn get_template(&self, template_id: Uuid) -> Result<TaskTemplateDetail, RepoError> {
        sqlx::query(
            r#"
            select
              id,
              name,
              type::text as task_type,
              profile,
              default_spec,
              enabled,
              created_by,
              created_at,
              updated_at
            from task_templates
            where id = $1
            "#,
        )
        .bind(template_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| RepoError::TemplateNotFound(template_id.to_string()))
        .and_then(|row| TaskTemplateDetail::from_row(&row))
    }

    pub async fn render_template(
        &self,
        template_id: Uuid,
        overrides: Value,
    ) -> Result<Value, RepoError> {
        let template = self.fetch_template_by_id(template_id).await?;
        let task_type = template.task_type();
        let profile = overrides
            .get("profile")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| template.profile().map(str::to_string));
        let merged = build_resolved_task_json(
            task_type,
            profile.as_deref(),
            Some(&template.default_spec),
            &overrides,
        )?;
        let spec: TaskSpec = serde_json::from_value(merged.clone())?;
        spec.validate()?;
        Ok(serde_json::to_value(spec.resolved())?)
    }

    pub async fn list_streams(
        &self,
        filter: StreamListFilter,
    ) -> Result<Vec<StreamSummary>, RepoError> {
        let mut builder = QueryBuilder::<Postgres>::new(
            r#"
            select
              sb.id,
              sb.task_id,
              sb.attempt_id,
              ta.attempt_no,
              t.tenant_id,
              t.name as task_name,
              t.assigned_node_id as node_id,
              sb.schema,
              sb.vhost,
              sb.app,
              sb.stream,
              sb.zlm_proxy_key,
              sb.zlm_pusher_key,
              sb.rtp_stream_id,
              t.started_at,
              t.updated_at,
              (
                select case
                  when te.event_type = 'stream_no_reader' then false
                  when te.event_type in ('stream_publish_requested', 'running') then true
                  else null
                end
                  from task_events te
                 where te.task_id = sb.task_id
                   and te.event_type in ('stream_no_reader', 'stream_publish_requested', 'running')
                 order by te.created_at desc, te.id desc
                 limit 1
              ) as has_viewer
            from stream_bindings sb
            join tasks t on t.id = sb.task_id
            join task_attempts ta on ta.id = sb.attempt_id
            where 1 = 1
            "#,
        );
        if let Some(tenant_id) = filter
            .tenant_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            builder.push(" and t.tenant_id = ");
            builder.push_bind(tenant_id);
        }
        if let Some(schema) = filter
            .schema
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            builder.push(" and sb.schema = ");
            builder.push_bind(schema);
        }
        if let Some(app) = filter
            .app
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            builder.push(" and sb.app = ");
            builder.push_bind(app);
        }
        if let Some(stream) = filter
            .stream
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            builder.push(" and sb.stream = ");
            builder.push_bind(stream);
        }
        if let Some(task_id) = filter.task_id {
            builder.push(" and sb.task_id = ");
            builder.push_bind(task_id);
        }
        if let Some(node_id) = filter.node_id {
            builder.push(" and t.assigned_node_id = ");
            builder.push_bind(node_id);
        }
        builder.push(" order by t.updated_at desc, sb.schema asc, sb.app asc, sb.stream asc");
        let mut streams = builder
            .build()
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|row| StreamSummary::from_row(&row))
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(has_viewer) = filter.has_viewer {
            streams.retain(|stream| stream.has_viewer == Some(has_viewer));
        }
        Ok(streams)
    }

    pub async fn list_record_files(
        &self,
        filter: RecordListFilter,
    ) -> Result<Page<RecordFileSummary>, RepoError> {
        let page = filter.page.unwrap_or(1).max(1);
        let page_size = filter.page_size.unwrap_or(20).clamp(1, 200);
        let total = self.count_record_files(&filter).await?;

        let mut builder = QueryBuilder::<Postgres>::new(
            r#"
            select
              rf.id,
              rf.task_id,
              rf.attempt_id,
              rf.vhost,
              rf.app,
              rf.stream,
              rf.file_path,
              rf.file_size,
              rf.time_len,
              rf.start_time,
              rf.source,
              rf.created_at
            from record_files rf
            join tasks t on t.id = rf.task_id
            where 1 = 1
            "#,
        );
        apply_record_filters(&mut builder, &filter);
        builder.push(" order by start_time desc nulls last, created_at desc limit ");
        builder.push_bind(i64::from(page_size));
        builder.push(" offset ");
        builder.push_bind(i64::from((page - 1) * page_size));

        let rows = builder
            .build()
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|row| RecordFileSummary::from_row(&row))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Page::new(rows, page, page_size, total))
    }

    pub async fn list_nodes(&self) -> Result<Vec<NodeSummary>, RepoError> {
        sqlx::query(
            r#"
            select
              n.id,
              n.node_name,
              n.hostname,
              n.labels,
              n.zlm_api_base,
              n.agent_stream_addr,
              n.network_mode,
              n.interfaces,
              n.healthy,
              n.last_seen_at,
              n.created_at,
              n.updated_at,
              c.ffmpeg_protocols,
              c.ffmpeg_formats,
              c.ffmpeg_encoders,
              c.ffmpeg_decoders,
              c.zlm_api_list,
              c.zlm_version,
              c.gpu,
              c.captured_at
            from media_nodes n
            left join node_capabilities c on c.node_id = n.id
            order by n.updated_at desc, n.node_name asc
            "#,
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| NodeSummary::from_row(&row))
        .collect()
    }

    pub async fn get_node_debug_target(&self, node_id: Uuid) -> Result<NodeDebugTarget, RepoError> {
        sqlx::query(
            r#"
            select id, zlm_api_base, zlm_api_secret
              from media_nodes
             where id = $1
            "#,
        )
        .bind(node_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(RepoError::NodeNotFound(node_id))
        .and_then(|row| {
            Ok(NodeDebugTarget {
                zlm_api_base: row.try_get("zlm_api_base")?,
                zlm_api_secret: row.try_get("zlm_api_secret")?,
            })
        })
    }

    pub async fn list_due_at_tasks(&self, now: DateTime<Utc>) -> Result<Vec<Uuid>, RepoError> {
        Ok(sqlx::query_scalar(
            r#"
            select id
              from tasks
             where schedule_start_mode = 'at'
               and status = 'VALIDATING'::task_status
               and resolved_spec is not null
               and nullif(resolved_spec->'schedule'->>'start_at', '') is not null
               and (resolved_spec->'schedule'->>'start_at')::timestamptz <= $1
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

        let parent = TaskSummary::from_row(&row)?;
        let mut requested_spec: TaskSpec = serde_json::from_value(row.try_get("requested_spec")?)?;
        requested_spec.schedule.start_mode = Some(StartMode::Immediate);
        requested_spec.schedule.start_at = None;
        requested_spec.schedule.cron = None;

        let resolved = self.resolve_requested_task(&requested_spec).await?;
        let requested_spec = resolved.requested_spec;
        let resolved_spec = resolved.resolved_spec;
        let template_id = resolved.template_id.or(parent.template_id);
        let tenant_id = resolved_spec.tenant_id().to_string();
        let created_by = resolved_spec
            .created_by()
            .unwrap_or("scheduler")
            .to_string();
        let now = Utc::now();
        let task_id = Uuid::now_v7();
        let summary = TaskSummary {
            id: task_id,
            tenant_id: tenant_id.clone(),
            name: resolved_spec.name.clone(),
            task_type: resolved_spec.task_type,
            status: resolved_spec.initial_status(),
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

        sqlx::query(
            r#"
            insert into tasks (
              id, tenant_id, name, type, status, template_id, profile, idempotency_key,
              priority, requested_spec, resolved_spec, created_by, assigned_node_id,
              current_attempt_no, schedule_start_mode, created_at, updated_at, started_at, finished_at
            ) values (
              $1, $2, $3, $4::task_type, $5::task_status, $6, $7, $8,
              $9, $10, $11, $12, null,
              0, 'immediate', $13, $13, null, null
            )
            "#,
        )
        .bind(task_id)
        .bind(&tenant_id)
        .bind(&summary.name)
        .bind(summary.task_type.as_str())
        .bind(summary.status.as_str())
        .bind(summary.template_id)
        .bind(summary.profile.as_deref())
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

    async fn resolve_requested_task(
        &self,
        requested_spec: &TaskSpec,
    ) -> Result<ResolvedTaskRequest, RepoError> {
        let template = if let Some(name) = requested_spec
            .template
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(self.fetch_template_by_name(name).await?)
        } else {
            None
        };

        if let Some(template) = &template {
            if template.task_type() != requested_spec.task_type {
                return Err(validation_error(
                    "template",
                    format!(
                        "template {} is for type {}, but request is {}",
                        template.name(),
                        template.task_type(),
                        requested_spec.task_type
                    ),
                ));
            }
        }

        let effective_profile = requested_spec
            .profile
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| {
                template
                    .as_ref()
                    .and_then(|value| value.profile().map(str::to_string))
            });
        let overlay = task_spec_overlay(requested_spec);
        let merged_json = build_resolved_task_json(
            requested_spec.task_type,
            effective_profile.as_deref(),
            template.as_ref().map(|value| &value.default_spec),
            &overlay,
        )?;
        let merged_spec: TaskSpec = serde_json::from_value(merged_json)?;
        merged_spec.validate()?;
        let resolved_spec = merged_spec.resolved();
        resolved_spec.validate()?;

        Ok(ResolvedTaskRequest {
            requested_spec: merged_spec,
            resolved_spec,
            template_id: template.map(|value| value.id()),
        })
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

        let template_id: Option<Uuid> = row.try_get("template_id")?;
        let requested_spec_value: Value = row.try_get("requested_spec")?;
        let mut requested_spec: TaskSpec = serde_json::from_value(requested_spec_value)?;
        if let Some(overrides) = overrides {
            apply_clone_overrides(&mut requested_spec, overrides);
        }
        let resolved = self.resolve_requested_task(&requested_spec).await?;
        let requested_spec = resolved.requested_spec;
        let resolved_spec = resolved.resolved_spec;
        let template_id = resolved.template_id.or(template_id);

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
              id, node_name, hostname, labels, zlm_api_base, zlm_api_secret, agent_stream_addr,
              network_mode, interfaces, healthy, last_seen_at, created_at, updated_at
            ) values (
              $1, $2, $3, $4, $5, $6, $7,
              $8, $9, true, $10, $11, $11
            )
            on conflict (id) do update
               set node_name = excluded.node_name,
                   hostname = excluded.hostname,
                   labels = excluded.labels,
                   zlm_api_base = excluded.zlm_api_base,
                   zlm_api_secret = excluded.zlm_api_secret,
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
        .bind(&registration.zlm_api_secret)
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

    async fn count_task_events(
        &self,
        task_id: Uuid,
        filter: &TaskEventFilter,
    ) -> Result<u64, RepoError> {
        let mut builder = QueryBuilder::<Postgres>::new(
            "select count(*) as total from task_events where task_id = ",
        );
        builder.push_bind(task_id);
        apply_task_event_filters(&mut builder, filter);

        let row = builder.build().fetch_one(&self.pool).await?;
        let total: i64 = row.try_get("total")?;
        Ok(total as u64)
    }

    async fn count_record_files(&self, filter: &RecordListFilter) -> Result<u64, RepoError> {
        let mut builder = QueryBuilder::<Postgres>::new(
            "select count(*) as total from record_files rf join tasks t on t.id = rf.task_id where 1 = 1",
        );
        apply_record_filters(&mut builder, filter);

        let row = builder.build().fetch_one(&self.pool).await?;
        let total: i64 = row.try_get("total")?;
        Ok(total as u64)
    }

    async fn fetch_template_by_name(&self, name: &str) -> Result<TaskTemplateDetail, RepoError> {
        sqlx::query(
            r#"
            select
              id,
              name,
              type::text as task_type,
              profile,
              default_spec,
              enabled,
              created_by,
              created_at,
              updated_at
            from task_templates
            where name = $1
              and enabled = true
            "#,
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| RepoError::TemplateNotFound(name.to_string()))
        .and_then(|row| TaskTemplateDetail::from_row(&row))
    }

    async fn fetch_template_by_id(
        &self,
        template_id: Uuid,
    ) -> Result<TaskTemplateDetail, RepoError> {
        self.get_template(template_id).await
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
struct ResolvedTaskRequest {
    requested_spec: TaskSpec,
    resolved_spec: TaskSpec,
    template_id: Option<Uuid>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TaskEventFilter {
    #[serde(default)]
    pub attempt_no: Option<i32>,
    #[serde(default)]
    pub source: Option<EventSource>,
    #[serde(default)]
    pub event_type: Option<String>,
    #[serde(default)]
    pub page: Option<u32>,
    #[serde(default)]
    pub page_size: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TaskLogFilter {
    #[serde(default)]
    pub attempt_no: Option<i32>,
    #[serde(default)]
    pub stream: Option<String>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskLogLine {
    pub ts: DateTime<Utc>,
    pub stream: String,
    pub line: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskLogResponse {
    pub attempt_no: i32,
    pub next_cursor: Option<String>,
    pub lines: Vec<TaskLogLine>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TemplateCreateRequest {
    pub name: String,
    #[serde(rename = "type")]
    pub task_type: TaskType,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub default_spec: Value,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TemplateListFilter {
    #[serde(default, rename = "type")]
    pub task_type: Option<TaskType>,
    #[serde(default)]
    pub keyword: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskTemplateSummary {
    pub id: Uuid,
    pub name: String,
    #[serde(rename = "type")]
    pub task_type: TaskType,
    pub profile: Option<String>,
    pub enabled: bool,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TaskTemplateSummary {
    fn from_row(row: &PgRow) -> Result<Self, RepoError> {
        Ok(Self {
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            task_type: TaskType::from_str(row.try_get::<&str, _>("task_type")?)
                .map_err(RepoError::ParseEnum)?,
            profile: row.try_get("profile")?,
            enabled: row.try_get("enabled")?,
            created_by: row.try_get("created_by")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskTemplateDetail {
    #[serde(flatten)]
    pub summary: TaskTemplateSummary,
    pub default_spec: Value,
}

impl TaskTemplateDetail {
    fn from_row(row: &PgRow) -> Result<Self, RepoError> {
        Ok(Self {
            summary: TaskTemplateSummary::from_row(row)?,
            default_spec: row.try_get("default_spec")?,
        })
    }

    fn id(&self) -> Uuid {
        self.summary.id
    }

    fn name(&self) -> &str {
        &self.summary.name
    }

    fn task_type(&self) -> TaskType {
        self.summary.task_type
    }

    fn profile(&self) -> Option<&str> {
        self.summary.profile.as_deref()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct StreamListFilter {
    #[serde(default)]
    pub tenant_id: Option<String>,
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub app: Option<String>,
    #[serde(default)]
    pub stream: Option<String>,
    #[serde(default)]
    pub task_id: Option<Uuid>,
    #[serde(default)]
    pub node_id: Option<Uuid>,
    #[serde(default)]
    pub has_viewer: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StreamSummary {
    pub id: Uuid,
    pub task_id: Uuid,
    pub attempt_id: Uuid,
    pub attempt_no: i32,
    pub tenant_id: String,
    pub task_name: String,
    pub node_id: Option<Uuid>,
    pub schema: String,
    pub vhost: String,
    pub app: String,
    pub stream: String,
    pub zlm_proxy_key: Option<String>,
    pub zlm_pusher_key: Option<String>,
    pub rtp_stream_id: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub has_viewer: Option<bool>,
}

impl StreamSummary {
    fn from_row(row: &PgRow) -> Result<Self, RepoError> {
        Ok(Self {
            id: row.try_get("id")?,
            task_id: row.try_get("task_id")?,
            attempt_id: row.try_get("attempt_id")?,
            attempt_no: row.try_get("attempt_no")?,
            tenant_id: row.try_get("tenant_id")?,
            task_name: row.try_get("task_name")?,
            node_id: row.try_get("node_id")?,
            schema: row.try_get("schema")?,
            vhost: row.try_get("vhost")?,
            app: row.try_get("app")?,
            stream: row.try_get("stream")?,
            zlm_proxy_key: row.try_get("zlm_proxy_key")?,
            zlm_pusher_key: row.try_get("zlm_pusher_key")?,
            rtp_stream_id: row.try_get("rtp_stream_id")?,
            started_at: row.try_get("started_at")?,
            updated_at: row.try_get("updated_at")?,
            has_viewer: row.try_get("has_viewer")?,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RecordListFilter {
    #[serde(default)]
    pub tenant_id: Option<String>,
    #[serde(default)]
    pub task_id: Option<Uuid>,
    #[serde(default)]
    pub stream: Option<String>,
    #[serde(default)]
    pub date_from: Option<DateTime<Utc>>,
    #[serde(default)]
    pub date_to: Option<DateTime<Utc>>,
    #[serde(default)]
    pub page: Option<u32>,
    #[serde(default)]
    pub page_size: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecordFileSummary {
    pub id: Uuid,
    pub task_id: Uuid,
    pub attempt_id: Option<Uuid>,
    pub vhost: Option<String>,
    pub app: Option<String>,
    pub stream: Option<String>,
    pub file_path: String,
    pub file_size: i64,
    pub time_len: Option<i32>,
    pub start_time: Option<DateTime<Utc>>,
    pub source: String,
    pub created_at: DateTime<Utc>,
}

impl RecordFileSummary {
    fn from_row(row: &PgRow) -> Result<Self, RepoError> {
        Ok(Self {
            id: row.try_get("id")?,
            task_id: row.try_get("task_id")?,
            attempt_id: row.try_get("attempt_id")?,
            vhost: row.try_get("vhost")?,
            app: row.try_get("app")?,
            stream: row.try_get("stream")?,
            file_path: row.try_get("file_path")?,
            file_size: row.try_get("file_size")?,
            time_len: row.try_get("time_len")?,
            start_time: row.try_get("start_time")?,
            source: row.try_get("source")?,
            created_at: row.try_get("created_at")?,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeSummary {
    pub id: Uuid,
    pub node_name: String,
    pub hostname: String,
    pub labels: Vec<String>,
    pub zlm_api_base: String,
    pub agent_stream_addr: String,
    pub network_mode: String,
    pub interfaces: Vec<String>,
    pub healthy: bool,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub ffmpeg_protocols: Vec<String>,
    pub ffmpeg_formats: Vec<String>,
    pub ffmpeg_encoders: Vec<String>,
    pub ffmpeg_decoders: Vec<String>,
    pub zlm_api_list: Vec<String>,
    pub zlm_version: Option<String>,
    pub gpu: Vec<String>,
    pub capability_captured_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot_usage: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub running_tasks: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connected: Option<bool>,
}

impl NodeSummary {
    fn from_row(row: &PgRow) -> Result<Self, RepoError> {
        Ok(Self {
            id: row.try_get("id")?,
            node_name: row.try_get("node_name")?,
            hostname: row.try_get("hostname")?,
            labels: serde_json::from_value(row.try_get("labels")?)?,
            zlm_api_base: row.try_get("zlm_api_base")?,
            agent_stream_addr: row.try_get("agent_stream_addr")?,
            network_mode: row.try_get("network_mode")?,
            interfaces: serde_json::from_value(row.try_get("interfaces")?)?,
            healthy: row.try_get("healthy")?,
            last_seen_at: row.try_get("last_seen_at")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
            ffmpeg_protocols: row
                .try_get::<Option<Value>, _>("ffmpeg_protocols")?
                .map(serde_json::from_value)
                .transpose()?
                .unwrap_or_default(),
            ffmpeg_formats: row
                .try_get::<Option<Value>, _>("ffmpeg_formats")?
                .map(serde_json::from_value)
                .transpose()?
                .unwrap_or_default(),
            ffmpeg_encoders: row
                .try_get::<Option<Value>, _>("ffmpeg_encoders")?
                .map(serde_json::from_value)
                .transpose()?
                .unwrap_or_default(),
            ffmpeg_decoders: row
                .try_get::<Option<Value>, _>("ffmpeg_decoders")?
                .map(serde_json::from_value)
                .transpose()?
                .unwrap_or_default(),
            zlm_api_list: row
                .try_get::<Option<Value>, _>("zlm_api_list")?
                .map(serde_json::from_value)
                .transpose()?
                .unwrap_or_default(),
            zlm_version: row.try_get("zlm_version")?,
            gpu: row
                .try_get::<Option<Value>, _>("gpu")?
                .map(serde_json::from_value)
                .transpose()?
                .unwrap_or_default(),
            capability_captured_at: row.try_get("captured_at")?,
            slot_usage: None,
            running_tasks: None,
            connected: None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct NodeDebugTarget {
    pub zlm_api_base: String,
    pub zlm_api_secret: String,
}

#[derive(Debug, Clone)]
pub struct CronScheduleEntry {
    pub task_id: Uuid,
    pub requested_spec: Value,
    pub created_at: DateTime<Utc>,
    pub last_scheduled_for: Option<DateTime<Utc>>,
}

fn default_true() -> bool {
    true
}

fn validation_error(field: &'static str, message: impl Into<String>) -> RepoError {
    RepoError::Validation(TaskValidationError {
        issues: vec![ValidationIssue::new(field, message)],
    })
}

fn parse_log_cursor(value: Option<&str>) -> Result<Option<DateTime<Utc>>, RepoError> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let millis = value
        .parse::<i64>()
        .map_err(|_| validation_error("cursor", "must be a unix timestamp in milliseconds"))?;
    DateTime::<Utc>::from_timestamp_millis(millis)
        .ok_or_else(|| validation_error("cursor", "must be a valid unix timestamp"))
        .map(Some)
}

fn log_cursor_string(value: DateTime<Utc>) -> String {
    value.timestamp_millis().to_string()
}

fn apply_task_event_filters<'a>(
    builder: &mut QueryBuilder<'a, Postgres>,
    filter: &'a TaskEventFilter,
) {
    if let Some(attempt_no) = filter.attempt_no {
        builder.push(" and attempt_no = ");
        builder.push_bind(attempt_no);
    }
    if let Some(source) = filter.source {
        builder.push(" and source = ");
        builder.push_bind(source.as_str());
        builder.push("::event_source");
    }
    if let Some(event_type) = filter
        .event_type
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        builder.push(" and event_type = ");
        builder.push_bind(event_type);
    }
}

fn apply_record_filters<'a>(
    builder: &mut QueryBuilder<'a, Postgres>,
    filter: &'a RecordListFilter,
) {
    if let Some(tenant_id) = filter
        .tenant_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        builder.push(" and t.tenant_id = ");
        builder.push_bind(tenant_id);
    }
    if let Some(task_id) = filter.task_id {
        builder.push(" and rf.task_id = ");
        builder.push_bind(task_id);
    }
    if let Some(stream) = filter
        .stream
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        builder.push(" and rf.stream = ");
        builder.push_bind(stream);
    }
    if let Some(date_from) = filter.date_from {
        builder.push(" and coalesce(rf.start_time, rf.created_at) >= ");
        builder.push_bind(date_from);
    }
    if let Some(date_to) = filter.date_to {
        builder.push(" and coalesce(rf.start_time, rf.created_at) <= ");
        builder.push_bind(date_to);
    }
}

fn normalize_template_default_spec(
    task_type: TaskType,
    profile: Option<&str>,
    default_spec: Value,
) -> Result<Value, RepoError> {
    if !default_spec.is_object() {
        return Err(validation_error(
            "default_spec",
            "template default_spec must be a JSON object",
        ));
    }
    build_resolved_task_json(task_type, profile, None, &default_spec)
}

fn build_resolved_task_json(
    task_type: TaskType,
    profile: Option<&str>,
    template_defaults: Option<&Value>,
    request_overrides: &Value,
) -> Result<Value, RepoError> {
    let mut merged = profile_defaults_json(task_type, profile);
    if let Some(template_defaults) = template_defaults {
        if !template_defaults.is_object() {
            return Err(validation_error(
                "template.default_spec",
                "template default_spec must be a JSON object",
            ));
        }
        deep_merge(&mut merged, template_defaults.clone());
    }
    if !request_overrides.is_object() {
        return Err(validation_error(
            "task",
            "request payload must be a JSON object",
        ));
    }
    deep_merge(&mut merged, request_overrides.clone());
    merged["type"] = Value::String(task_type.as_str().to_string());
    if merged
        .get("profile")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_none()
    {
        if let Some(profile) = profile.map(str::trim).filter(|value| !value.is_empty()) {
            merged["profile"] = Value::String(profile.to_string());
        }
    }
    Ok(merged)
}

fn profile_defaults_json(task_type: TaskType, profile: Option<&str>) -> Value {
    let mut value = json!({});
    let Some(profile) = profile.map(str::trim).filter(|value| !value.is_empty()) else {
        return value;
    };

    let defaults = match profile {
        "realtime_compat" => json!({
            "process": {
                "mode": "copy_or_transcode",
                "video_codec": "h264",
                "audio_codec": "aac"
            },
            "publish": {
                "enable_rtsp": true,
                "enable_rtmp": true,
                "enable_http_ts": true,
                "enable_http_fmp4": true,
                "enable_hls": false,
                "enable_webrtc": false
            }
        }),
        "rtc_web_compat" => json!({
            "process": {
                "mode": "transcode",
                "video_codec": "h264",
                "audio_codec": "opus",
                "profile": "baseline"
            },
            "publish": {
                "enable_rtsp": true,
                "enable_rtmp": false,
                "enable_http_ts": false,
                "enable_http_fmp4": true,
                "enable_hls": false,
                "enable_webrtc": true
            }
        }),
        "archive_quality" => json!({
            "process": {
                "mode": "copy_or_transcode"
            },
            "record": {
                "enabled": true,
                "format": "mp4"
            }
        }),
        "multicast_ts" => json!({
            "process": {
                "mode": "copy_or_transcode"
            },
            "publish": {
                "format": "mpegts",
                "ttl": 1,
                "reuse": true,
                "pkt_size": 1316
            }
        }),
        "rtmp_hevc_ext" => json!({
            "process": {
                "mode": "transcode",
                "video_codec": "h265",
                "audio_codec": "aac"
            },
            "publish": {
                "enable_rtmp": true
            }
        }),
        _ => json!({}),
    };
    deep_merge(&mut value, defaults);

    if !value.is_object() {
        return json!({});
    }

    if task_type == TaskType::FileTranscode {
        value["recovery"] = json!({"policy": "on_failure"});
    }

    value
}

fn task_spec_overlay(spec: &TaskSpec) -> Value {
    let mut overlay = serde_json::Map::new();
    overlay.insert(
        "type".to_string(),
        Value::String(spec.task_type.as_str().to_string()),
    );
    overlay.insert("name".to_string(), Value::String(spec.name.clone()));
    overlay.insert("priority".to_string(), json!(spec.priority));
    if let Some(template) = spec
        .template
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        overlay.insert(
            "template".to_string(),
            Value::String(template.trim().to_string()),
        );
    }
    if let Some(profile) = spec
        .profile
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        overlay.insert(
            "profile".to_string(),
            Value::String(profile.trim().to_string()),
        );
    }

    let mut common = serde_json::Map::new();
    if let Some(tenant_id) = spec
        .common
        .tenant_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        common.insert(
            "tenant_id".to_string(),
            Value::String(tenant_id.trim().to_string()),
        );
    }
    if let Some(created_by) = spec
        .common
        .created_by
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        common.insert(
            "created_by".to_string(),
            Value::String(created_by.trim().to_string()),
        );
    }
    if let Some(callback_url) = spec
        .common
        .callback_url
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        common.insert(
            "callback_url".to_string(),
            Value::String(callback_url.trim().to_string()),
        );
    }
    if !spec.common.labels.is_empty() {
        common.insert("labels".to_string(), json!(spec.common.labels));
    }
    if !common.is_empty() {
        overlay.insert("common".to_string(), Value::Object(common));
    }

    let input = overlay_optional_fields(&[
        ("kind", spec.input.kind.map(|value| json!(value))),
        ("url", spec.input.url.as_ref().map(|value| json!(value))),
        ("group", spec.input.group.as_ref().map(|value| json!(value))),
        ("port", spec.input.port.map(|value| json!(value))),
        (
            "interface_ip",
            spec.input.interface_ip.as_ref().map(|value| json!(value)),
        ),
        ("ttl", spec.input.ttl.map(|value| json!(value))),
        ("reuse", spec.input.reuse.map(|value| json!(value))),
        ("pkt_size", spec.input.pkt_size.map(|value| json!(value))),
        ("dscp", spec.input.dscp.map(|value| json!(value))),
        (
            "buffer_size",
            spec.input.buffer_size.map(|value| json!(value)),
        ),
        ("fifo_size", spec.input.fifo_size.map(|value| json!(value))),
        (
            "probe_timeout_ms",
            spec.input.probe_timeout_ms.map(|value| json!(value)),
        ),
        ("tcp_mode", spec.input.tcp_mode.map(|value| json!(value))),
        ("ssrc", spec.input.ssrc.map(|value| json!(value))),
    ]);
    if let Some(input) = input {
        overlay.insert("input".to_string(), input);
    }

    let process = overlay_optional_fields(&[
        ("mode", spec.process.mode.as_ref().map(|value| json!(value))),
        (
            "video_codec",
            spec.process.video_codec.as_ref().map(|value| json!(value)),
        ),
        (
            "audio_codec",
            spec.process.audio_codec.as_ref().map(|value| json!(value)),
        ),
        ("bitrate", spec.process.bitrate.map(|value| json!(value))),
        ("fps", spec.process.fps.map(|value| json!(value))),
        ("gop", spec.process.gop.map(|value| json!(value))),
        (
            "profile",
            spec.process.profile.as_ref().map(|value| json!(value)),
        ),
        (
            "preset",
            spec.process.preset.as_ref().map(|value| json!(value)),
        ),
    ]);
    if let Some(process) = process {
        overlay.insert("process".to_string(), process);
    }

    let publish = overlay_optional_fields(&[
        ("kind", spec.publish.kind.map(|value| json!(value))),
        ("url", spec.publish.url.as_ref().map(|value| json!(value))),
        (
            "group",
            spec.publish.group.as_ref().map(|value| json!(value)),
        ),
        ("port", spec.publish.port.map(|value| json!(value))),
        (
            "interface_ip",
            spec.publish.interface_ip.as_ref().map(|value| json!(value)),
        ),
        ("ttl", spec.publish.ttl.map(|value| json!(value))),
        ("reuse", spec.publish.reuse.map(|value| json!(value))),
        ("pkt_size", spec.publish.pkt_size.map(|value| json!(value))),
        ("dscp", spec.publish.dscp.map(|value| json!(value))),
        (
            "buffer_size",
            spec.publish.buffer_size.map(|value| json!(value)),
        ),
        (
            "fifo_size",
            spec.publish.fifo_size.map(|value| json!(value)),
        ),
        (
            "format",
            spec.publish.format.as_ref().map(|value| json!(value)),
        ),
        (
            "enable_rtsp",
            spec.publish.enable_rtsp.map(|value| json!(value)),
        ),
        (
            "enable_rtmp",
            spec.publish.enable_rtmp.map(|value| json!(value)),
        ),
        (
            "enable_http_ts",
            spec.publish.enable_http_ts.map(|value| json!(value)),
        ),
        (
            "enable_http_fmp4",
            spec.publish.enable_http_fmp4.map(|value| json!(value)),
        ),
        (
            "enable_hls",
            spec.publish.enable_hls.map(|value| json!(value)),
        ),
        (
            "enable_webrtc",
            spec.publish.enable_webrtc.map(|value| json!(value)),
        ),
        (
            "stop_on_no_reader",
            spec.publish.stop_on_no_reader.map(|value| json!(value)),
        ),
    ]);
    if let Some(publish) = publish {
        overlay.insert("publish".to_string(), publish);
    }

    let record = overlay_optional_fields(&[
        ("enabled", spec.record.enabled.map(|value| json!(value))),
        ("format", spec.record.format.map(|value| json!(value))),
        (
            "segment_sec",
            spec.record.segment_sec.map(|value| json!(value)),
        ),
        (
            "save_path",
            spec.record.save_path.as_ref().map(|value| json!(value)),
        ),
        ("as_player", spec.record.as_player.map(|value| json!(value))),
        (
            "archive_policy",
            spec.record
                .archive_policy
                .as_ref()
                .map(|value| json!(value)),
        ),
        (
            "retention_days",
            spec.record.retention_days.map(|value| json!(value)),
        ),
    ]);
    if let Some(record) = record {
        overlay.insert("record".to_string(), record);
    }

    let recovery = overlay_optional_fields(&[
        ("policy", spec.recovery.policy.map(|value| json!(value))),
        (
            "resume_mode",
            spec.recovery.resume_mode.as_ref().map(|value| json!(value)),
        ),
        (
            "orphan_adopt",
            spec.recovery.orphan_adopt.map(|value| json!(value)),
        ),
        (
            "max_consecutive_failures",
            spec.recovery
                .max_consecutive_failures
                .map(|value| json!(value)),
        ),
    ]);
    if let Some(recovery) = recovery {
        overlay.insert("recovery".to_string(), recovery);
    }

    let mut schedule = serde_json::Map::new();
    if let Some(start_mode) = spec.schedule.start_mode {
        schedule.insert("start_mode".to_string(), json!(start_mode));
    }
    if let Some(start_at) = spec.schedule.start_at {
        schedule.insert("start_at".to_string(), json!(start_at));
    }
    if let Some(cron) = spec
        .schedule
        .cron
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        schedule.insert("cron".to_string(), json!(cron));
    }
    if !schedule.is_empty() {
        overlay.insert("schedule".to_string(), Value::Object(schedule));
    }

    let mut resource = serde_json::Map::new();
    if !spec.resource.required_labels.is_empty() {
        resource.insert(
            "required_labels".to_string(),
            json!(spec.resource.required_labels),
        );
    }
    if !spec.resource.preferred_labels.is_empty() {
        resource.insert(
            "preferred_labels".to_string(),
            json!(spec.resource.preferred_labels),
        );
    }
    if let Some(network_interface) = spec
        .resource
        .network_interface
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        resource.insert("network_interface".to_string(), json!(network_interface));
    }
    if let Some(need_gpu) = spec.resource.need_gpu {
        resource.insert("need_gpu".to_string(), json!(need_gpu));
    }
    if let Some(slot_class) = spec
        .resource
        .slot_class
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        resource.insert("slot_class".to_string(), json!(slot_class));
    }
    if let Some(max_cpu_percent) = spec.resource.max_cpu_percent {
        resource.insert("max_cpu_percent".to_string(), json!(max_cpu_percent));
    }
    if !resource.is_empty() {
        overlay.insert("resource".to_string(), Value::Object(resource));
    }

    Value::Object(overlay)
}

fn overlay_optional_fields(fields: &[(&str, Option<Value>)]) -> Option<Value> {
    let mut object = serde_json::Map::new();
    for (key, value) in fields {
        if let Some(value) = value {
            object.insert((*key).to_string(), value.clone());
        }
    }
    (!object.is_empty()).then_some(Value::Object(object))
}

fn deep_merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base_map), Value::Object(overlay_map)) => {
            for (key, overlay_value) in overlay_map {
                match base_map.get_mut(&key) {
                    Some(base_value) => deep_merge(base_value, overlay_value),
                    None => {
                        base_map.insert(key, overlay_value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
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

impl TaskListFilter {
    pub fn for_principal(mut self, tenant_id: Option<&str>) -> Result<Self, RepoError> {
        let Some(tenant_id) = tenant_id else {
            return Ok(self);
        };

        match self.tenant_id.as_deref() {
            Some(current) if current != tenant_id => Err(validation_error(
                "tenant_id",
                "must match the authenticated tenant scope",
            )),
            _ => {
                self.tenant_id = Some(tenant_id.to_string());
                Ok(self)
            }
        }
    }
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
    #[error("template {0} was not found")]
    TemplateNotFound(String),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_resolved_task_json_applies_profile_template_then_request() {
        let merged = build_resolved_task_json(
            TaskType::LiveRelay,
            Some("realtime_compat"),
            Some(&json!({
                "publish": {
                    "enable_rtsp": false,
                    "enable_hls": true
                },
                "record": {
                    "enabled": true
                }
            })),
            &json!({
                "name": "relay-camera-01",
                "common": {
                    "created_by": "alice"
                },
                "input": {
                    "kind": "rtsp",
                    "url": "rtsp://camera.example/live"
                },
                "publish": {
                    "enable_rtsp": true
                }
            }),
        )
        .expect("merged json should build");

        let spec: TaskSpec = serde_json::from_value(merged).expect("task spec should parse");
        let resolved = spec.resolved();

        assert_eq!(resolved.process.video_codec.as_deref(), Some("h264"));
        assert_eq!(resolved.publish.enable_rtsp, Some(true));
        assert_eq!(resolved.publish.enable_hls, Some(true));
        assert_eq!(resolved.record.enabled, Some(true));
    }

    #[test]
    fn task_list_filter_is_scoped_to_principal_tenant() {
        let filter = TaskListFilter {
            tenant_id: None,
            ..TaskListFilter::default()
        }
        .for_principal(Some("tenant-a"))
        .expect("tenant scope should apply");

        assert_eq!(filter.tenant_id.as_deref(), Some("tenant-a"));

        let error = TaskListFilter {
            tenant_id: Some("tenant-b".to_string()),
            ..TaskListFilter::default()
        }
        .for_principal(Some("tenant-a"))
        .expect_err("mismatched tenant should fail");

        assert!(matches!(error, RepoError::Validation(_)));
    }

    #[test]
    fn task_spec_overlay_skips_empty_option_fields() {
        let spec = TaskSpec {
            task_type: TaskType::LiveRelay,
            template: Some("tpl_default_rtsp".to_string()),
            name: "relay-camera-01".to_string(),
            profile: None,
            priority: 50,
            common: media_domain::CommonSpec {
                tenant_id: None,
                created_by: Some("alice".to_string()),
                callback_url: None,
                labels: Vec::new(),
            },
            input: media_domain::InputSpec {
                kind: Some(media_domain::InputKind::Rtsp),
                url: Some("rtsp://camera.example/live".to_string()),
                ..Default::default()
            },
            process: Default::default(),
            publish: Default::default(),
            record: Default::default(),
            recovery: Default::default(),
            schedule: Default::default(),
            resource: Default::default(),
        };

        let overlay = task_spec_overlay(&spec);

        assert_eq!(overlay["template"], json!("tpl_default_rtsp"));
        assert_eq!(overlay["common"]["created_by"], json!("alice"));
        assert!(overlay["publish"].is_null());
    }
}
