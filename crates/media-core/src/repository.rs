use std::{
    net::IpAddr,
    path::{Component, Path},
    str::FromStr,
};

use chrono::{DateTime, Utc};
use media_domain::{
    AgentRegistration, AttemptStatus, CapabilitySnapshot, EventSource, GpuDeviceInfo,
    GpuRuntimeStats, HeartbeatSnapshot, InputKind, Page, PublishTargetKind, RecoveryPolicy,
    SourceMode, StartMode, TaskOperation, TaskSpec, TaskStateError, TaskStatus, TaskType,
    TaskValidationError, ValidationIssue, WorkerKind,
};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{
    PgPool, Postgres, QueryBuilder, Row,
    postgres::{PgQueryResult, PgRow},
};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct TaskRepository {
    pool: PgPool,
    callback_settle_delay: chrono::Duration,
}

impl TaskRepository {
    pub fn new(pool: PgPool) -> Self {
        Self::with_callback_settle_delay(pool, chrono::Duration::milliseconds(8_000))
    }

    pub fn with_callback_settle_delay(
        pool: PgPool,
        callback_settle_delay: chrono::Duration,
    ) -> Self {
        Self {
            pool,
            callback_settle_delay,
        }
    }

    pub async fn health_check(&self) -> Result<(), RepoError> {
        sqlx::query("select 1").execute(&self.pool).await?;
        Ok(())
    }

    pub async fn has_enabled_admin_user(&self) -> Result<bool, RepoError> {
        Ok(sqlx::query_scalar(
            r#"
            select exists (
              select 1
                from auth_users
               where enabled = true
                 and role = 'admin'
            )
            "#,
        )
        .fetch_one(&self.pool)
        .await?)
    }

    pub async fn create_bootstrap_admin(
        &self,
        username: &str,
        password_hash: &str,
        must_change_password: bool,
    ) -> Result<(), RepoError> {
        let now = Utc::now();
        sqlx::query(
            r#"
            insert into auth_users (
              id, username, password_hash, role, enabled, must_change_password,
              password_changed_at, created_at, updated_at
            ) values (
              $1, $2, $3, 'admin', true, $4, $5, $5, $5
            )
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(username)
        .bind(password_hash)
        .bind(must_change_password)
        .bind(now)
        .execute(&self.pool)
        .await?;
        self.insert_security_audit_event(SecurityAuditEventRecord {
            event_type: "admin_bootstrapped".to_string(),
            actor: username.to_string(),
            subject: Some(username.to_string()),
            remote_ip: None,
            user_agent: None,
            payload: json!({}),
        })
        .await?;
        Ok(())
    }

    pub async fn reset_user_password(
        &self,
        username: &str,
        password_hash: &str,
        must_change_password: bool,
        actor: &str,
        event_type: &str,
        remote_ip: Option<IpAddr>,
        user_agent: Option<&str>,
    ) -> Result<(), RepoError> {
        let row = sqlx::query(
            r#"
            update auth_users
               set password_hash = $1,
                   must_change_password = $2,
                   password_changed_at = $3,
                   updated_at = $3
             where username = $4
         returning id
            "#,
        )
        .bind(password_hash)
        .bind(must_change_password)
        .bind(Utc::now())
        .bind(username)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| RepoError::AuthUserNotFound(username.to_string()))?;
        let user_id: Uuid = row.try_get("id")?;
        self.revoke_user_refresh_sessions(user_id, Utc::now())
            .await?;
        self.insert_security_audit_event(SecurityAuditEventRecord {
            event_type: event_type.to_string(),
            actor: actor.to_string(),
            subject: Some(username.to_string()),
            remote_ip,
            user_agent: user_agent.map(str::to_string),
            payload: json!({}),
        })
        .await?;
        Ok(())
    }

    pub async fn find_auth_user_by_username(
        &self,
        username: &str,
    ) -> Result<Option<AuthUser>, RepoError> {
        sqlx::query(
            r#"
            select
              id,
              username,
              password_hash,
              role,
              enabled,
              must_change_password,
              last_login_at,
              password_changed_at,
              created_at,
              updated_at
            from auth_users
            where username = $1
            "#,
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await?
        .map(|row| AuthUser::from_row(&row))
        .transpose()
    }

    pub async fn touch_auth_user_login(
        &self,
        user_id: Uuid,
        logged_in_at: DateTime<Utc>,
    ) -> Result<(), RepoError> {
        sqlx::query(
            r#"
            update auth_users
               set last_login_at = $1,
                   updated_at = $1
             where id = $2
            "#,
        )
        .bind(logged_in_at)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn insert_refresh_session(&self, record: NewRefreshSession) -> Result<(), RepoError> {
        sqlx::query(
            r#"
            insert into auth_refresh_sessions (
              id, user_id, token_hash, expires_at, revoked_at, created_at,
              updated_at, last_used_at, client_ip, user_agent
            ) values (
              $1, $2, $3, $4, null, $5,
              $5, null, $6::inet, $7
            )
            "#,
        )
        .bind(record.id)
        .bind(record.user_id)
        .bind(record.token_hash)
        .bind(record.expires_at)
        .bind(record.created_at)
        .bind(record.client_ip.map(|value| value.to_string()))
        .bind(record.user_agent.as_deref())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn find_refresh_session(
        &self,
        token_hash: &str,
    ) -> Result<Option<RefreshSession>, RepoError> {
        sqlx::query(
            r#"
            select
              rs.id,
              rs.user_id,
              rs.token_hash,
              rs.expires_at,
              rs.revoked_at,
              rs.created_at as session_created_at,
              rs.updated_at as session_updated_at,
              rs.last_used_at,
              rs.client_ip::text as client_ip,
              rs.user_agent,
              u.username,
              u.password_hash,
              u.role,
              u.enabled,
              u.must_change_password,
              u.last_login_at,
              u.password_changed_at,
              u.created_at as user_created_at,
              u.updated_at as user_updated_at
            from auth_refresh_sessions rs
            join auth_users u on u.id = rs.user_id
            where rs.token_hash = $1
            "#,
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?
        .map(|row| RefreshSession::from_row(&row))
        .transpose()
    }

    pub async fn rotate_refresh_session(
        &self,
        session_id: Uuid,
        token_hash: &str,
        expires_at: DateTime<Utc>,
        used_at: DateTime<Utc>,
        client_ip: Option<IpAddr>,
        user_agent: Option<&str>,
    ) -> Result<(), RepoError> {
        sqlx::query(
            r#"
            update auth_refresh_sessions
               set token_hash = $1,
                   expires_at = $2,
                   updated_at = $3,
                   last_used_at = $3,
                   client_ip = $4::inet,
                   user_agent = $5
             where id = $6
            "#,
        )
        .bind(token_hash)
        .bind(expires_at)
        .bind(used_at)
        .bind(client_ip.map(|value| value.to_string()))
        .bind(user_agent)
        .bind(session_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn revoke_refresh_session(
        &self,
        token_hash: &str,
        revoked_at: DateTime<Utc>,
    ) -> Result<bool, RepoError> {
        let result = sqlx::query(
            r#"
            update auth_refresh_sessions
               set revoked_at = coalesce(revoked_at, $1),
                   updated_at = $1
             where token_hash = $2
            "#,
        )
        .bind(revoked_at)
        .bind(token_hash)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn revoke_user_refresh_sessions(
        &self,
        user_id: Uuid,
        revoked_at: DateTime<Utc>,
    ) -> Result<u64, RepoError> {
        let result = sqlx::query(
            r#"
            update auth_refresh_sessions
               set revoked_at = coalesce(revoked_at, $1),
                   updated_at = $1
             where user_id = $2
               and revoked_at is null
            "#,
        )
        .bind(revoked_at)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    pub async fn list_machine_allowlist(&self) -> Result<Vec<MachineAllowlistEntry>, RepoError> {
        sqlx::query(
            r#"
            select
              id,
              cidr::text as cidr,
              description,
              created_at,
              updated_at
            from machine_api_allowlist
            order by cidr asc
            "#,
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| MachineAllowlistEntry::from_row(&row))
        .collect()
    }

    pub async fn replace_machine_allowlist(
        &self,
        entries: &[MachineAllowlistWrite],
    ) -> Result<(), RepoError> {
        let now = Utc::now();
        let mut tx = self.pool.begin().await?;
        sqlx::query("delete from machine_api_allowlist")
            .execute(&mut *tx)
            .await?;
        for entry in entries {
            sqlx::query(
                r#"
                insert into machine_api_allowlist (id, cidr, description, created_at, updated_at)
                values ($1, $2::cidr, $3, $4, $4)
                "#,
            )
            .bind(Uuid::now_v7())
            .bind(&entry.cidr)
            .bind(&entry.description)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn is_machine_ip_allowlisted(&self, ip: IpAddr) -> Result<bool, RepoError> {
        Ok(sqlx::query_scalar(
            r#"
            select exists (
              select 1
                from machine_api_allowlist
               where $1::inet <<= cidr
            )
            "#,
        )
        .bind(ip.to_string())
        .fetch_one(&self.pool)
        .await?)
    }

    pub async fn insert_security_audit_event(
        &self,
        record: SecurityAuditEventRecord,
    ) -> Result<(), RepoError> {
        sqlx::query(
            r#"
            insert into security_audit_events (
              id, event_type, actor, subject, remote_ip, user_agent, payload, created_at
            ) values (
              $1, $2, $3, $4, $5::inet, $6, $7, $8
            )
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(&record.event_type)
        .bind(&record.actor)
        .bind(record.subject.as_deref())
        .bind(record.remote_ip.map(|value| value.to_string()))
        .bind(record.user_agent.as_deref())
        .bind(&record.payload)
        .bind(Utc::now())
        .execute(&self.pool)
        .await?;
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

        Ok(TaskDetail {
            task,
            requested_spec,
            resolved_spec,
            current_attempt,
            recent_events,
            callback_delivery,
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
              rf.http_url,
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

    pub async fn list_task_record_files(
        &self,
        task_id: Uuid,
    ) -> Result<Vec<RecordFileSummary>, RepoError> {
        Ok(sqlx::query(
            r#"
            select
              rf.id,
              rf.task_id,
              rf.attempt_id,
              rf.vhost,
              rf.app,
              rf.stream,
              rf.file_path,
              rf.http_url,
              rf.file_size,
              rf.time_len,
              rf.start_time,
              rf.source,
              rf.created_at
            from record_files rf
            where rf.task_id = $1
            order by coalesce(rf.start_time, rf.created_at) desc, rf.id desc
            "#,
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| RecordFileSummary::from_row(&row))
        .collect::<Result<Vec<_>, _>>()?)
    }

    pub async fn list_file_artifacts(
        &self,
        filter: FileArtifactListFilter,
    ) -> Result<Page<FileArtifactSummary>, RepoError> {
        let page = filter.page.unwrap_or(1).max(1);
        let page_size = filter.page_size.unwrap_or(20).clamp(1, 200);
        let total = self.count_file_artifacts(&filter).await?;

        let mut builder = QueryBuilder::<Postgres>::new(
            r#"
            select
              ta.id,
              t.type::text as task_type,
              ta.task_id,
              ta.attempt_id,
              ta.node_id,
              ta.file_name,
              ta.file_path,
              ta.http_url,
              ta.file_size,
              ta.created_at
            from transcode_artifacts ta
            join tasks t on t.id = ta.task_id
            where 1 = 1
            "#,
        );
        apply_file_artifact_filters(&mut builder, &filter);
        builder.push(" order by created_at desc limit ");
        builder.push_bind(i64::from(page_size));
        builder.push(" offset ");
        builder.push_bind(i64::from((page - 1) * page_size));

        let rows = builder
            .build()
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|row| FileArtifactSummary::from_row(&row))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Page::new(rows, page, page_size, total))
    }

    pub async fn list_task_file_artifacts(
        &self,
        task_id: Uuid,
    ) -> Result<Vec<FileArtifactSummary>, RepoError> {
        Ok(sqlx::query(
            r#"
            select
              ta.id,
              t.type::text as task_type,
              ta.task_id,
              ta.attempt_id,
              ta.node_id,
              ta.file_name,
              ta.file_path,
              ta.http_url,
              ta.file_size,
              ta.created_at
            from transcode_artifacts ta
            join tasks t on t.id = ta.task_id
            where ta.task_id = $1
            order by created_at desc, id desc
            "#,
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| FileArtifactSummary::from_row(&row))
        .collect::<Result<Vec<_>, _>>()?)
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
              c.gpu_devices,
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

    pub async fn list_node_heartbeats(
        &self,
        node_id: Uuid,
        limit: u32,
    ) -> Result<Vec<NodeHeartbeatSummary>, RepoError> {
        let limit = limit.clamp(1, 200);
        Ok(sqlx::query(
            r#"
            select
              node_id,
              cpu_percent,
              mem_percent,
              disk_percent,
              running_tasks,
              slot_usage,
              zlm_alive,
              ffmpeg_alive,
              gpu_runtime,
              node_time,
              received_at
            from node_heartbeats
            where node_id = $1
            order by received_at desc, node_time desc
            limit $2
            "#,
        )
        .bind(node_id)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| NodeHeartbeatSummary::from_row(&row))
        .collect::<Result<Vec<_>, _>>()?)
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

    pub async fn list_hook_events(
        &self,
        filter: HookEventListFilter,
    ) -> Result<Vec<HookEventSummary>, RepoError> {
        let mut builder = QueryBuilder::<Postgres>::new(
            r#"
            select
              id,
              server_id,
              hook_name,
              dedup_key,
              payload,
              received_at,
              processed_at
            from hook_events
            where 1 = 1
            "#,
        );
        if let Some(node_id) = filter.node_id {
            builder.push(" and server_id = ");
            builder.push_bind(node_id.to_string());
        }
        if let Some(hook_name) = filter
            .hook_name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            builder.push(" and hook_name = ");
            builder.push_bind(hook_name);
        }
        builder.push(" order by received_at desc, id desc limit ");
        builder.push_bind(i64::from(filter.limit.unwrap_or(50).clamp(1, 200)));

        Ok(builder
            .build()
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|row| HookEventSummary::from_row(&row))
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub async fn list_due_callback_jobs(
        &self,
        now: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<CallbackOutboxJob>, RepoError> {
        let limit = limit.clamp(1, 100);
        Ok(sqlx::query(
            r#"
            select
              id,
              task_id,
              attempt_id,
              attempt_no,
              callback_url,
              event_type,
              reason,
              delivery_attempts
            from task_callback_outbox
            where status in ('pending', 'retrying')
              and deliver_after <= $1
            order by deliver_after asc, created_at asc, id asc
            limit $2
            "#,
        )
        .bind(now)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| CallbackOutboxJob::from_row(&row))
        .collect::<Result<Vec<_>, _>>()?)
    }

    pub async fn mark_callback_delivered(
        &self,
        job: &CallbackOutboxJob,
        http_status: i32,
        response_body: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<(), RepoError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"
            update task_callback_outbox
               set status = 'delivered',
                   delivery_attempts = delivery_attempts + 1,
                   last_http_status = $1,
                   last_error = null,
                   last_response_body = $2,
                   updated_at = $3,
                   delivered_at = $3
             where id = $4
            "#,
        )
        .bind(http_status)
        .bind(response_body.clone())
        .bind(now)
        .bind(job.id)
        .execute(&mut *tx)
        .await?;
        self.insert_event(
            &mut tx,
            job.task_id,
            job.attempt_id,
            Some(job.attempt_no),
            EventSource::Core,
            "callback_delivered",
            "info",
            json!({
                "callback_url": job.callback_url,
                "event_type": job.event_type,
                "reason": job.reason,
                "http_status": http_status,
                "response_body": response_body,
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn schedule_callback_retry(
        &self,
        job: &CallbackOutboxJob,
        http_status: Option<i32>,
        response_body: Option<String>,
        last_error: String,
        retry_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<(), RepoError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"
            update task_callback_outbox
               set status = 'retrying',
                   delivery_attempts = delivery_attempts + 1,
                   last_http_status = $1,
                   last_error = $2,
                   last_response_body = $3,
                   deliver_after = $4,
                   updated_at = $5
             where id = $6
            "#,
        )
        .bind(http_status)
        .bind(&last_error)
        .bind(response_body.clone())
        .bind(retry_at)
        .bind(now)
        .bind(job.id)
        .execute(&mut *tx)
        .await?;
        self.insert_event(
            &mut tx,
            job.task_id,
            job.attempt_id,
            Some(job.attempt_no),
            EventSource::Core,
            "callback_retry_scheduled",
            "warn",
            json!({
                "callback_url": job.callback_url,
                "event_type": job.event_type,
                "reason": job.reason,
                "http_status": http_status,
                "response_body": response_body,
                "last_error": last_error,
                "retry_at": retry_at,
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn mark_callback_dead(
        &self,
        job: &CallbackOutboxJob,
        http_status: Option<i32>,
        response_body: Option<String>,
        last_error: impl Into<String>,
        now: DateTime<Utc>,
    ) -> Result<(), RepoError> {
        let last_error = last_error.into();
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"
            update task_callback_outbox
               set status = 'dead',
                   delivery_attempts = delivery_attempts + 1,
                   last_http_status = $1,
                   last_error = $2,
                   last_response_body = $3,
                   updated_at = $4
             where id = $5
            "#,
        )
        .bind(http_status)
        .bind(&last_error)
        .bind(response_body.clone())
        .bind(now)
        .bind(job.id)
        .execute(&mut *tx)
        .await?;
        self.insert_event(
            &mut tx,
            job.task_id,
            job.attempt_id,
            Some(job.attempt_no),
            EventSource::Core,
            "callback_dead_lettered",
            "error",
            json!({
                "callback_url": job.callback_url,
                "event_type": job.event_type,
                "reason": job.reason,
                "http_status": http_status,
                "response_body": response_body,
                "last_error": last_error,
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

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

    async fn resolve_requested_task(
        &self,
        requested_spec: &TaskSpec,
    ) -> Result<ResolvedTaskRequest, RepoError> {
        let overlay = task_spec_overlay(requested_spec);
        let merged_json = build_resolved_task_json(requested_spec.task_type, &overlay)?;
        let merged_spec: TaskSpec = serde_json::from_value(merged_json)?;
        merged_spec.validate()?;
        validate_task_callback_url(&merged_spec)?;
        validate_managed_file_publish_target(&merged_spec)?;
        let resolved_spec = merged_spec.resolved();
        resolved_spec.validate()?;
        validate_task_callback_url(&resolved_spec)?;
        validate_managed_file_publish_target(&resolved_spec)?;

        Ok(ResolvedTaskRequest {
            requested_spec: merged_spec,
            resolved_spec,
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
        self.enqueue_retry(
            current,
            EventSource::User,
            "task_retry_requested",
            json!({}),
        )
        .await
    }

    async fn enqueue_retry(
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

    pub async fn recover_tasks_for_disconnected_node(
        &self,
        node_id: Uuid,
    ) -> Result<(), RepoError> {
        let rows = sqlx::query(
            r#"
            select id, status::text as status, current_attempt_no, resolved_spec
              from tasks
             where assigned_node_id = $1
               and current_attempt_no > 0
               and status in ('DISPATCHING', 'STARTING', 'RUNNING', 'RECOVERING')
             order by updated_at asc
            "#,
        )
        .bind(node_id)
        .fetch_all(&self.pool)
        .await?;

        let mut retry_candidates = Vec::new();

        for row in rows {
            let task_id: Uuid = row.try_get("id")?;
            let attempt_no: i32 = row.try_get("current_attempt_no")?;
            let status = TaskStatus::from_str(&row.try_get::<String, _>("status")?)?;
            let resolved_spec: Option<Value> = row.try_get("resolved_spec")?;

            match status {
                TaskStatus::Dispatching => {
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
                               failure_code = 'node_disconnected',
                               failure_reason = $2,
                               ended_at = $3
                         where task_id = $4
                           and attempt_no = $5
                           and status = 'PENDING'::attempt_status
                        "#,
                    )
                    .bind(node_id)
                    .bind("control-plane session closed before dispatch was acknowledged")
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
                        "task_requeued_after_node_disconnect",
                        "warn",
                        json!({
                            "node_id": node_id,
                            "attempt_no": attempt_no,
                            "requeued": true,
                        }),
                    )
                    .await?;
                    tx.commit().await?;
                }
                TaskStatus::Starting | TaskStatus::Running | TaskStatus::Recovering => {
                    let should_retry = resolved_spec
                        .as_ref()
                        .and_then(|value| serde_json::from_value::<TaskSpec>(value.clone()).ok())
                        .is_some_and(|spec| retry_enabled_on_disconnect(&spec));
                    let mut tx = self.pool.begin().await?;
                    let now = Utc::now();
                    self.mark_task_lost(
                        &mut tx,
                        task_id,
                        attempt_no,
                        node_id,
                        "node_disconnected",
                        "control-plane session closed before task completed",
                        now,
                    )
                    .await?;
                    self.insert_event(
                        &mut tx,
                        task_id,
                        None,
                        Some(attempt_no),
                        EventSource::Core,
                        "task_lost_after_node_disconnect",
                        "warn",
                        json!({
                            "node_id": node_id,
                            "attempt_no": attempt_no,
                            "auto_retry": should_retry,
                        }),
                    )
                    .await?;
                    tx.commit().await?;

                    if should_retry {
                        retry_candidates.push(task_id);
                    }
                }
                _ => {}
            }
        }

        for task_id in retry_candidates {
            let current = self.fetch_task_summary(task_id).await?;
            if current.status == TaskStatus::Lost {
                self.enqueue_retry(
                    current,
                    EventSource::Core,
                    "task_retry_after_node_disconnect",
                    json!({
                        "reason": "node_disconnected",
                        "auto_retry": true,
                    }),
                )
                .await?;
            }
        }

        Ok(())
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
            sqlx::query(
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

        self.enqueue_task_completed_callback(
            &mut tx,
            task_id,
            attempt_no,
            "terminal_state",
            now + self.callback_settle_delay,
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
        let mut tx = self.pool.begin().await?;
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
        .execute(&mut *tx)
        .await?;

        if result.rows_affected() == 0 {
            return Err(RepoError::NodeNotFound(node_id));
        }

        sqlx::query(
            r#"
            insert into node_heartbeats (
              id, node_id, cpu_percent, mem_percent, disk_percent, running_tasks,
              slot_usage, zlm_alive, ffmpeg_alive, gpu_runtime, node_time, received_at
            ) values (
              $1, $2, $3, $4, $5, $6,
              $7, $8, $9, $10, $11, $12
            )
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(node_id)
        .bind(heartbeat.cpu_percent)
        .bind(heartbeat.mem_percent)
        .bind(heartbeat.disk_percent)
        .bind(i32::try_from(heartbeat.running_tasks).unwrap_or(i32::MAX))
        .bind(heartbeat.slot_usage)
        .bind(heartbeat.zlm_alive)
        .bind(heartbeat.ffmpeg_alive)
        .bind(serde_json::to_value(&heartbeat.gpu_runtime)?)
        .bind(heartbeat.node_time)
        .bind(Utc::now())
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

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
              ffmpeg_decoders, zlm_api_list, zlm_version, gpu, gpu_devices, captured_at
            ) values (
              $1, $2, $3, $4,
              $5, $6, $7, $8, $9, $10
            )
            on conflict (node_id) do update
               set ffmpeg_protocols = excluded.ffmpeg_protocols,
                   ffmpeg_formats = excluded.ffmpeg_formats,
                   ffmpeg_encoders = excluded.ffmpeg_encoders,
                   ffmpeg_decoders = excluded.ffmpeg_decoders,
                   zlm_api_list = excluded.zlm_api_list,
                   zlm_version = excluded.zlm_version,
                   gpu = excluded.gpu,
                   gpu_devices = excluded.gpu_devices,
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
        .bind(serde_json::to_value(&snapshot.gpu_devices)?)
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
                self.promote_task_running(&mut tx, event.task_id, event.attempt_no, node_id, now)
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
        self.upsert_file_artifacts_from_snapshot(&mut tx, node_id, &snapshot)
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

        let Some(agent_stream_addr) = sqlx::query_scalar::<_, String>(
            r#"
            select agent_stream_addr
              from media_nodes
             where id = $1
            "#,
        )
        .bind(node_id)
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
            self.upsert_file_artifact_row(
                tx,
                snapshot.task_id,
                attempt_id,
                node_id,
                &agent_stream_addr,
                metadata,
            )
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
            self.upsert_file_artifact_row(
                tx,
                snapshot.task_id,
                attempt_id,
                node_id,
                &agent_stream_addr,
                metadata,
            )
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
                self.upsert_file_artifact_row(
                    tx,
                    snapshot.task_id,
                    attempt_id,
                    node_id,
                    &agent_stream_addr,
                    metadata,
                )
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
        agent_stream_addr: &str,
        metadata: FileArtifactMetadata,
    ) -> Result<(), RepoError> {
        let http_url = artifact_http_url_from_path(agent_stream_addr, metadata.file_path.as_str())?;

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
            if should_persist_record_file_hook(hook_name, &binding, &record)? {
                let http_url = resolve_record_http_url(&mut tx, server_id, &record).await?;
                sqlx::query(
                    r#"
                    insert into record_files (
                      id, task_id, attempt_id, vhost, app, stream, file_path, http_url, file_size,
                      time_len, start_time, source, created_at
                    ) values (
                      $1, $2, $3, $4, $5, $6, $7, $8, $9,
                      $10, $11, 'hook', $12
                    )
                    on conflict (file_path) do update
                       set task_id = excluded.task_id,
                           attempt_id = excluded.attempt_id,
                           vhost = excluded.vhost,
                           app = excluded.app,
                           stream = excluded.stream,
                           http_url = excluded.http_url,
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
                .bind(http_url.as_deref())
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
                        "url": http_url.clone().or(record.url.clone()),
                        "http_url": http_url,
                        "file_size": record.file_size,
                        "time_len": record.time_len_sec,
                        "start_time": record.start_time,
                        "source": "hook",
                    }),
                )
                .await?;

                self.enqueue_artifact_update_callback_if_needed(
                    &mut tx,
                    binding.task_id,
                    binding.attempt_no,
                )
                .await?;
            }
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
            if publish_stream_matches(task_id, attempt_no, &spec, app, stream)
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

    async fn count_file_artifacts(
        &self,
        filter: &FileArtifactListFilter,
    ) -> Result<u64, RepoError> {
        let mut builder = QueryBuilder::<Postgres>::new(
            "select count(*) as total from transcode_artifacts ta join tasks t on t.id = ta.task_id where 1 = 1",
        );
        apply_file_artifact_filters(&mut builder, filter);

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
              ta.attempt_no,
              t.resolved_spec
            from stream_bindings sb
            join task_attempts ta
              on ta.id = sb.attempt_id
            join tasks t
              on t.id = sb.task_id
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

    async fn enqueue_task_completed_callback(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        task_id: Uuid,
        attempt_no: i32,
        reason: &str,
        deliver_after: DateTime<Utc>,
    ) -> Result<(), RepoError> {
        let Some((attempt_id, callback_url)) = self
            .callback_target_for_attempt(tx, task_id, attempt_no)
            .await?
        else {
            return Ok(());
        };
        let result = self
            .enqueue_callback_job(
                tx,
                task_id,
                attempt_id,
                attempt_no,
                &callback_url,
                "task.completed",
                reason,
                deliver_after,
            )
            .await?;

        if result.rows_affected() > 0 {
            self.insert_event(
                tx,
                task_id,
                attempt_id,
                Some(attempt_no),
                EventSource::Core,
                "callback_enqueued",
                "info",
                json!({
                    "callback_url": callback_url,
                    "event_type": "task.completed",
                    "reason": reason,
                    "deliver_after": deliver_after,
                }),
            )
            .await?;
        }

        Ok(())
    }

    async fn enqueue_task_status_callback_if_needed(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        task_id: Uuid,
        attempt_no: i32,
        reason: &str,
        deliver_after: DateTime<Utc>,
    ) -> Result<(), RepoError> {
        let event_type = "task.status";
        if self
            .callback_job_exists(tx, task_id, attempt_no, event_type, reason)
            .await?
        {
            return Ok(());
        }

        let Some((attempt_id, callback_url)) = self
            .callback_target_for_attempt(tx, task_id, attempt_no)
            .await?
        else {
            return Ok(());
        };

        let result = self
            .enqueue_callback_job(
                tx,
                task_id,
                attempt_id,
                attempt_no,
                &callback_url,
                event_type,
                reason,
                deliver_after,
            )
            .await?;

        if result.rows_affected() > 0 {
            self.insert_event(
                tx,
                task_id,
                attempt_id,
                Some(attempt_no),
                EventSource::Core,
                "callback_enqueued",
                "info",
                json!({
                    "callback_url": callback_url,
                    "event_type": event_type,
                    "reason": reason,
                    "deliver_after": deliver_after,
                }),
            )
            .await?;
        }

        Ok(())
    }

    async fn enqueue_artifact_update_callback_if_needed(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        task_id: Uuid,
        attempt_no: i32,
    ) -> Result<(), RepoError> {
        let task_status: Option<String> = sqlx::query_scalar(
            r#"
            select status::text
              from tasks
             where id = $1
            "#,
        )
        .bind(task_id)
        .fetch_optional(&mut **tx)
        .await?;
        let Some(task_status) = task_status else {
            return Ok(());
        };
        if !matches!(
            task_status.as_str(),
            "SUCCEEDED" | "FAILED" | "CANCELED" | "LOST"
        ) {
            return Ok(());
        }

        let terminal_callback_delivered: bool = sqlx::query_scalar(
            r#"
            select exists (
              select 1
                from task_callback_outbox
               where task_id = $1
                 and attempt_no = $2
                 and event_type = 'task.completed'
                 and reason = 'terminal_state'
                 and status = 'delivered'
            )
            "#,
        )
        .bind(task_id)
        .bind(attempt_no)
        .fetch_one(&mut **tx)
        .await?;
        if !terminal_callback_delivered {
            return Ok(());
        }

        self.enqueue_task_completed_callback(tx, task_id, attempt_no, "artifact_update", Utc::now())
            .await
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

    async fn callback_target_for_attempt(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        task_id: Uuid,
        attempt_no: i32,
    ) -> Result<Option<(Option<Uuid>, String)>, RepoError> {
        let row = sqlx::query(
            r#"
            select
              ta.id as attempt_id,
              coalesce(
                nullif(t.resolved_spec->'common'->>'callback_url', ''),
                nullif(t.requested_spec->'common'->>'callback_url', '')
              ) as callback_url
            from tasks t
            left join task_attempts ta
              on ta.task_id = t.id
             and ta.attempt_no = $2
            where t.id = $1
            "#,
        )
        .bind(task_id)
        .bind(attempt_no)
        .fetch_optional(&mut **tx)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let callback_url = row
            .try_get::<Option<String>, _>("callback_url")?
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let Some(callback_url) = callback_url else {
            return Ok(None);
        };
        let attempt_id: Option<Uuid> = row.try_get("attempt_id")?;
        Ok(Some((attempt_id, callback_url)))
    }

    async fn callback_job_exists(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        task_id: Uuid,
        attempt_no: i32,
        event_type: &str,
        reason: &str,
    ) -> Result<bool, RepoError> {
        sqlx::query_scalar(
            r#"
            select exists (
              select 1
                from task_callback_outbox
               where task_id = $1
                 and attempt_no = $2
                 and event_type = $3
                 and reason = $4
            )
            "#,
        )
        .bind(task_id)
        .bind(attempt_no)
        .bind(event_type)
        .bind(reason)
        .fetch_one(&mut **tx)
        .await
        .map_err(Into::into)
    }

    async fn enqueue_callback_job(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        task_id: Uuid,
        attempt_id: Option<Uuid>,
        attempt_no: i32,
        callback_url: &str,
        event_type: &str,
        reason: &str,
        deliver_after: DateTime<Utc>,
    ) -> Result<PgQueryResult, RepoError> {
        sqlx::query(
            r#"
            insert into task_callback_outbox (
              id, task_id, attempt_id, attempt_no, callback_url, event_type, reason,
              status, delivery_attempts, deliver_after, created_at, updated_at
            ) values (
              $1, $2, $3, $4, $5, $6, $7,
              'pending', 0, $8, $9, $9
            )
            on conflict do nothing
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(task_id)
        .bind(attempt_id)
        .bind(attempt_no)
        .bind(callback_url)
        .bind(event_type)
        .bind(reason)
        .bind(deliver_after)
        .bind(Utc::now())
        .execute(&mut **tx)
        .await
        .map_err(Into::into)
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
        self.enqueue_task_completed_callback(
            tx,
            task_id,
            attempt_no,
            "terminal_state",
            now + self.callback_settle_delay,
        )
        .await?;
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

        self.enqueue_task_status_callback_if_needed(tx, task_id, attempt_no, "running", now)
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
        self.enqueue_task_completed_callback(
            tx,
            task_id,
            attempt_no,
            "terminal_state",
            now + self.callback_settle_delay,
        )
        .await?;
        Ok(())
    }
}

fn retry_enabled_on_disconnect(spec: &TaskSpec) -> bool {
    !matches!(
        spec.recovery
            .policy
            .unwrap_or(RecoveryPolicy::default_for(spec.task_type)),
        RecoveryPolicy::Never
    )
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

#[derive(Debug, Clone, Serialize)]
pub struct TaskPreview {
    pub requested_spec: Value,
    pub resolved_spec: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StreamListFilter {
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub viewer_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitrate_kbps: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub play_urls: Vec<String>,
}

impl StreamSummary {
    fn from_row(row: &PgRow) -> Result<Self, RepoError> {
        Ok(Self {
            id: row.try_get("id")?,
            task_id: row.try_get("task_id")?,
            attempt_id: row.try_get("attempt_id")?,
            attempt_no: row.try_get("attempt_no")?,
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
            viewer_count: None,
            bitrate_kbps: None,
            play_urls: Vec::new(),
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RecordListFilter {
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
    pub http_url: Option<String>,
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
            http_url: row.try_get("http_url")?,
            file_size: row.try_get("file_size")?,
            time_len: row.try_get("time_len")?,
            start_time: row.try_get("start_time")?,
            source: row.try_get("source")?,
            created_at: row.try_get("created_at")?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileArtifactKind {
    TranscodeOutput,
    BridgeOutput,
    StreamIngestRecord,
}

impl FileArtifactKind {
    fn from_task_type(value: &str) -> Result<Self, RepoError> {
        match value {
            "file_transcode" => Ok(Self::TranscodeOutput),
            "stream_bridge" => Ok(Self::BridgeOutput),
            "stream_ingest" => Ok(Self::StreamIngestRecord),
            other => Err(RepoError::Validation(TaskValidationError {
                issues: vec![ValidationIssue::new(
                    "artifact_kind",
                    format!("unsupported file artifact task type: {other}"),
                )],
            })),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct FileArtifactListFilter {
    #[serde(default)]
    pub task_id: Option<Uuid>,
    #[serde(default)]
    pub artifact_kind: Option<FileArtifactKind>,
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
pub struct FileArtifactSummary {
    pub id: Uuid,
    pub artifact_kind: FileArtifactKind,
    pub task_id: Uuid,
    pub attempt_id: Option<Uuid>,
    pub node_id: Uuid,
    pub file_name: String,
    pub file_path: String,
    pub http_url: String,
    pub file_size: i64,
    pub created_at: DateTime<Utc>,
}

impl FileArtifactSummary {
    fn from_row(row: &PgRow) -> Result<Self, RepoError> {
        Ok(Self {
            id: row.try_get("id")?,
            artifact_kind: FileArtifactKind::from_task_type(row.try_get::<&str, _>("task_type")?)?,
            task_id: row.try_get("task_id")?,
            attempt_id: row.try_get("attempt_id")?,
            node_id: row.try_get("node_id")?,
            file_name: row.try_get("file_name")?,
            file_path: row.try_get("file_path")?,
            http_url: row.try_get("http_url")?,
            file_size: row.try_get("file_size")?,
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
    pub gpu_devices: Vec<GpuDeviceInfo>,
    pub capability_captured_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot_usage: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub running_tasks: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connected: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zlm_alive: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ffmpeg_alive: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu_runtime: Option<Vec<GpuRuntimeStats>>,
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
            gpu_devices: row
                .try_get::<Option<Value>, _>("gpu_devices")?
                .map(serde_json::from_value)
                .transpose()?
                .unwrap_or_default(),
            capability_captured_at: row.try_get("captured_at")?,
            slot_usage: None,
            running_tasks: None,
            connected: None,
            cpu_percent: None,
            mem_percent: None,
            disk_percent: None,
            zlm_alive: None,
            ffmpeg_alive: None,
            gpu_runtime: None,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeHeartbeatSummary {
    pub node_id: Uuid,
    pub cpu_percent: f64,
    pub mem_percent: f64,
    pub disk_percent: f64,
    pub running_tasks: u32,
    pub slot_usage: f64,
    pub zlm_alive: bool,
    pub ffmpeg_alive: bool,
    pub gpu_runtime: Vec<GpuRuntimeStats>,
    pub node_time: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
}

impl NodeHeartbeatSummary {
    fn from_row(row: &PgRow) -> Result<Self, RepoError> {
        let running_tasks = row.try_get::<i32, _>("running_tasks")?;
        Ok(Self {
            node_id: row.try_get("node_id")?,
            cpu_percent: row.try_get("cpu_percent")?,
            mem_percent: row.try_get("mem_percent")?,
            disk_percent: row.try_get("disk_percent")?,
            running_tasks: u32::try_from(running_tasks).unwrap_or_default(),
            slot_usage: row.try_get("slot_usage")?,
            zlm_alive: row.try_get("zlm_alive")?,
            ffmpeg_alive: row.try_get("ffmpeg_alive")?,
            gpu_runtime: row
                .try_get::<Option<Value>, _>("gpu_runtime")?
                .map(serde_json::from_value)
                .transpose()?
                .unwrap_or_default(),
            node_time: row.try_get("node_time")?,
            received_at: row.try_get("received_at")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct NodeDebugTarget {
    pub zlm_api_base: String,
    pub zlm_api_secret: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HookEventListFilter {
    #[serde(default)]
    pub node_id: Option<Uuid>,
    #[serde(default)]
    pub hook_name: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HookEventSummary {
    pub id: Uuid,
    pub server_id: String,
    pub hook_name: String,
    pub dedup_key: String,
    pub payload: Value,
    pub received_at: DateTime<Utc>,
    pub processed_at: Option<DateTime<Utc>>,
}

impl HookEventSummary {
    fn from_row(row: &PgRow) -> Result<Self, RepoError> {
        Ok(Self {
            id: row.try_get("id")?,
            server_id: row.try_get("server_id")?,
            hook_name: row.try_get("hook_name")?,
            dedup_key: row.try_get("dedup_key")?,
            payload: row.try_get("payload")?,
            received_at: row.try_get("received_at")?,
            processed_at: row.try_get("processed_at")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct CronScheduleEntry {
    pub task_id: Uuid,
    pub requested_spec: Value,
    pub created_at: DateTime<Utc>,
    pub last_scheduled_for: Option<DateTime<Utc>>,
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

fn apply_file_artifact_filters<'a>(
    builder: &mut QueryBuilder<'a, Postgres>,
    filter: &'a FileArtifactListFilter,
) {
    if let Some(task_id) = filter.task_id {
        builder.push(" and ta.task_id = ");
        builder.push_bind(task_id);
    }
    if let Some(artifact_kind) = filter.artifact_kind {
        builder.push(" and t.type = ");
        builder.push_bind(match artifact_kind {
            FileArtifactKind::TranscodeOutput => "file_transcode",
            FileArtifactKind::BridgeOutput => "stream_bridge",
            FileArtifactKind::StreamIngestRecord => "stream_ingest",
        });
        builder.push("::task_type");
    }
    if let Some(date_from) = filter.date_from {
        builder.push(" and ta.created_at >= ");
        builder.push_bind(date_from);
    }
    if let Some(date_to) = filter.date_to {
        builder.push(" and ta.created_at <= ");
        builder.push_bind(date_to);
    }
}

const ZLM_HTTP_ROOT: &str = "/data/zlm/www";
const ZLM_RECORD_HTTP_ROOT: &str = "/data/zlm/www/record";
const LEGACY_ZLM_RECORD_ROOT: &str = "/data/zlm/record";

fn relative_path_under_root<'a>(path: &'a str, root: &str) -> Option<&'a str> {
    if path == root {
        return None;
    }
    path.strip_prefix(root)?.strip_prefix('/')
}

fn normalized_absolute_path(path: &str) -> Result<String, RepoError> {
    let path = Path::new(path.trim());
    if !path.is_absolute() {
        return Err(validation_error("publish.url", "must be an absolute path"));
    }

    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(value) => parts.push(value.to_string_lossy().to_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(validation_error(
                    "publish.url",
                    "must not contain parent segments",
                ));
            }
            Component::Prefix(_) => {
                return Err(validation_error("publish.url", "must be a POSIX path"));
            }
        }
    }

    Ok(format!("/{}", parts.join("/")))
}

fn validate_managed_file_publish_target(spec: &TaskSpec) -> Result<(), RepoError> {
    if !matches!(spec.publish.kind, Some(PublishTargetKind::File)) {
        return Ok(());
    }

    if spec
        .publish
        .url
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
    {
        return Err(validation_error(
            "publish.url",
            "must not be provided for file output; output path is managed by the platform",
        ));
    }
    Ok(())
}

fn validate_task_callback_url(spec: &TaskSpec) -> Result<(), RepoError> {
    let Some(callback_url) = spec
        .common
        .callback_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(());
    };

    let url = Url::parse(callback_url).map_err(|_| {
        validation_error(
            "common.callback_url",
            "must be an absolute http:// or https:// URL",
        )
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(validation_error(
            "common.callback_url",
            "must use http:// or https://",
        ));
    }
    if url.host_str().is_none() {
        return Err(validation_error(
            "common.callback_url",
            "must include a host",
        ));
    }
    Ok(())
}

fn artifact_http_url_from_path(
    agent_stream_addr: &str,
    file_path: &str,
) -> Result<String, RepoError> {
    let normalized = normalized_absolute_path(file_path)?;
    let relative = relative_path_under_root(&normalized, ZLM_HTTP_ROOT).ok_or_else(|| {
        validation_error("publish.url", "output path must be under /data/zlm/www")
    })?;
    absolute_http_url_from_relative(agent_stream_addr, relative).ok_or_else(|| {
        validation_error(
            "publish.url",
            format!("failed to build artifact URL from {file_path}"),
        )
    })
}

fn absolute_http_url_from_relative(agent_stream_addr: &str, relative: &str) -> Option<String> {
    let base = Url::parse(agent_stream_addr)
        .map_err(|error| {
            tracing::warn!(
                %agent_stream_addr,
                %error,
                "invalid node stream base while building HTTP URL"
            );
        })
        .ok()?;
    base.join(relative).ok().map(|value| value.to_string())
}

fn record_http_url_from_path(agent_stream_addr: &str, file_path: &str) -> Option<String> {
    let normalized = normalized_absolute_path(file_path).ok()?;
    if let Some(relative) = relative_path_under_root(&normalized, ZLM_HTTP_ROOT) {
        return absolute_http_url_from_relative(agent_stream_addr, relative);
    }
    let relative = relative_path_under_root(&normalized, LEGACY_ZLM_RECORD_ROOT)?;
    let record_relative_root =
        relative_path_under_root(ZLM_RECORD_HTTP_ROOT, ZLM_HTTP_ROOT).unwrap_or("record");
    let translated = format!(
        "{}/{}",
        record_relative_root,
        relative.trim_start_matches('/')
    );
    absolute_http_url_from_relative(agent_stream_addr, &translated)
}

fn resolve_absolute_http_url(agent_stream_addr: &str, value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if Url::parse(trimmed).is_ok() {
        return Some(trimmed.to_string());
    }
    let Ok(base) = Url::parse(agent_stream_addr) else {
        return None;
    };
    base.join(trimmed).ok().map(|value| value.to_string())
}

fn should_persist_record_file_hook(
    hook_name: &str,
    binding: &HookStreamBinding,
    record: &ZlmRecordFileRecord,
) -> Result<bool, RepoError> {
    if record.record_format.as_deref() != Some("hls") {
        return Ok(true);
    }

    if hook_name != "on_record_hls" {
        return Ok(false);
    }

    if !is_hls_playlist_record_path(record.file_path.as_str()) {
        return Ok(false);
    }

    Ok(binding
        .resolved_task_spec()?
        .is_some_and(|spec| spec.record.wants_hls()))
}

fn is_hls_playlist_record_path(file_path: &str) -> bool {
    let Ok(normalized) = normalized_absolute_path(file_path) else {
        return false;
    };
    let in_record_root = relative_path_under_root(&normalized, ZLM_RECORD_HTTP_ROOT).is_some()
        || relative_path_under_root(&normalized, LEGACY_ZLM_RECORD_ROOT).is_some();
    in_record_root
        && Path::new(&normalized)
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("m3u8"))
}

async fn resolve_record_http_url(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    server_id: &str,
    record: &ZlmRecordFileRecord,
) -> Result<Option<String>, RepoError> {
    let Some(node_id) = Uuid::parse_str(server_id.trim()).ok() else {
        return Ok(record.url.as_deref().and_then(|raw_url| {
            Url::parse(raw_url.trim())
                .ok()
                .map(|value| value.to_string())
        }));
    };
    let Some(agent_stream_addr) = sqlx::query_scalar::<_, String>(
        r#"
        select agent_stream_addr
          from media_nodes
         where id = $1
        "#,
    )
    .bind(node_id)
    .fetch_optional(&mut **tx)
    .await?
    else {
        return Ok(record.url.as_deref().and_then(|raw_url| {
            Url::parse(raw_url.trim())
                .ok()
                .map(|value| value.to_string())
        }));
    };

    if let Some(http_url) = record_http_url_from_path(agent_stream_addr.as_str(), &record.file_path)
    {
        return Ok(Some(http_url));
    }

    Ok(record
        .url
        .as_deref()
        .and_then(|raw_url| resolve_absolute_http_url(agent_stream_addr.as_str(), raw_url)))
}

fn build_resolved_task_json(
    task_type: TaskType,
    request_overrides: &Value,
) -> Result<Value, RepoError> {
    let mut merged = json!({});
    if !request_overrides.is_object() {
        return Err(validation_error(
            "task",
            "request payload must be a JSON object",
        ));
    }
    deep_merge(&mut merged, request_overrides.clone());
    merged["type"] = Value::String(task_type.as_str().to_string());
    strip_legacy_dispatch_fields(&mut merged);
    Ok(merged)
}

fn task_spec_overlay(spec: &TaskSpec) -> Value {
    let mut overlay = serde_json::Map::new();
    overlay.insert(
        "type".to_string(),
        Value::String(spec.task_type.as_str().to_string()),
    );
    overlay.insert("name".to_string(), Value::String(spec.name.clone()));
    overlay.insert("priority".to_string(), json!(spec.priority));

    let mut common = serde_json::Map::new();
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
        (
            "source_mode",
            spec.input.source_mode.map(|value| json!(value)),
        ),
        (
            "loop_enabled",
            spec.input.loop_enabled.map(|value| json!(value)),
        ),
        ("url", spec.input.url.as_ref().map(|value| json!(value))),
        ("group", spec.input.group.as_ref().map(|value| json!(value))),
        ("port", spec.input.port.map(|value| json!(value))),
        (
            "interface_name",
            spec.input.interface_name.as_ref().map(|value| json!(value)),
        ),
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
        ("bitrate", spec.process.bitrate.map(|value| json!(value))),
        ("fps", spec.process.fps.map(|value| json!(value))),
        ("gop", spec.process.gop.map(|value| json!(value))),
    ]);
    if let Some(process) = process {
        overlay.insert("process".to_string(), process);
    }

    let stream = overlay_optional_fields(&[
        ("app", spec.stream.app.as_ref().map(|value| json!(value))),
        ("name", spec.stream.name.as_ref().map(|value| json!(value))),
        (
            "vhost",
            spec.stream.vhost.as_ref().map(|value| json!(value)),
        ),
    ]);
    if let Some(stream) = stream {
        overlay.insert("stream".to_string(), stream);
    }

    let expose = overlay_optional_fields(&[
        (
            "enable_rtsp",
            spec.expose.enable_rtsp.map(|value| json!(value)),
        ),
        (
            "enable_rtmp",
            spec.expose.enable_rtmp.map(|value| json!(value)),
        ),
        (
            "enable_http_ts",
            spec.expose.enable_http_ts.map(|value| json!(value)),
        ),
        (
            "enable_http_fmp4",
            spec.expose.enable_http_fmp4.map(|value| json!(value)),
        ),
        (
            "enable_hls",
            spec.expose.enable_hls.map(|value| json!(value)),
        ),
        (
            "stop_on_no_reader",
            spec.expose.stop_on_no_reader.map(|value| json!(value)),
        ),
    ]);
    if let Some(expose) = expose {
        overlay.insert("expose".to_string(), expose);
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
            "interface_name",
            spec.publish
                .interface_name
                .as_ref()
                .map(|value| json!(value)),
        ),
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
    ]);
    if let Some(publish) = publish {
        overlay.insert("publish".to_string(), publish);
    }

    let record = overlay_optional_fields(&[
        ("enabled", spec.record.enabled.map(|value| json!(value))),
        ("format", spec.record.format.map(|value| json!(value))),
        (
            "duration_sec",
            spec.record.duration_sec.map(|value| json!(value)),
        ),
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
    if !resource.is_empty() {
        overlay.insert("resource".to_string(), Value::Object(resource));
    }

    Value::Object(overlay)
}

fn strip_legacy_dispatch_fields(value: &mut Value) {
    if let Some(process) = value.get_mut("process").and_then(Value::as_object_mut) {
        process.remove("video_codec");
        process.remove("audio_codec");
        process.remove("profile");
        process.remove("preset");
    }
    if let Some(resource) = value.get_mut("resource").and_then(Value::as_object_mut) {
        resource.remove("need_gpu");
    }
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

#[derive(Debug, Clone, Deserialize)]
struct FileArtifactMetadata {
    file_name: String,
    file_path: String,
    file_size: i64,
}

#[derive(Debug, Clone)]
struct HookStreamBinding {
    task_id: Uuid,
    attempt_id: Uuid,
    attempt_no: i32,
    resolved_spec: Option<Value>,
}

impl HookStreamBinding {
    fn from_row(row: &PgRow) -> Result<Self, RepoError> {
        Ok(Self {
            task_id: row.try_get("task_id")?,
            attempt_id: row.try_get("attempt_id")?,
            attempt_no: row.try_get("attempt_no")?,
            resolved_spec: row.try_get("resolved_spec")?,
        })
    }

    fn resolved_task_spec(&self) -> Result<Option<TaskSpec>, RepoError> {
        self.resolved_spec
            .clone()
            .map(serde_json::from_value::<TaskSpec>)
            .transpose()
            .map_err(RepoError::from)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TaskListFilter {
    #[serde(default)]
    pub status: Option<TaskStatus>,
    #[serde(default, rename = "type")]
    pub task_type: Option<TaskType>,
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

const TASK_TRANSCODE_NONE: &str = "none";
const TASK_TRANSCODE_ADAPTIVE: &str = "adaptive";
const TASK_TRANSCODE_FORCED: &str = "forced";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: Uuid,
    pub name: String,
    #[serde(rename = "type")]
    pub task_type: TaskType,
    pub status: TaskStatus,
    pub priority: u8,
    pub created_by: String,
    pub assigned_node_id: Option<Uuid>,
    pub current_attempt_no: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcode_mode: Option<String>,
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
        let resolved_spec = optional_json_column(row, "resolved_spec")?;
        let transcode_mode = resolved_spec
            .as_ref()
            .and_then(task_summary_transcode_mode_from_value);

        Ok(Self {
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            task_type,
            status,
            priority: u8::try_from(priority).unwrap_or(50),
            created_by: row.try_get("created_by")?,
            assigned_node_id: row.try_get("assigned_node_id")?,
            current_attempt_no: row.try_get("current_attempt_no")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
            started_at: row.try_get("started_at")?,
            finished_at: row.try_get("finished_at")?,
            transcode_mode,
        })
    }
}

fn optional_json_column(row: &PgRow, column: &str) -> Result<Option<Value>, RepoError> {
    match row.try_get::<Option<Value>, _>(column) {
        Ok(value) => Ok(value),
        Err(sqlx::Error::ColumnNotFound(_)) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn task_summary_transcode_mode_from_value(value: &Value) -> Option<String> {
    serde_json::from_value::<TaskSpec>(value.clone())
        .ok()
        .and_then(|spec| task_summary_transcode_mode(&spec).map(str::to_string))
}

fn task_summary_transcode_mode(spec: &TaskSpec) -> Option<&'static str> {
    match task_runtime_mode(spec) {
        TaskRuntimeMode::ZlmProxy | TaskRuntimeMode::ZlmRtpServer => Some(TASK_TRANSCODE_NONE),
        TaskRuntimeMode::ManagedProcess => {
            if should_force_bridge_stabilization_transcode(spec) {
                Some(TASK_TRANSCODE_FORCED)
            } else {
                let default_mode = match spec.task_type {
                    TaskType::StreamBridge => "passthrough",
                    TaskType::StreamIngest | TaskType::FileTranscode => "copy_or_transcode",
                };
                effective_transcode_mode(spec.process.mode.as_deref(), default_mode)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskRuntimeMode {
    ZlmProxy,
    ZlmRtpServer,
    ManagedProcess,
}

fn task_runtime_mode(spec: &TaskSpec) -> TaskRuntimeMode {
    match spec.task_type {
        TaskType::FileTranscode | TaskType::StreamBridge => TaskRuntimeMode::ManagedProcess,
        TaskType::StreamIngest => match (spec.input.kind, spec.input.source_mode) {
            (Some(InputKind::GbRtp), _) => TaskRuntimeMode::ZlmRtpServer,
            (Some(InputKind::Rtsp | InputKind::Rtmp | InputKind::HttpFlv), _) => {
                TaskRuntimeMode::ZlmProxy
            }
            (Some(InputKind::Hls | InputKind::HttpTs), Some(SourceMode::Live)) => {
                TaskRuntimeMode::ZlmProxy
            }
            _ => TaskRuntimeMode::ManagedProcess,
        },
    }
}

fn effective_transcode_mode(mode: Option<&str>, default_mode: &str) -> Option<&'static str> {
    match mode.unwrap_or(default_mode) {
        "passthrough" => Some(TASK_TRANSCODE_NONE),
        "copy_or_transcode" => Some(TASK_TRANSCODE_ADAPTIVE),
        "force_transcode" => Some(TASK_TRANSCODE_FORCED),
        _ => None,
    }
}

fn should_force_bridge_stabilization_transcode(spec: &TaskSpec) -> bool {
    if spec.task_type != TaskType::StreamBridge
        || spec.process.mode.as_deref().unwrap_or("passthrough") != "passthrough"
    {
        return false;
    }

    bridge_output_format(spec).is_some_and(|format| format.eq_ignore_ascii_case("mpegts"))
        && matches!(
            spec.input.kind,
            Some(
                InputKind::Rtsp
                    | InputKind::Rtmp
                    | InputKind::Hls
                    | InputKind::HttpFlv
                    | InputKind::HttpTs
            )
        )
}

fn bridge_output_format(spec: &TaskSpec) -> Option<String> {
    match spec.publish.kind? {
        PublishTargetKind::File => Some(
            spec.publish
                .format
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("mp4")
                .to_ascii_lowercase(),
        ),
        PublishTargetKind::UdpMpegtsMulticast => Some(
            spec.publish
                .format
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("mpegts")
                .to_ascii_lowercase(),
        ),
        PublishTargetKind::RtpMulticast => Some(
            spec.publish
                .format
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("rtp_mpegts")
                .to_ascii_lowercase(),
        ),
        PublishTargetKind::RtmpPush => Some("flv".to_string()),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskDetail {
    pub task: TaskSummary,
    pub requested_spec: Value,
    pub resolved_spec: Option<Value>,
    pub current_attempt: Option<AttemptSummary>,
    pub recent_events: Vec<TaskEventSummary>,
    pub callback_delivery: Option<CallbackDeliverySummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CallbackDeliverySummary {
    pub callback_url: String,
    pub event_type: String,
    pub reason: String,
    pub status: String,
    pub delivery_attempts: u32,
    pub last_http_status: Option<i32>,
    pub last_error: Option<String>,
    pub delivered_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

impl CallbackDeliverySummary {
    fn from_row(row: &PgRow) -> Result<Self, RepoError> {
        Ok(Self {
            callback_url: row.try_get("callback_url")?,
            event_type: row.try_get("event_type")?,
            reason: row.try_get("reason")?,
            status: row.try_get("status")?,
            delivery_attempts: u32::try_from(row.try_get::<i32, _>("delivery_attempts")?)
                .unwrap_or_default(),
            last_http_status: row.try_get("last_http_status")?,
            last_error: row.try_get("last_error")?,
            delivered_at: row.try_get("delivered_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct CallbackOutboxJob {
    pub id: Uuid,
    pub task_id: Uuid,
    pub attempt_id: Option<Uuid>,
    pub attempt_no: i32,
    pub callback_url: String,
    pub event_type: String,
    pub reason: String,
    pub delivery_attempts: u32,
}

impl CallbackOutboxJob {
    fn from_row(row: &PgRow) -> Result<Self, RepoError> {
        Ok(Self {
            id: row.try_get("id")?,
            task_id: row.try_get("task_id")?,
            attempt_id: row.try_get("attempt_id")?,
            attempt_no: row.try_get("attempt_no")?,
            callback_url: row.try_get("callback_url")?,
            event_type: row.try_get("event_type")?,
            reason: row.try_get("reason")?,
            delivery_attempts: u32::try_from(row.try_get::<i32, _>("delivery_attempts")?)
                .unwrap_or_default(),
        })
    }
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

#[derive(Debug, Clone)]
pub struct AuthUser {
    pub id: Uuid,
    pub username: String,
    pub password_hash: String,
    pub role: String,
    pub enabled: bool,
    pub must_change_password: bool,
}

impl AuthUser {
    fn from_row(row: &PgRow) -> Result<Self, RepoError> {
        Ok(Self {
            id: row.try_get("id")?,
            username: row.try_get("username")?,
            password_hash: row.try_get("password_hash")?,
            role: row.try_get("role")?,
            enabled: row.try_get("enabled")?,
            must_change_password: row.try_get("must_change_password")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct RefreshSession {
    pub id: Uuid,
    pub token_hash: String,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub user: AuthUser,
}

impl RefreshSession {
    fn from_row(row: &PgRow) -> Result<Self, RepoError> {
        Ok(Self {
            id: row.try_get("id")?,
            token_hash: row.try_get("token_hash")?,
            expires_at: row.try_get("expires_at")?,
            revoked_at: row.try_get("revoked_at")?,
            user: AuthUser::from_row(row)?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct NewRefreshSession {
    pub id: Uuid,
    pub user_id: Uuid,
    pub token_hash: String,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub client_ip: Option<IpAddr>,
    pub user_agent: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MachineAllowlistEntry {
    pub id: Uuid,
    pub cidr: String,
    pub description: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl MachineAllowlistEntry {
    fn from_row(row: &PgRow) -> Result<Self, RepoError> {
        Ok(Self {
            id: row.try_get("id")?,
            cidr: row.try_get("cidr")?,
            description: row.try_get("description")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MachineAllowlistWrite {
    pub cidr: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct SecurityAuditEventRecord {
    pub event_type: String,
    pub actor: String,
    pub subject: Option<String>,
    pub remote_ip: Option<IpAddr>,
    pub user_agent: Option<String>,
    pub payload: Value,
}

#[derive(Debug, Error)]
pub enum RepoError {
    #[error("task {0} was not found")]
    TaskNotFound(Uuid),
    #[error("auth user {0} was not found")]
    AuthUserNotFound(String),
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

fn publish_stream_matches(
    task_id: Uuid,
    _attempt_no: i32,
    spec: &TaskSpec,
    app: &str,
    stream: &str,
) -> bool {
    if spec.task_type == TaskType::StreamIngest
        && spec.input.kind != Some(media_domain::InputKind::GbRtp)
    {
        let publish_app = spec.stream.app.as_deref().unwrap_or("live");
        let mut task_id_buf = Uuid::encode_buffer();
        let publish_stream = spec
            .stream
            .name
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                task_id
                    .as_hyphenated()
                    .encode_lower(&mut task_id_buf)
                    .to_string()
            });
        return publish_app == app && publish_stream == stream;
    }

    let Some(url) = spec.publish.url.as_deref() else {
        return false;
    };
    let Some((publish_app, publish_stream)) = parse_publish_stream_url(url) else {
        return false;
    };
    publish_app == app && publish_stream == stream
}

fn rtp_stream_matches(task_id: Uuid, attempt_no: i32, spec: &TaskSpec, stream_id: &str) -> bool {
    spec.task_type == TaskType::StreamIngest
        && spec.input.kind == Some(media_domain::InputKind::GbRtp)
        && build_rtp_stream_id(task_id, attempt_no) == stream_id
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
    fn build_resolved_task_json_applies_request_defaults() {
        let merged = build_resolved_task_json(
            TaskType::StreamIngest,
            &json!({
                "name": "relay-camera-01",
                "common": {
                    "created_by": "alice"
                },
                "input": {
                    "kind": "rtsp",
                    "source_mode": "live",
                    "url": "rtsp://camera.example/live"
                },
                "expose": {
                    "enable_rtsp": true
                }
            }),
        )
        .expect("merged json should build");

        let spec: TaskSpec = serde_json::from_value(merged).expect("task spec should parse");
        let resolved = spec.resolved();

        assert_eq!(resolved.process.mode, None);
        assert_eq!(resolved.expose.enable_rtsp, Some(true));
        assert_eq!(resolved.expose.enable_hls, Some(false));
        assert_eq!(resolved.record.enabled, Some(false));
    }

    #[test]
    fn task_summary_transcode_mode_marks_live_rtsp_ingest_as_non_transcode() {
        let spec: TaskSpec = serde_json::from_value(json!({
            "type": "stream_ingest",
            "name": "relay-camera-01",
            "input": {
                "kind": "rtsp",
                "source_mode": "live",
                "url": "rtsp://camera.example/live"
            }
        }))
        .expect("spec should parse");

        assert_eq!(
            task_summary_transcode_mode(&spec),
            Some(TASK_TRANSCODE_NONE)
        );
    }

    #[test]
    fn task_summary_transcode_mode_defaults_file_transcode_to_adaptive() {
        let spec: TaskSpec = serde_json::from_value(json!({
            "type": "file_transcode",
            "name": "transcode-archive",
            "input": {
                "kind": "file",
                "source_mode": "vod",
                "url": "archive/demo.mp4"
            },
            "publish": {
                "kind": "file"
            }
        }))
        .expect("spec should parse");

        assert_eq!(
            task_summary_transcode_mode(&spec),
            Some(TASK_TRANSCODE_ADAPTIVE)
        );
    }

    #[test]
    fn task_summary_transcode_mode_marks_mpegts_bridge_stabilization_as_forced() {
        let spec: TaskSpec = serde_json::from_value(json!({
            "type": "stream_bridge",
            "name": "bridge-live-to-mcast",
            "input": {
                "kind": "rtsp",
                "source_mode": "live",
                "url": "rtsp://camera.example/live"
            },
            "publish": {
                "kind": "udp_mpegts_multicast",
                "group": "239.0.0.10",
                "port": 1234
            },
            "process": {
                "mode": "passthrough"
            }
        }))
        .expect("spec should parse");

        assert_eq!(
            task_summary_transcode_mode(&spec),
            Some(TASK_TRANSCODE_FORCED)
        );
    }

    #[test]
    fn task_spec_overlay_skips_empty_option_fields() {
        let spec = TaskSpec {
            task_type: TaskType::StreamIngest,
            name: "relay-camera-01".to_string(),
            priority: 50,
            common: media_domain::CommonSpec {
                created_by: Some("alice".to_string()),
                callback_url: None,
                labels: Vec::new(),
            },
            input: media_domain::InputSpec {
                kind: Some(media_domain::InputKind::Rtsp),
                source_mode: Some(media_domain::SourceMode::Live),
                url: Some("rtsp://camera.example/live".to_string()),
                ..Default::default()
            },
            stream: Default::default(),
            expose: Default::default(),
            process: Default::default(),
            publish: Default::default(),
            record: Default::default(),
            recovery: Default::default(),
            schedule: Default::default(),
            resource: Default::default(),
        };

        let overlay = task_spec_overlay(&spec);

        assert_eq!(overlay["common"]["created_by"], json!("alice"));
        assert!(overlay["publish"].is_null());
    }

    #[test]
    fn task_spec_overlay_preserves_record_duration_sec() {
        let mut spec = TaskSpec {
            task_type: TaskType::StreamIngest,
            name: "duration-check".to_string(),
            priority: 50,
            common: media_domain::CommonSpec {
                created_by: Some("alice".to_string()),
                callback_url: None,
                labels: Vec::new(),
            },
            input: media_domain::InputSpec {
                kind: Some(media_domain::InputKind::HttpMp4),
                source_mode: Some(media_domain::SourceMode::Vod),
                url: Some("http://127.0.0.1/test.mp4".to_string()),
                ..Default::default()
            },
            stream: Default::default(),
            expose: Default::default(),
            process: Default::default(),
            publish: Default::default(),
            record: Default::default(),
            recovery: Default::default(),
            schedule: Default::default(),
            resource: Default::default(),
        };
        spec.record.enabled = Some(true);
        spec.record.duration_sec = Some(300);

        let overlay = task_spec_overlay(&spec);

        assert_eq!(overlay["record"]["duration_sec"], json!(300));
    }

    #[test]
    fn task_spec_overlay_preserves_input_loop_enabled() {
        let spec = TaskSpec {
            task_type: TaskType::StreamIngest,
            name: "loop-check".to_string(),
            priority: 50,
            common: media_domain::CommonSpec {
                created_by: Some("alice".to_string()),
                callback_url: None,
                labels: Vec::new(),
            },
            input: media_domain::InputSpec {
                kind: Some(media_domain::InputKind::HttpMp4),
                source_mode: Some(media_domain::SourceMode::Vod),
                loop_enabled: Some(true),
                url: Some("http://127.0.0.1/test.mp4".to_string()),
                ..Default::default()
            },
            stream: Default::default(),
            expose: Default::default(),
            process: Default::default(),
            publish: Default::default(),
            record: Default::default(),
            recovery: Default::default(),
            schedule: Default::default(),
            resource: Default::default(),
        };

        let overlay = task_spec_overlay(&spec);

        assert_eq!(overlay["input"]["loop_enabled"], json!(true));
    }

    #[test]
    fn artifact_http_url_from_path_uses_node_stream_base() {
        let url = artifact_http_url_from_path(
            "http://192.168.1.10:8081",
            "/data/zlm/www/artifacts/transcode/2026/clip.mp4",
        )
        .expect("artifact url should build");

        assert_eq!(
            url,
            "http://192.168.1.10:8081/artifacts/transcode/2026/clip.mp4"
        );
    }

    #[test]
    fn record_http_url_from_path_uses_web_root_directly() {
        let url = record_http_url_from_path(
            "http://192.168.1.10:8081",
            "/data/zlm/www/record/live/camera01/clip.mp4",
        )
        .expect("record url should build");

        assert_eq!(
            url,
            "http://192.168.1.10:8081/record/live/camera01/clip.mp4"
        );
    }

    #[test]
    fn record_http_url_from_path_translates_legacy_record_root() {
        let url = record_http_url_from_path(
            "http://192.168.1.10:8081",
            "/data/zlm/record/live/camera01/clip.mp4",
        )
        .expect("legacy record url should translate");

        assert_eq!(
            url,
            "http://192.168.1.10:8081/record/live/camera01/clip.mp4"
        );
    }

    #[test]
    fn resolve_absolute_http_url_accepts_relative_paths() {
        let url = resolve_absolute_http_url("http://worker.example:8081", "/record/live.m3u8")
            .expect("relative hook url should resolve");
        assert_eq!(url, "http://worker.example:8081/record/live.m3u8");
    }

    #[test]
    fn is_hls_playlist_record_path_accepts_record_root_m3u8_only() {
        assert!(is_hls_playlist_record_path(
            "/data/zlm/www/record/live/camera01/index.m3u8"
        ));
        assert!(is_hls_playlist_record_path(
            "/data/zlm/record/live/camera01/index.m3u8"
        ));
        assert!(!is_hls_playlist_record_path(
            "/data/zlm/www/live/camera01/hls.m3u8"
        ));
        assert!(!is_hls_playlist_record_path(
            "/data/zlm/www/record/live/camera01/index-00001.ts"
        ));
    }

    #[test]
    fn should_persist_record_file_hook_only_keeps_hls_record_playlists() {
        let binding = HookStreamBinding {
            task_id: Uuid::now_v7(),
            attempt_id: Uuid::now_v7(),
            attempt_no: 1,
            resolved_spec: Some(json!({
                "type": "stream_ingest",
                "name": "record-hls",
                "common": {"created_by": "tester"},
                "input": {"kind": "rtsp", "url": "rtsp://camera/live"},
                "stream": {"app": "live", "name": "camera01"},
                "expose": {
                    "enable_rtsp": false,
                    "enable_rtmp": false,
                    "enable_http_ts": false,
                    "enable_http_fmp4": false,
                    "enable_hls": false
                },
                "process": {"mode": "copy_or_transcode"},
                "record": {"enabled": true, "format": "hls"},
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            })),
        };
        let playlist = ZlmRecordFileRecord {
            record_format: Some("hls".to_string()),
            schema: None,
            vhost: "__defaultVhost__".to_string(),
            app: "live".to_string(),
            stream: "camera01".to_string(),
            file_path: "/data/zlm/www/record/live/camera01/index.m3u8".to_string(),
            file_size: 1024,
            time_len_sec: Some(30),
            start_time: None,
            file_name: Some("index.m3u8".to_string()),
            folder: Some("/data/zlm/www/record/live/camera01".to_string()),
            url: Some("http://stream.example/record/live/camera01/index.m3u8".to_string()),
        };
        let segment = ZlmRecordFileRecord {
            file_path: "/data/zlm/www/record/live/camera01/index-00001.ts".to_string(),
            file_name: Some("index-00001.ts".to_string()),
            ..playlist.clone()
        };

        assert!(
            should_persist_record_file_hook("on_record_hls", &binding, &playlist)
                .expect("playlist should evaluate")
        );
        assert!(
            !should_persist_record_file_hook("on_record_ts", &binding, &segment)
                .expect("segment should evaluate")
        );

        let exposed_only_binding = HookStreamBinding {
            resolved_spec: Some(json!({
                "type": "stream_ingest",
                "name": "expose-hls",
                "common": {"created_by": "tester"},
                "input": {"kind": "rtsp", "url": "rtsp://camera/live"},
                "stream": {"app": "live", "name": "camera01"},
                "expose": {
                    "enable_rtsp": false,
                    "enable_rtmp": false,
                    "enable_http_ts": false,
                    "enable_http_fmp4": false,
                    "enable_hls": true
                },
                "process": {"mode": "copy_or_transcode"},
                "record": {"enabled": false},
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            })),
            ..binding
        };
        let exposed_playlist = ZlmRecordFileRecord {
            file_path: "/data/zlm/www/live/camera01/hls.m3u8".to_string(),
            file_name: Some("hls.m3u8".to_string()),
            folder: Some("/data/zlm/www/live/camera01".to_string()),
            url: Some("http://stream.example/live/camera01/hls.m3u8".to_string()),
            ..playlist
        };

        assert!(
            !should_persist_record_file_hook(
                "on_record_hls",
                &exposed_only_binding,
                &exposed_playlist
            )
            .expect("exposed playlist should evaluate")
        );
    }

    #[test]
    fn validate_managed_file_publish_target_rejects_file_path_override() {
        let spec: TaskSpec = serde_json::from_value(json!({
            "type": "file_transcode",
            "name": "artifact-test",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "input.mp4"},
            "process": {"mode": "copy_or_transcode"},
            "record": {},
            "publish": {
                "kind": "file",
                "url": "/tmp/output.mp4"
            },
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }))
        .expect("task spec should parse");

        let error =
            validate_managed_file_publish_target(&spec).expect_err("invalid output should reject");
        assert!(matches!(error, RepoError::Validation(_)));
    }
}
