//! 回调 outbox 仓储：负责回调任务的入队、去重、延迟释放、重试、成功和死信状态更新。

use chrono::{DateTime, Utc};
use media_domain::EventSource;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{
    Postgres, Row,
    postgres::{PgQueryResult, PgRow},
};
use uuid::Uuid;

use super::{RepoError, TaskRepository};

impl TaskRepository {
    pub async fn list_due_callback_jobs(
        &self,
        now: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<CallbackOutboxJob>, RepoError> {
        let limit = limit.clamp(1, 100);
        // outbox worker 只领取到期任务，按 deliver_after/created_at 保持近似 FIFO；
        // limit 上限防止一次调度占用数据库连接太久。
        sqlx::query(
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
        .collect::<Result<Vec<_>, _>>()
    }

    pub async fn mark_callback_delivered(
        &self,
        job: &CallbackOutboxJob,
        http_status: i32,
        response_body: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<(), RepoError> {
        let mut tx = self.pool.begin().await?;
        // 成功投递只更新 outbox，不再写 task event；任务完成事件已经在入队时记录，
        // 这里保留 HTTP 响应用于排障即可。
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
        // 重试调度和事件写入在同一事务中完成，保证 UI 能同时看到 retrying 状态
        // 和下一次投递时间。
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
        // 达到最大重试次数或不可恢复错误时进入 dead letter，并把最后一次失败
        // 作为 Core 事件写回任务时间线。
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

    pub(super) async fn terminal_state_callback_deliver_after(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        task_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<DateTime<Utc>, RepoError> {
        // 终态回调需要等产物 hook 有机会先入库；没有产物预期的任务只等待
        // 较短 settle delay，减少回调延迟。
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

    #[allow(clippy::too_many_arguments)] // R3 claim records will replace this legacy enqueue shape.
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
        // 数据库唯一约束负责最终去重；调用方仍先查一遍，减少无效 insert 和事件噪音。
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

    pub(super) async fn enqueue_task_completed_callback(
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

    pub(super) async fn enqueue_task_status_callback_if_needed(
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

    pub(super) async fn enqueue_artifact_update_callback_if_needed(
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

        let terminal_callback_active: bool = sqlx::query_scalar(
            r#"
            select exists (
              select 1
                from task_callback_outbox
               where task_id = $1
                 and attempt_no = $2
                 and event_type = 'task.completed'
                 and reason = 'terminal_state'
                 and status in ('pending', 'retrying', 'delivered')
            )
            "#,
        )
        .bind(task_id)
        .bind(attempt_no)
        .fetch_one(&mut **tx)
        .await?;
        if !terminal_callback_active {
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
    pub(super) fn from_row(row: &PgRow) -> Result<Self, RepoError> {
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
