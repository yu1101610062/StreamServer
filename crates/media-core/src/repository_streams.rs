//! 流状态仓储：查询 ZLM 流、hook 事件和外显后的流事件载荷。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Postgres, QueryBuilder, Row, postgres::PgRow};
use uuid::Uuid;

use super::{OutputMountPrefixes, RepoError, TaskRepository, externalize_path_fields_in_payload};

impl TaskRepository {
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
              coalesce(t.assigned_node_id, sb.node_id, ta.node_id) as node_id,
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

    pub async fn list_task_streams(&self, task_id: Uuid) -> Result<Vec<StreamSummary>, RepoError> {
        sqlx::query(
            r#"
            select
              sb.id,
              sb.task_id,
              sb.attempt_id,
              ta.attempt_no,
              t.name as task_name,
              coalesce(t.assigned_node_id, sb.node_id, ta.node_id) as node_id,
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
            where sb.task_id = $1
              and t.current_attempt_no > 0
              and ta.attempt_no = t.current_attempt_no
            order by sort_created_at desc, sb.created_at desc, t.created_at desc, sb.id desc, sb.schema asc, sb.app asc, sb.stream asc
            "#,
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| StreamSummary::from_row(&row))
        .collect()
    }

    pub async fn list_hook_events(
        &self,
        filter: HookEventListFilter,
    ) -> Result<Vec<HookEventSummary>, RepoError> {
        let mut builder = QueryBuilder::<Postgres>::new(
            r#"
            select
              hook_events.id,
              hook_events.server_id,
              hook_events.hook_name,
              hook_events.dedup_key,
              hook_events.payload,
              hook_events.received_at,
              hook_events.processed_at,
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

        builder
            .build()
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|row| HookEventSummary::from_row(&row))
            .collect::<Result<Vec<_>, _>>()
    }
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
