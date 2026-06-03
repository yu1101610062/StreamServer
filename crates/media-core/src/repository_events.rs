//! 任务事件仓储：负责写入任务事件，并提供事件分页、日志游标和历史清理查询。

use std::str::FromStr;

use chrono::{DateTime, Utc};
use media_domain::{EventSource, Page};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Postgres, QueryBuilder, Row, postgres::PgRow};
use uuid::Uuid;

use super::{
    OutputMountPrefixes, RepoError, TaskRepository, externalize_path_fields_in_payload,
    validation_error,
};

impl TaskRepository {
    pub(super) async fn insert_event(
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
    pub(super) fn from_row(row: &PgRow) -> Result<Self, RepoError> {
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
