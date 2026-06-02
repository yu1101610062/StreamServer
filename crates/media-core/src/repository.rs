#[cfg(test)]
#[path = "tests/repository.rs"]
mod tests;

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
    artifact_callback_wait_timeout: chrono::Duration,
}

impl TaskRepository {
    pub fn new(pool: PgPool) -> Self {
        // 默认延迟给 Agent 留出上报终态产物的时间，避免终态回调先于文件产物到达。
        Self::with_callback_delays(
            pool,
            chrono::Duration::milliseconds(8_000),
            chrono::Duration::milliseconds(30_000),
        )
    }

    pub fn with_callback_settle_delay(
        pool: PgPool,
        callback_settle_delay: chrono::Duration,
    ) -> Self {
        Self::with_callback_delays(pool, callback_settle_delay, callback_settle_delay)
    }

    pub fn with_callback_delays(
        pool: PgPool,
        callback_settle_delay: chrono::Duration,
        artifact_callback_wait_timeout: chrono::Duration,
    ) -> Self {
        Self {
            pool,
            callback_settle_delay,
            artifact_callback_wait_timeout,
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
            where task_events.task_id = "#,
        );
        builder.push_bind(task_id);
        apply_task_event_filters(&mut builder, &filter);
        builder.push(" order by task_events.created_at desc, task_events.id desc limit ");
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
              coalesce(nullif(ta.zlm_schema, ''), sb.schema) as schema,
              sb.vhost,
              sb.app,
              sb.stream,
              sb.zlm_proxy_key,
              sb.zlm_pusher_key,
              sb.rtp_stream_id,
              t.started_at,
              t.updated_at,
              greatest(sb.created_at, t.created_at) as sort_created_at,
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
              and t.current_attempt_no > 0
              and ta.attempt_no = t.current_attempt_no
              and t.status in ('DISPATCHING', 'STARTING', 'RUNNING', 'STOPPING', 'RECOVERING', 'RECLAIMING')
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
        builder.push(
            " order by sort_created_at desc, sb.created_at desc, t.created_at desc, sb.id desc, sb.schema asc, sb.app asc, sb.stream asc",
        );
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
              t.name as task_name,
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
              rf.created_at,
              n.agent_stream_addr,
              n.output_mount_relative_prefix_mp4,
              n.output_mount_relative_prefix_hls
            from record_files rf
            join task_attempts ta on ta.id = rf.attempt_id
            join media_nodes n on n.id = ta.node_id
            join tasks t on t.id = rf.task_id
            where 1 = 1
              and rf.file_path like '%/data/zlm/www/output/%'
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
              t.name as task_name,
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
              rf.created_at,
              n.agent_stream_addr,
              n.output_mount_relative_prefix_mp4,
              n.output_mount_relative_prefix_hls
            from record_files rf
            join task_attempts ta on ta.id = rf.attempt_id
            join media_nodes n on n.id = ta.node_id
            join tasks t on t.id = rf.task_id
            where rf.task_id = $1
              and rf.file_path like '%/data/zlm/www/output/%'
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
              t.name as task_name,
              ta.attempt_id,
              ta.node_id,
              ta.file_name,
              ta.file_path,
              ta.http_url,
              ta.file_size,
              ta.created_at,
              n.agent_stream_addr,
              n.output_mount_relative_prefix_mp4,
              n.output_mount_relative_prefix_hls
            from transcode_artifacts ta
            join tasks t on t.id = ta.task_id
            join media_nodes n on n.id = ta.node_id
            where 1 = 1
              and ta.file_path like '%/data/zlm/www/output/%'
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

    pub async fn insert_media_upload_asset(
        &self,
        asset: NewMediaUploadAsset,
    ) -> Result<MediaUploadAssetSummary, RepoError> {
        sqlx::query(
            r#"
            insert into media_upload_assets (
              id, node_id, file_name, source_url, http_url, duration_sec, file_size,
              sha256, content_type, created_by, created_at
            ) values (
              $1, $2, $3, $4, $5, $6, $7,
              $8, $9, $10, $11
            )
            "#,
        )
        .bind(asset.id)
        .bind(asset.node_id)
        .bind(&asset.file_name)
        .bind(&asset.source_url)
        .bind(&asset.http_url)
        .bind(asset.duration_sec)
        .bind(asset.file_size)
        .bind(&asset.sha256)
        .bind(&asset.content_type)
        .bind(&asset.created_by)
        .bind(asset.created_at)
        .execute(&self.pool)
        .await?;

        self.get_media_upload_asset(asset.id)
            .await?
            .ok_or(RepoError::MediaUploadAssetNotFound(asset.id))
    }

    pub async fn list_media_upload_assets(
        &self,
        filter: MediaUploadAssetListFilter,
    ) -> Result<Page<MediaUploadAssetSummary>, RepoError> {
        let page = filter.page.unwrap_or(1).max(1);
        let page_size = filter.page_size.unwrap_or(20).clamp(1, 200);
        let total = self.count_media_upload_assets(&filter).await?;

        let mut builder = QueryBuilder::<Postgres>::new(
            r#"
            select
              a.id,
              a.node_id,
              n.node_name,
              a.file_name,
              a.source_url,
              a.http_url,
              a.duration_sec,
              a.file_size,
              a.sha256,
              a.content_type,
              a.status,
              a.file_deleted,
              a.created_by,
              a.created_at,
              a.deleted_by,
              a.deleted_at
            from media_upload_assets a
            join media_nodes n on n.id = a.node_id
            where 1 = 1
            "#,
        );
        apply_media_upload_asset_filters(&mut builder, &filter);
        builder.push(" order by a.created_at desc, a.id desc limit ");
        builder.push_bind(i64::from(page_size));
        builder.push(" offset ");
        builder.push_bind(i64::from((page - 1) * page_size));

        let rows = builder
            .build()
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|row| MediaUploadAssetSummary::from_row(&row))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Page::new(rows, page, page_size, total))
    }

    pub async fn get_media_upload_asset(
        &self,
        id: Uuid,
    ) -> Result<Option<MediaUploadAssetSummary>, RepoError> {
        sqlx::query(
            r#"
            select
              a.id,
              a.node_id,
              n.node_name,
              a.file_name,
              a.source_url,
              a.http_url,
              a.duration_sec,
              a.file_size,
              a.sha256,
              a.content_type,
              a.status,
              a.file_deleted,
              a.created_by,
              a.created_at,
              a.deleted_by,
              a.deleted_at
            from media_upload_assets a
            join media_nodes n on n.id = a.node_id
            where a.id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .map(|row| MediaUploadAssetSummary::from_row(&row))
        .transpose()
    }

    pub async fn get_media_upload_asset_delete_target(
        &self,
        id: Uuid,
    ) -> Result<Option<MediaUploadAssetDeleteTarget>, RepoError> {
        sqlx::query(
            r#"
            select
              a.node_id,
              a.source_url,
              a.file_deleted,
              n.agent_http_base_url
            from media_upload_assets a
            join media_nodes n on n.id = a.node_id
            where a.id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .map(|row| {
            Ok(MediaUploadAssetDeleteTarget {
                node_id: row.try_get("node_id")?,
                source_url: row.try_get("source_url")?,
                file_deleted: row.try_get("file_deleted")?,
                agent_http_base_url: row.try_get("agent_http_base_url")?,
            })
        })
        .transpose()
    }

    pub async fn mark_media_upload_asset_deleted(
        &self,
        id: Uuid,
        file_deleted: bool,
        deleted_by: &str,
    ) -> Result<MediaUploadAssetSummary, RepoError> {
        let result = sqlx::query(
            r#"
            update media_upload_assets
               set status = 'deleted',
                   file_deleted = file_deleted or $2,
                   deleted_by = nullif($3, ''),
                   deleted_at = coalesce(deleted_at, now())
             where id = $1
            "#,
        )
        .bind(id)
        .bind(file_deleted)
        .bind(deleted_by)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Err(RepoError::MediaUploadAssetNotFound(id));
        }

        self.get_media_upload_asset(id)
            .await?
            .ok_or(RepoError::MediaUploadAssetNotFound(id))
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
              t.name as task_name,
              ta.attempt_id,
              ta.node_id,
              ta.file_name,
              ta.file_path,
              ta.http_url,
              ta.file_size,
              ta.created_at,
              n.agent_stream_addr,
              n.output_mount_relative_prefix_mp4,
              n.output_mount_relative_prefix_hls
            from transcode_artifacts ta
            join tasks t on t.id = ta.task_id
            join media_nodes n on n.id = ta.node_id
            where ta.task_id = $1
              and ta.file_path like '%/data/zlm/www/output/%'
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
              n.agent_http_base_url,
              n.zlm_rtmp_port,
              n.zlm_rtsp_port,
              n.network_mode,
              n.interfaces,
              n.healthy,
              n.control_connected,
              n.last_seen_at,
              n.control_last_seen_at,
              n.media_last_seen_at,
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
              upload_disk_total_bytes,
              upload_disk_available_bytes,
              upload_disk_used_percent,
              running_tasks,
              starting_tasks,
              stopping_tasks,
              orphaned_tasks,
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
              processed_at,
              n.output_mount_relative_prefix_mp4,
              n.output_mount_relative_prefix_hls
            from hook_events
            left join media_servers ms on ms.server_id = hook_events.server_id
            left join media_nodes n on n.id = ms.node_id
            where 1 = 1
            "#,
        );
        if let Some(node_id) = filter.node_id {
            builder.push(" and ms.node_id = ");
            builder.push_bind(node_id);
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

    pub async fn mark_tasks_reclaiming_for_disconnected_node(
        &self,
        node_id: Uuid,
    ) -> Result<(), RepoError> {
        let rows = sqlx::query(
            r#"
            select id, status::text as status, current_attempt_no
             from tasks
             where assigned_node_id = $1
               and current_attempt_no > 0
               and status in ('DISPATCHING', 'STARTING', 'RUNNING', 'STOPPING', 'RECOVERING')
             order by updated_at asc
            "#,
        )
        .bind(node_id)
        .fetch_all(&self.pool)
        .await?;

        for row in rows {
            let task_id: Uuid = row.try_get("id")?;
            let attempt_no: i32 = row.try_get("current_attempt_no")?;
            let status = TaskStatus::from_str(&row.try_get::<String, _>("status")?)?;
            let now = Utc::now();
            let reclaim_deadline_at = now
                + if status == TaskStatus::Dispatching {
                    chrono::Duration::seconds(DISPATCH_RECLAIM_GRACE_SECS)
                } else {
                    chrono::Duration::seconds(RUNTIME_RECLAIM_GRACE_SECS)
                };
            let mut tx = self.pool.begin().await?;
            let updated = sqlx::query(
                r#"
                update tasks
                   set status = 'RECLAIMING'::task_status,
                       reclaim_deadline_at = $1,
                       updated_at = $2,
                       finished_at = null
                 where id = $3
                   and current_attempt_no = $4
                   and status = $5::task_status
                "#,
            )
            .bind(reclaim_deadline_at)
            .bind(now)
            .bind(task_id)
            .bind(attempt_no)
            .bind(status.as_str())
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() == 0 {
                tx.commit().await?;
                continue;
            }

            self.insert_event(
                &mut tx,
                task_id,
                None,
                Some(attempt_no),
                EventSource::Core,
                "task_reclaiming_after_node_disconnect",
                "warn",
                json!({
                    "node_id": node_id,
                    "attempt_no": attempt_no,
                    "from": status,
                    "to": TaskStatus::Reclaiming,
                    "reclaim_deadline_at": reclaim_deadline_at,
                }),
            )
            .await?;
            tx.commit().await?;
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
                'RECLAIMING'::task_status,
                'LOST'::task_status
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

    pub async fn upsert_node_registration(
        &self,
        registration: &AgentRegistration,
        seen_at: DateTime<Utc>,
    ) -> Result<(), RepoError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"
            insert into media_nodes (
              id, node_name, hostname, labels, zlm_api_base, zlm_api_secret, agent_stream_addr,
              agent_http_base_url, zlm_rtmp_port, zlm_rtsp_port,
              output_mount_relative_prefix_mp4, output_mount_relative_prefix_hls,
              network_mode, interfaces, healthy, control_connected, last_seen_at,
              control_last_seen_at, created_at, updated_at
            ) values (
              $1, $2, $3, $4, $5, $6, $7,
              $8, $9, $10, $11, $12, $13, $14, true, true, $15, $15, $16, $16
            )
            on conflict (id) do update
               set node_name = excluded.node_name,
                   hostname = excluded.hostname,
                   labels = excluded.labels,
                   zlm_api_base = excluded.zlm_api_base,
                   zlm_api_secret = excluded.zlm_api_secret,
                   agent_stream_addr = excluded.agent_stream_addr,
                   agent_http_base_url = excluded.agent_http_base_url,
                   zlm_rtmp_port = excluded.zlm_rtmp_port,
                   zlm_rtsp_port = excluded.zlm_rtsp_port,
                   output_mount_relative_prefix_mp4 =
                     excluded.output_mount_relative_prefix_mp4,
                   output_mount_relative_prefix_hls =
                     excluded.output_mount_relative_prefix_hls,
                   network_mode = excluded.network_mode,
                   interfaces = excluded.interfaces,
                   healthy = true,
                   control_connected = true,
                   last_seen_at = excluded.last_seen_at,
                   control_last_seen_at = excluded.control_last_seen_at,
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
        .bind(&registration.agent_http_base_url)
        .bind(i32::from(registration.zlm_rtmp_port))
        .bind(i32::from(registration.zlm_rtsp_port))
        .bind(&registration.output_mount_relative_prefix_mp4)
        .bind(&registration.output_mount_relative_prefix_hls)
        .bind(registration.network_mode.as_str())
        .bind(serde_json::to_value(&registration.interfaces)?)
        .bind(seen_at)
        .bind(Utc::now())
        .execute(&mut *tx)
        .await?;

        let zlm_server_id = registration.zlm_server_id.trim();
        if !zlm_server_id.is_empty() {
            sqlx::query(
                r#"
                insert into media_servers (server_id, node_id, last_seen_at, created_at, updated_at)
                values ($1, $2, $3, $4, $4)
                on conflict (server_id) do update
                   set node_id = excluded.node_id,
                       last_seen_at = excluded.last_seen_at,
                       updated_at = excluded.updated_at
                "#,
            )
            .bind(zlm_server_id)
            .bind(registration.node_id)
            .bind(seen_at)
            .bind(Utc::now())
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;

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
               set healthy = control_connected and not $4,
                   last_seen_at = $1,
                   updated_at = $2
             where id = $3
            "#,
        )
        .bind(heartbeat.node_time)
        .bind(Utc::now())
        .bind(node_id)
        .bind(heartbeat.artifact_cleanup_blocked)
        .execute(&mut *tx)
        .await?;

        if result.rows_affected() == 0 {
            return Err(RepoError::NodeNotFound(node_id));
        }

        sqlx::query(
            r#"
            insert into node_heartbeats (
              id, node_id, cpu_percent, mem_percent, disk_percent, running_tasks,
              upload_disk_total_bytes, upload_disk_available_bytes, upload_disk_used_percent,
              starting_tasks, stopping_tasks, orphaned_tasks,
              slot_usage, zlm_alive, ffmpeg_alive, gpu_runtime, node_time, received_at
            ) values (
              $1, $2, $3, $4, $5, $6,
              $7, $8, $9,
              $10, $11, $12,
              $13, $14, $15, $16, $17, $18
            )
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(node_id)
        .bind(heartbeat.cpu_percent)
        .bind(heartbeat.mem_percent)
        .bind(heartbeat.disk_percent)
        .bind(i32::try_from(heartbeat.running_tasks).unwrap_or(i32::MAX))
        .bind(i64::try_from(heartbeat.upload_disk_total_bytes).unwrap_or(i64::MAX))
        .bind(i64::try_from(heartbeat.upload_disk_available_bytes).unwrap_or(i64::MAX))
        .bind(heartbeat.upload_disk_used_percent)
        .bind(i32::try_from(heartbeat.starting_tasks).unwrap_or(i32::MAX))
        .bind(i32::try_from(heartbeat.stopping_tasks).unwrap_or(i32::MAX))
        .bind(i32::try_from(heartbeat.orphaned_tasks).unwrap_or(i32::MAX))
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
                   control_connected = $1,
                   last_seen_at = coalesce($2, last_seen_at),
                   control_last_seen_at = case when $1 then coalesce($2, control_last_seen_at) else control_last_seen_at end,
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

    pub async fn record_media_server_seen(
        &self,
        node_id: Uuid,
        server_id: &str,
        seen_at: DateTime<Utc>,
    ) -> Result<(), RepoError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"
            insert into media_servers (server_id, node_id, last_seen_at, created_at, updated_at)
            values ($1, $2, $3, $4, $4)
            on conflict (server_id) do update
               set node_id = excluded.node_id,
                   last_seen_at = excluded.last_seen_at,
                   updated_at = excluded.updated_at
            "#,
        )
        .bind(server_id.trim())
        .bind(node_id)
        .bind(seen_at)
        .bind(Utc::now())
        .execute(&mut *tx)
        .await?;

        let result = sqlx::query(
            r#"
            update media_nodes
               set healthy = control_connected,
                   last_seen_at = $1,
                   media_last_seen_at = $1,
                   updated_at = $2
             where id = $3
            "#,
        )
        .bind(seen_at)
        .bind(Utc::now())
        .bind(node_id)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 0 {
            return Err(RepoError::NodeNotFound(node_id));
        }

        tx.commit().await?;
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

    pub async fn resolve_node_id_by_server_id(
        &self,
        server_id: &str,
    ) -> Result<Option<Uuid>, RepoError> {
        let row = sqlx::query("select node_id from media_servers where server_id = $1")
            .bind(server_id.trim())
            .fetch_optional(&self.pool)
            .await?;

        row.map(|row| row.try_get("node_id"))
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
            .find_stream_binding_for_record_hook(&mut tx, server_id, &record)
            .await?
        {
            if should_persist_record_file_hook(hook_name, &binding, &record)? {
                let stored_http_url = relative_record_http_url_from_hook(&record);
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
                .bind(stored_http_url.as_deref())
                .bind(record.file_size)
                .bind(record.time_len_sec)
                .bind(record.start_time)
                .bind(Utc::now())
                .execute(&mut *tx)
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
            .find_stream_binding_for_hook(
                &mut tx,
                server_id,
                &record.vhost,
                &record.app,
                &record.stream,
            )
            .await?
        {
            if should_persist_zlm_stream_event(&record.event_type, &record.event_level) {
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
                  id, task_id, attempt_id, server_id, node_id, schema, vhost, app, stream,
                  zlm_proxy_key, zlm_pusher_key, rtp_stream_id
                )
                values ($1, $2, $3, $4, $5, $6, $7, $8, $9, null, null, $10)
                on conflict (server_id, schema, vhost, app, stream) do update
                  set task_id = excluded.task_id,
                      attempt_id = excluded.attempt_id,
                      node_id = excluded.node_id,
                      rtp_stream_id = coalesce(excluded.rtp_stream_id, stream_bindings.rtp_stream_id)
                "#,
            )
            .bind(Uuid::now_v7())
            .bind(record.task_id)
            .bind(attempt_id)
            .bind(server_id)
            .bind(node_id)
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
            let sticky_reconnect = sticky_reconnect_from_spec_value(Some(&record.resolved_spec))?;
            if !sticky_reconnect {
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
        }

        self.mark_hook_event_processed(&mut tx, dedup_key).await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn find_task_for_publish_stream(
        &self,
        server_id: &str,
        vhost: &str,
        app: &str,
        stream: &str,
    ) -> Result<Option<PublishTaskTarget>, RepoError> {
        if let Some(row) = sqlx::query(
            r#"
            select
              sb.task_id,
              ta.id as attempt_id,
              ta.attempt_no,
              t.resolved_spec
            from stream_bindings sb
            join task_attempts ta
              on ta.id = sb.attempt_id
            join tasks t
              on t.id = sb.task_id
            where sb.server_id = $1
              and sb.vhost = $2
              and sb.app = $3
              and sb.stream = $4
              and ta.attempt_no = t.current_attempt_no
            order by sb.created_at desc
            limit 1
            "#,
        )
        .bind(server_id.trim())
        .bind(vhost)
        .bind(app)
        .bind(stream)
        .fetch_optional(&self.pool)
        .await?
        {
            return Ok(Some(PublishTaskTarget {
                task_id: row.try_get("task_id")?,
                attempt_id: row.try_get("attempt_id")?,
                attempt_no: row.try_get("attempt_no")?,
                resolved_spec: row.try_get("resolved_spec")?,
            }));
        }

        let Some(node_id) = self.resolve_node_id_by_server_id(server_id).await? else {
            return Ok(None);
        };
        if self.node_has_multiple_media_servers(node_id).await? {
            return Ok(None);
        }
        self.find_task_for_publish_stream_on_node(node_id, app, stream)
            .await
    }

    async fn find_task_for_publish_stream_on_node(
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
              and t.status in ('DISPATCHING', 'STARTING', 'RUNNING', 'RECOVERING', 'RECLAIMING')
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
        server_id: &str,
        stream_id: &str,
    ) -> Result<Option<PublishTaskTarget>, RepoError> {
        if let Some(row) = sqlx::query(
            r#"
            select
              sb.task_id,
              ta.id as attempt_id,
              ta.attempt_no,
              t.resolved_spec
            from stream_bindings sb
            join task_attempts ta
              on ta.id = sb.attempt_id
            join tasks t
              on t.id = sb.task_id
            where sb.server_id = $1
              and sb.rtp_stream_id = $2
              and ta.attempt_no = t.current_attempt_no
            order by sb.created_at desc
            limit 1
            "#,
        )
        .bind(server_id.trim())
        .bind(stream_id)
        .fetch_optional(&self.pool)
        .await?
        {
            return Ok(Some(PublishTaskTarget {
                task_id: row.try_get("task_id")?,
                attempt_id: row.try_get("attempt_id")?,
                attempt_no: row.try_get("attempt_no")?,
                resolved_spec: row.try_get("resolved_spec")?,
            }));
        }

        let Some(node_id) = self.resolve_node_id_by_server_id(server_id).await? else {
            return Ok(None);
        };
        if self.node_has_multiple_media_servers(node_id).await? {
            return Ok(None);
        }
        self.find_task_for_rtp_stream_on_node(node_id, stream_id)
            .await
    }

    async fn find_task_for_rtp_stream_on_node(
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
              and t.status in ('DISPATCHING', 'STARTING', 'RUNNING', 'RECOVERING', 'RECLAIMING')
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

    async fn node_has_multiple_media_servers(&self, node_id: Uuid) -> Result<bool, RepoError> {
        let count: i64 = sqlx::query_scalar(
            r#"
            select count(*)
              from media_servers
             where node_id = $1
            "#,
        )
        .bind(node_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(count > 1)
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
            "select count(*) as total from record_files rf join task_attempts ta on ta.id = rf.attempt_id join media_nodes n on n.id = ta.node_id join tasks t on t.id = rf.task_id where 1 = 1 and rf.file_path like '%/data/zlm/www/output/%'",
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
            "select count(*) as total from transcode_artifacts ta join tasks t on t.id = ta.task_id join media_nodes n on n.id = ta.node_id where 1 = 1 and ta.file_path like '%/data/zlm/www/output/%'",
        );
        apply_file_artifact_filters(&mut builder, filter);

        let row = builder.build().fetch_one(&self.pool).await?;
        let total: i64 = row.try_get("total")?;
        Ok(total as u64)
    }

    async fn count_media_upload_assets(
        &self,
        filter: &MediaUploadAssetListFilter,
    ) -> Result<u64, RepoError> {
        let mut builder = QueryBuilder::<Postgres>::new(
            "select count(*) as total from media_upload_assets a join media_nodes n on n.id = a.node_id where 1 = 1",
        );
        apply_media_upload_asset_filters(&mut builder, filter);

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
        if !should_persist_hook_event(hook_name) {
            return Ok(true);
        }
        let payload = compact_hook_payload(hook_name, payload);
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
        server_id: &str,
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
              t.resolved_spec,
              ta.started_at,
              ta.ended_at
            from stream_bindings sb
            join task_attempts ta
              on ta.id = sb.attempt_id
            join tasks t
              on t.id = sb.task_id
            where sb.server_id = $1
              and sb.vhost = $2
              and sb.app = $3
              and sb.stream = $4
              and ta.attempt_no = t.current_attempt_no
            order by sb.created_at desc
            limit 1
            "#,
        )
        .bind(server_id)
        .bind(vhost)
        .bind(app)
        .bind(stream)
        .fetch_optional(&mut **tx)
        .await?
        .map(|row| HookStreamBinding::from_row(&row))
        .transpose()
    }

    async fn find_stream_binding_for_record_hook(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        server_id: &str,
        record: &ZlmRecordFileRecord,
    ) -> Result<Option<HookStreamBinding>, RepoError> {
        if let Some(task_id) = task_id_from_managed_output_path(&record.file_path) {
            if let Some(binding) = self
                .find_record_hook_binding_for_task(tx, server_id, task_id, record)
                .await?
            {
                return Ok(Some(binding));
            }
        }

        self.find_stream_binding_for_hook(tx, server_id, &record.vhost, &record.app, &record.stream)
            .await
    }

    async fn find_record_hook_binding_for_task(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        server_id: &str,
        task_id: Uuid,
        record: &ZlmRecordFileRecord,
    ) -> Result<Option<HookStreamBinding>, RepoError> {
        let rows = sqlx::query(
            r#"
            select
              ta.task_id,
              ta.id as attempt_id,
              ta.attempt_no,
              t.resolved_spec,
              ta.started_at,
              ta.ended_at
            from task_attempts ta
            join tasks t
              on t.id = ta.task_id
            join media_servers ms
              on ms.node_id = ta.node_id
            where ta.task_id = $1
              and ms.server_id = $2
            order by ta.attempt_no desc
            "#,
        )
        .bind(task_id)
        .bind(server_id.trim())
        .fetch_all(&mut **tx)
        .await?;

        let mut latest_matching = None;
        for row in rows {
            let binding = HookStreamBinding::from_row(&row)?;
            let Some(spec) = binding.resolved_task_spec()? else {
                continue;
            };
            if !publish_stream_matches(
                binding.task_id,
                binding.attempt_no,
                &spec,
                &record.app,
                &record.stream,
            ) {
                continue;
            }
            if latest_matching.is_none() {
                latest_matching = Some(binding.clone());
            }
            if record
                .start_time
                .is_some_and(|start_time| binding.matches_record_start(start_time))
            {
                return Ok(Some(binding));
            }
        }

        Ok(latest_matching)
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
        let task_row = sqlx::query(
            r#"
            select
              status::text as task_status,
              type::text as task_type,
              coalesce(resolved_spec, requested_spec) as task_spec
              from tasks
             where id = $1
            "#,
        )
        .bind(task_id)
        .fetch_optional(&mut **tx)
        .await?;
        let Some(task_row) = task_row else {
            return Ok(());
        };
        let task_status: String = task_row.try_get("task_status")?;
        if !matches!(
            task_status.as_str(),
            "SUCCEEDED" | "FAILED" | "CANCELED" | "LOST"
        ) {
            return Ok(());
        }
        let task_type: String = task_row.try_get("task_type")?;
        let task_spec: Option<Value> = task_row.try_get("task_spec")?;
        let now = Utc::now();

        if task_expects_artifacts_from_value(task_type.as_str(), task_spec.as_ref())
            && self
                .release_pending_terminal_callback_for_artifact(tx, task_id, attempt_no, now)
                .await?
        {
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

        self.enqueue_task_completed_callback(tx, task_id, attempt_no, "artifact_update", now)
            .await
    }

    async fn release_pending_terminal_callback_for_artifact(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        task_id: Uuid,
        attempt_no: i32,
        now: DateTime<Utc>,
    ) -> Result<bool, RepoError> {
        let result = sqlx::query(
            r#"
            update task_callback_outbox
               set deliver_after = least(deliver_after, $3),
                   updated_at = $3
             where task_id = $1
               and attempt_no = $2
               and event_type = 'task.completed'
               and reason = 'terminal_state'
               and status in ('pending', 'retrying')
            "#,
        )
        .bind(task_id)
        .bind(attempt_no)
        .bind(now)
        .execute(&mut **tx)
        .await?;
        Ok(result.rows_affected() > 0)
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

    async fn delete_stream_bindings_for_task(
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

    async fn validate_attempt_ownership(
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

    async fn reconcile_exited_snapshot(
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

    async fn consecutive_failed_attempts_before(
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

impl TaskRepository {
    async fn terminal_state_callback_deliver_after(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        task_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<DateTime<Utc>, RepoError> {
        let delay = if self.task_expects_artifacts(tx, task_id).await?
            && !self.task_already_has_artifacts(tx, task_id).await?
        {
            self.artifact_callback_wait_timeout
        } else {
            self.callback_settle_delay
        };
        Ok(now + delay)
    }

    async fn task_expects_artifacts(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        task_id: Uuid,
    ) -> Result<bool, RepoError> {
        let row = sqlx::query(
            r#"
            select
              type::text as task_type,
              coalesce(resolved_spec, requested_spec) as task_spec
              from tasks
             where id = $1
            "#,
        )
        .bind(task_id)
        .fetch_optional(&mut **tx)
        .await?;
        let Some(row) = row else {
            return Ok(false);
        };
        let task_type: String = row.try_get("task_type")?;
        let task_spec: Option<Value> = row.try_get("task_spec")?;
        Ok(task_expects_artifacts_from_value(
            task_type.as_str(),
            task_spec.as_ref(),
        ))
    }

    async fn task_already_has_artifacts(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        task_id: Uuid,
    ) -> Result<bool, RepoError> {
        sqlx::query_scalar(
            r#"
            select exists (
              select 1
                from record_files
               where task_id = $1
              union all
              select 1
                from transcode_artifacts
               where task_id = $1
            )
            "#,
        )
        .bind(task_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(Into::into)
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

fn sticky_reconnect_from_spec_value(value: Option<&Value>) -> Result<bool, RepoError> {
    Ok(value
        .cloned()
        .map(serde_json::from_value::<TaskSpec>)
        .transpose()?
        .is_some_and(|spec| spec.stream_ingest_uses_sticky_reconnect()))
}

fn sticky_reconnect_active(ownership: &AttemptOwnership) -> Result<bool, RepoError> {
    Ok(
        sticky_reconnect_from_spec_value(ownership.resolved_spec.as_ref())?
            && ownership.task_status != TaskStatus::Stopping
            && ownership.stop_requested_at.is_none(),
    )
}

fn task_expects_artifacts_from_value(task_type: &str, task_spec: Option<&Value>) -> bool {
    match task_type {
        "stream_ingest" => task_spec
            .and_then(|value| value.get("record"))
            .and_then(|value| value.get("enabled"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "stream_bridge" => task_spec
            .and_then(|value| value.get("publish"))
            .and_then(|value| value.get("kind"))
            .and_then(Value::as_str)
            .is_some_and(|value| value == "file"),
        "file_transcode" => true,
        _ => false,
    }
}

fn start_rejected_retry_limit(spec: &TaskSpec) -> Option<u32> {
    if !retry_enabled_on_disconnect(spec) {
        return Some(0);
    }
    Some(
        spec.recovery
            .max_consecutive_failures
            .unwrap_or(DEFAULT_MAX_CONSECUTIVE_FAILURES),
    )
}

const DEFAULT_MAX_CONSECUTIVE_FAILURES: u32 = 3;
const DISPATCH_RECLAIM_GRACE_SECS: i64 = 10;
const RUNTIME_RECLAIM_GRACE_SECS: i64 = 60;

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
    #[serde(skip)]
    pub sort_created_at: DateTime<Utc>,
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
            sort_created_at: row.try_get("sort_created_at")?,
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
    pub task_name: String,
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
        let prefixes = OutputMountPrefixes::from_row(row)?;
        let agent_stream_addr = row.try_get::<&str, _>("agent_stream_addr")?;
        let raw_file_path = row.try_get::<&str, _>("file_path")?;
        Ok(Self {
            id: row.try_get("id")?,
            task_id: row.try_get("task_id")?,
            task_name: row.try_get("task_name")?,
            attempt_id: row.try_get("attempt_id")?,
            vhost: row.try_get("vhost")?,
            app: row.try_get("app")?,
            stream: row.try_get("stream")?,
            file_path: externalize_managed_path(raw_file_path, "file_path", &prefixes)?,
            http_url: absolute_http_url_from_file_path(agent_stream_addr, raw_file_path),
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
    pub task_name: String,
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
        let prefixes = OutputMountPrefixes::from_row(row)?;
        let agent_stream_addr = row.try_get::<&str, _>("agent_stream_addr")?;
        let raw_file_path = row.try_get::<&str, _>("file_path")?;
        Ok(Self {
            id: row.try_get("id")?,
            artifact_kind: FileArtifactKind::from_task_type(row.try_get::<&str, _>("task_type")?)?,
            task_id: row.try_get("task_id")?,
            task_name: row.try_get("task_name")?,
            attempt_id: row.try_get("attempt_id")?,
            node_id: row.try_get("node_id")?,
            file_name: row.try_get("file_name")?,
            file_path: externalize_managed_path(raw_file_path, "file_path", &prefixes)?,
            http_url: absolute_http_url_from_file_path(agent_stream_addr, raw_file_path)
                .ok_or_else(|| {
                    validation_error(
                        "http_url",
                        format!("failed to build artifact URL from {raw_file_path}"),
                    )
                })?,
            file_size: row.try_get("file_size")?,
            created_at: row.try_get("created_at")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct NewMediaUploadAsset {
    pub id: Uuid,
    pub node_id: Uuid,
    pub file_name: String,
    pub source_url: String,
    pub http_url: String,
    pub duration_sec: i64,
    pub file_size: i64,
    pub sha256: String,
    pub content_type: String,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MediaUploadAssetListFilter {
    #[serde(default)]
    pub node_id: Option<Uuid>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub keyword: Option<String>,
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
pub struct MediaUploadAssetSummary {
    pub id: Uuid,
    pub node_id: Uuid,
    pub node_name: String,
    pub file_name: String,
    pub source_url: String,
    pub http_url: String,
    pub duration_sec: i64,
    pub file_size: i64,
    pub sha256: String,
    pub content_type: String,
    pub status: String,
    pub file_deleted: bool,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub deleted_by: Option<String>,
    pub deleted_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct MediaUploadAssetDeleteTarget {
    pub node_id: Uuid,
    pub source_url: String,
    pub file_deleted: bool,
    pub agent_http_base_url: String,
}

impl MediaUploadAssetSummary {
    fn from_row(row: &PgRow) -> Result<Self, RepoError> {
        Ok(Self {
            id: row.try_get("id")?,
            node_id: row.try_get("node_id")?,
            node_name: row.try_get("node_name")?,
            file_name: row.try_get("file_name")?,
            source_url: row.try_get("source_url")?,
            http_url: row.try_get("http_url")?,
            duration_sec: row.try_get("duration_sec")?,
            file_size: row.try_get("file_size")?,
            sha256: row.try_get("sha256")?,
            content_type: row.try_get("content_type")?,
            status: row.try_get("status")?,
            file_deleted: row.try_get("file_deleted")?,
            created_by: row.try_get("created_by")?,
            created_at: row.try_get("created_at")?,
            deleted_by: row.try_get("deleted_by")?,
            deleted_at: row.try_get("deleted_at")?,
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
    pub agent_http_base_url: String,
    pub zlm_rtmp_port: u16,
    pub zlm_rtsp_port: u16,
    pub network_mode: String,
    pub interfaces: Vec<String>,
    pub healthy: bool,
    pub control_connected: bool,
    pub media_alive: bool,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub control_last_seen_at: Option<DateTime<Utc>>,
    pub media_last_seen_at: Option<DateTime<Utc>>,
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
    pub starting_tasks: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stopping_tasks: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orphaned_tasks: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connected: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_disk_total_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_disk_available_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_disk_used_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zlm_alive: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ffmpeg_alive: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu_runtime: Option<Vec<GpuRuntimeStats>>,
}

impl NodeSummary {
    fn from_row(row: &PgRow) -> Result<Self, RepoError> {
        let media_last_seen_at: Option<DateTime<Utc>> = row.try_get("media_last_seen_at")?;
        let zlm_rtmp_port = u16::try_from(row.try_get::<i32, _>("zlm_rtmp_port")?)
            .map_err(|_| validation_error("zlm_rtmp_port", "stored value is out of range"))?;
        let zlm_rtsp_port = u16::try_from(row.try_get::<i32, _>("zlm_rtsp_port")?)
            .map_err(|_| validation_error("zlm_rtsp_port", "stored value is out of range"))?;
        let media_alive = media_last_seen_at
            .map(|seen_at| seen_at >= Utc::now() - chrono::Duration::seconds(30))
            .unwrap_or(false);
        Ok(Self {
            id: row.try_get("id")?,
            node_name: row.try_get("node_name")?,
            hostname: row.try_get("hostname")?,
            labels: serde_json::from_value(row.try_get("labels")?)?,
            zlm_api_base: row.try_get("zlm_api_base")?,
            agent_stream_addr: row.try_get("agent_stream_addr")?,
            agent_http_base_url: row.try_get("agent_http_base_url")?,
            zlm_rtmp_port,
            zlm_rtsp_port,
            network_mode: row.try_get("network_mode")?,
            interfaces: serde_json::from_value(row.try_get("interfaces")?)?,
            healthy: row.try_get("healthy")?,
            control_connected: row.try_get("control_connected")?,
            media_alive,
            last_seen_at: row.try_get("last_seen_at")?,
            control_last_seen_at: row.try_get("control_last_seen_at")?,
            media_last_seen_at,
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
            starting_tasks: None,
            stopping_tasks: None,
            orphaned_tasks: None,
            connected: None,
            cpu_percent: None,
            mem_percent: None,
            disk_percent: None,
            upload_disk_total_bytes: None,
            upload_disk_available_bytes: None,
            upload_disk_used_percent: None,
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
    pub upload_disk_total_bytes: u64,
    pub upload_disk_available_bytes: u64,
    pub upload_disk_used_percent: f64,
    pub running_tasks: u32,
    pub starting_tasks: u32,
    pub stopping_tasks: u32,
    pub orphaned_tasks: u32,
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
            upload_disk_total_bytes: u64::try_from(
                row.try_get::<i64, _>("upload_disk_total_bytes")?,
            )
            .unwrap_or_default(),
            upload_disk_available_bytes: u64::try_from(
                row.try_get::<i64, _>("upload_disk_available_bytes")?,
            )
            .unwrap_or_default(),
            upload_disk_used_percent: row.try_get("upload_disk_used_percent")?,
            running_tasks: u32::try_from(running_tasks).unwrap_or_default(),
            starting_tasks: u32::try_from(row.try_get::<i32, _>("starting_tasks")?)
                .unwrap_or_default(),
            stopping_tasks: u32::try_from(row.try_get::<i32, _>("stopping_tasks")?)
                .unwrap_or_default(),
            orphaned_tasks: u32::try_from(row.try_get::<i32, _>("orphaned_tasks")?)
                .unwrap_or_default(),
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
        let prefixes = OutputMountPrefixes::from_optional_row(row)?;
        Ok(Self {
            id: row.try_get("id")?,
            server_id: row.try_get("server_id")?,
            hook_name: row.try_get("hook_name")?,
            dedup_key: row.try_get("dedup_key")?,
            payload: externalize_path_fields_in_payload(
                row.try_get("payload")?,
                prefixes.as_ref(),
            )?,
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

fn apply_media_upload_asset_filters<'a>(
    builder: &mut QueryBuilder<'a, Postgres>,
    filter: &'a MediaUploadAssetListFilter,
) {
    if let Some(node_id) = filter.node_id {
        builder.push(" and a.node_id = ");
        builder.push_bind(node_id);
    }
    let status = filter.status.as_deref().map(str::trim).unwrap_or("active");
    if !status.is_empty() && status != "all" {
        builder.push(" and a.status = ");
        builder.push_bind(status);
    }
    if let Some(keyword) = filter
        .keyword
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        builder.push(" and (a.file_name ilike ");
        builder.push_bind(format!("%{keyword}%"));
        builder.push(" or a.source_url ilike ");
        builder.push_bind(format!("%{keyword}%"));
        builder.push(" or a.sha256 ilike ");
        builder.push_bind(format!("%{keyword}%"));
        builder.push(")");
    }
    if let Some(date_from) = filter.date_from {
        builder.push(" and a.created_at >= ");
        builder.push_bind(date_from);
    }
    if let Some(date_to) = filter.date_to {
        builder.push(" and a.created_at <= ");
        builder.push_bind(date_to);
    }
}

const ZLM_HTTP_ROOT_SEGMENT: &str = "/data/zlm/www";
const ZLM_OUTPUT_HTTP_ROOT_SEGMENT: &str = "/data/zlm/www/output";
const ZLM_OUTPUT_MP4_RELATIVE_ROOT: &str = "output/mp4";
const ZLM_OUTPUT_HLS_RELATIVE_ROOT: &str = "output/hls";

// 输出路径既可能来自安装目录，也可能来自容器时代遗留路径；统一转成相对 HTTP 根路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagedOutputBucket {
    Mp4,
    Hls,
}

#[derive(Debug, Clone)]
struct OutputMountPrefixes {
    mp4: String,
    hls: String,
}

impl OutputMountPrefixes {
    fn from_row(row: &PgRow) -> Result<Self, RepoError> {
        Ok(Self {
            mp4: row.try_get("output_mount_relative_prefix_mp4")?,
            hls: row.try_get("output_mount_relative_prefix_hls")?,
        })
    }

    fn from_optional_row(row: &PgRow) -> Result<Option<Self>, RepoError> {
        let mp4: Option<String> = row.try_get("output_mount_relative_prefix_mp4")?;
        let hls: Option<String> = row.try_get("output_mount_relative_prefix_hls")?;
        match (mp4, hls) {
            (Some(mp4), Some(hls)) => Ok(Some(Self { mp4, hls })),
            (None, None) => Ok(None),
            _ => Err(validation_error(
                "file_path",
                "node output mount prefixes are incomplete",
            )),
        }
    }

    fn relative_prefix_for_bucket(&self, bucket: ManagedOutputBucket) -> &str {
        match bucket {
            ManagedOutputBucket::Mp4 => self.mp4.as_str(),
            ManagedOutputBucket::Hls => self.hls.as_str(),
        }
    }
}

fn relative_path_under_root<'a>(path: &'a str, root: &str) -> Option<&'a str> {
    if path == root {
        return None;
    }
    path.strip_prefix(root)?.strip_prefix('/')
}

fn zlm_http_root_in_path(path: &str) -> Option<&str> {
    // 兼容网络挂载和原 Docker 路径，只要路径中包含 /data/zlm/www 就可外部化。
    for (index, _) in path.match_indices(ZLM_HTTP_ROOT_SEGMENT) {
        let end = index + ZLM_HTTP_ROOT_SEGMENT.len();
        let suffix = &path[end..];
        if suffix.is_empty() || suffix.starts_with('/') {
            return Some(&path[..end]);
        }
    }
    None
}

fn relative_path_under_zlm_http_root(path: &str) -> Option<&str> {
    let root = zlm_http_root_in_path(path)?;
    relative_path_under_root(path, root)
}

fn relative_path_under_output_root<'a>(
    path: &'a str,
    bucket: ManagedOutputBucket,
) -> Option<&'a str> {
    let relative = relative_path_under_zlm_http_root(path)?;
    let root = match bucket {
        ManagedOutputBucket::Mp4 => ZLM_OUTPUT_MP4_RELATIVE_ROOT,
        ManagedOutputBucket::Hls => ZLM_OUTPUT_HLS_RELATIVE_ROOT,
    };
    if relative == root {
        return Some("");
    }
    relative_path_under_root(relative, root)
}

fn task_id_from_managed_output_path(path: &str) -> Option<Uuid> {
    // 托管输出目录约定为 output/{mp4,hls}/node-*/{task_id}/...，Hook 可据此反查任务。
    let normalized = normalized_absolute_path(path).ok()?;
    let relative = relative_path_under_output_root(&normalized, ManagedOutputBucket::Mp4)
        .or_else(|| relative_path_under_output_root(&normalized, ManagedOutputBucket::Hls))?;
    let mut segments = relative.split('/').filter(|segment| !segment.is_empty());
    let _node_dir = segments.next()?;
    Uuid::parse_str(segments.next()?).ok()
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

    // 文件输出路径由平台分配，禁止客户端指定绝对路径绕过 allowlist 和清理策略。
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

fn relative_http_url_from_path(file_path: &str) -> Result<String, RepoError> {
    let normalized = normalized_absolute_path(file_path)?;
    let relative = relative_path_under_zlm_http_root(&normalized).ok_or_else(|| {
        validation_error("publish.url", "output path must be under */data/zlm/www")
    })?;
    Ok(format!("/{}", relative.trim_start_matches('/')))
}

fn managed_output_bucket_from_path(path: &str) -> Option<ManagedOutputBucket> {
    if relative_path_under_output_root(path, ManagedOutputBucket::Mp4).is_some() {
        return Some(ManagedOutputBucket::Mp4);
    }
    if relative_path_under_output_root(path, ManagedOutputBucket::Hls).is_some() {
        return Some(ManagedOutputBucket::Hls);
    }
    None
}

fn visible_root_for_bucket(
    path: &str,
    bucket: ManagedOutputBucket,
    prefixes: &OutputMountPrefixes,
) -> Option<String> {
    let zlm_http_root = zlm_http_root_in_path(path)?;
    let relative_prefix = prefixes.relative_prefix_for_bucket(bucket);
    Some(if relative_prefix.is_empty() {
        zlm_http_root.to_string()
    } else {
        format!("{zlm_http_root}/{relative_prefix}")
    })
}

fn external_relative_path_from_normalized(
    path: &str,
    prefixes: &OutputMountPrefixes,
) -> Option<String> {
    let bucket = managed_output_bucket_from_path(path)?;
    let visible_root = visible_root_for_bucket(path, bucket, prefixes)?;
    if path == visible_root {
        return Some("/".to_string());
    }
    relative_path_under_root(path, &visible_root)
        .map(|relative| format!("/{}", relative.trim_start_matches('/')))
}

fn externalize_managed_path(
    path: &str,
    field: &'static str,
    prefixes: &OutputMountPrefixes,
) -> Result<String, RepoError> {
    let normalized = normalized_absolute_path(path)?;
    if let Some(relative) = external_relative_path_from_normalized(&normalized, prefixes) {
        return Ok(relative);
    }

    tracing::warn!(
        field,
        path = %normalized,
        "managed path is outside outward-facing storage roots"
    );
    Err(validation_error(
        field,
        format!("must be under *{ZLM_OUTPUT_HTTP_ROOT_SEGMENT}"),
    ))
}

fn externalize_path_fields_in_payload(
    value: Value,
    prefixes: Option<&OutputMountPrefixes>,
) -> Result<Value, RepoError> {
    match value {
        Value::Array(items) => items
            .into_iter()
            .map(|item| externalize_path_fields_in_payload(item, prefixes))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Value::Object(entries) => {
            let mut normalized = serde_json::Map::with_capacity(entries.len());
            for (key, value) in entries {
                let rewritten = match key.as_str() {
                    "file_path" => externalize_path_field_value(value, "file_path", prefixes)?,
                    "folder" => externalize_path_field_value(value, "folder", prefixes)?,
                    _ => externalize_path_fields_in_payload(value, prefixes)?,
                };
                normalized.insert(key, rewritten);
            }
            Ok(Value::Object(normalized))
        }
        other => Ok(other),
    }
}

fn externalize_path_field_value(
    value: Value,
    field: &'static str,
    prefixes: Option<&OutputMountPrefixes>,
) -> Result<Value, RepoError> {
    match value {
        Value::Null => Ok(Value::Null),
        Value::String(path) if path.trim().is_empty() => Ok(Value::String(path)),
        Value::String(path) => {
            if let Some(prefixes) = prefixes {
                externalize_managed_path(&path, field, prefixes).map(Value::String)
            } else {
                Ok(Value::String(path))
            }
        }
        Value::Array(items) => items
            .into_iter()
            .map(|item| externalize_path_field_value(item, field, prefixes))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Value::Object(entries) => {
            externalize_path_fields_in_payload(Value::Object(entries), prefixes)
        }
        other => Ok(other),
    }
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

fn absolute_http_url_from_file_path(agent_stream_addr: &str, file_path: &str) -> Option<String> {
    let relative = relative_http_url_from_path(file_path).ok()?;
    absolute_http_url_from_relative(agent_stream_addr, &relative)
}

fn relative_record_http_url_from_hook(record: &ZlmRecordFileRecord) -> Option<String> {
    relative_http_url_from_path(&record.file_path).ok()
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
    let in_record_root =
        relative_path_under_output_root(&normalized, ManagedOutputBucket::Hls).is_some();
    in_record_root
        && Path::new(&normalized)
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("m3u8"))
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

#[allow(dead_code)]
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
    pub resolved_spec: Value,
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
    started_at: Option<DateTime<Utc>>,
    ended_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
struct AttemptOwnership {
    attempt_id: Option<Uuid>,
    task_status: TaskStatus,
    resolved_spec: Option<Value>,
    stop_requested_at: Option<DateTime<Utc>>,
    desired_terminal_status: Option<TaskStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OwnershipMode {
    CurrentOwner,
    AuthorizedAttempt,
}

impl HookStreamBinding {
    fn from_row(row: &PgRow) -> Result<Self, RepoError> {
        Ok(Self {
            task_id: row.try_get("task_id")?,
            attempt_id: row.try_get("attempt_id")?,
            attempt_no: row.try_get("attempt_no")?,
            resolved_spec: row.try_get("resolved_spec")?,
            started_at: row.try_get("started_at")?,
            ended_at: row.try_get("ended_at")?,
        })
    }

    fn resolved_task_spec(&self) -> Result<Option<TaskSpec>, RepoError> {
        self.resolved_spec
            .clone()
            .map(serde_json::from_value::<TaskSpec>)
            .transpose()
            .map_err(RepoError::from)
    }

    fn matches_record_start(&self, start_time: DateTime<Utc>) -> bool {
        self.started_at.is_some_and(|started_at| {
            started_at <= start_time && self.ended_at.unwrap_or(start_time) >= start_time
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
    pub records: Vec<RecordFileSummary>,
    pub file_artifacts: Vec<FileArtifactSummary>,
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
        let prefixes = OutputMountPrefixes::from_optional_row(row)?;

        Ok(Self {
            id: row.try_get("id")?,
            attempt_no: row.try_get("attempt_no")?,
            source,
            event_type: row.try_get("event_type")?,
            event_level: row.try_get("event_level")?,
            payload: externalize_path_fields_in_payload(
                row.try_get("payload")?,
                prefixes.as_ref(),
            )?,
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
    #[error("media upload asset {0} was not found")]
    MediaUploadAssetNotFound(Uuid),
    #[error("task {0} is missing resolved_spec")]
    TaskMissingResolvedSpec(Uuid),
    #[error("task is not dispatchable from status {0}")]
    TaskNotDispatchable(TaskStatus),
    #[error("task cannot be deleted from status {0}")]
    TaskDeleteForbidden(TaskStatus),
    #[error("recording control is not supported: {0}")]
    RecordingControlUnsupported(String),
    #[error("idempotency key already exists with different request body")]
    IdempotencyConflict,
    #[error("operation with the same idempotency key is still in progress")]
    OperationInProgress,
    #[error("task {task_id} attempt {attempt_no} violates repository invariants: {detail}")]
    TaskAttemptInvariant {
        task_id: Uuid,
        attempt_no: i32,
        detail: String,
    },
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

fn should_persist_agent_task_event(event_type: &str, event_level: &str) -> bool {
    if event_level == "error" {
        return true;
    }
    !matches!(
        event_type,
        "source_reconnecting" | "stream_cleanup" | "stream_lookup_miss"
    )
}

fn should_persist_zlm_stream_event(event_type: &str, event_level: &str) -> bool {
    if event_level == "error" {
        return true;
    }
    event_type != "stream_lookup_miss"
}

fn should_persist_hook_event(hook_name: &str) -> bool {
    hook_name != "on_server_keepalive"
}

fn compact_hook_payload(hook_name: &str, payload: Value) -> Value {
    if hook_name == "on_server_keepalive" {
        return json!({ "compacted": true });
    }
    if !matches!(
        hook_name,
        "on_publish"
            | "on_stream_not_found"
            | "on_stream_none_reader"
            | "on_record_ts"
            | "on_record_mp4"
    ) {
        return payload;
    }

    let mut compacted = serde_json::Map::new();
    compacted.insert("compacted".to_string(), Value::Bool(true));
    for key in [
        "mediaServerId",
        "schema",
        "protocol",
        "vhost",
        "app",
        "stream",
        "ip",
        "port",
        "file_path",
        "file_name",
        "folder",
        "url",
        "file_size",
        "time_len",
        "start_time",
    ] {
        if let Some(value) = payload.get(key) {
            compacted.insert(key.to_string(), value.clone());
        }
    }
    Value::Object(compacted)
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
