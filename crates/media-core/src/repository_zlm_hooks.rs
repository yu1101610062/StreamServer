//! ZLM hook 仓储：处理 ZLM 发布、流状态、录制文件 hook，并维护 hook 与任务的绑定关系。

use chrono::{DateTime, Utc};
use media_domain::{EventSource, TaskSpec, TaskType};
use serde_json::{Value, json};
use sqlx::{Postgres, Row, postgres::PgRow};
use uuid::Uuid;

use super::{
    RepoError, TaskRepository, is_hls_playlist_record_path, relative_http_url_from_path,
    sticky_reconnect_from_spec_value, task_id_from_managed_output_path,
};

impl TaskRepository {
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

    #[allow(clippy::too_many_arguments)] // The arguments are the authenticated hook evidence row.
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

#[derive(Debug, Clone)]
pub(super) struct HookStreamBinding {
    pub(super) task_id: Uuid,
    pub(super) attempt_id: Uuid,
    pub(super) attempt_no: i32,
    pub(super) resolved_spec: Option<Value>,
    pub(super) started_at: Option<DateTime<Utc>>,
    pub(super) ended_at: Option<DateTime<Utc>>,
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

fn relative_record_http_url_from_hook(record: &ZlmRecordFileRecord) -> Option<String> {
    relative_http_url_from_path(&record.file_path).ok()
}

pub(super) fn should_persist_record_file_hook(
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

pub(super) fn should_persist_zlm_stream_event(event_type: &str, event_level: &str) -> bool {
    if event_level == "error" {
        return true;
    }
    event_type != "stream_lookup_miss"
}

pub(super) fn should_persist_hook_event(hook_name: &str) -> bool {
    hook_name != "on_server_keepalive"
}

pub(super) fn compact_hook_payload(hook_name: &str, payload: Value) -> Value {
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
