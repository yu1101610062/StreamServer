//! 产物仓储：负责录像文件、转码产物和上传素材的查询、登记、删除目标解析及路径外显。

use chrono::{DateTime, Utc};
use media_domain::{Page, TaskValidationError, ValidationIssue};
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, QueryBuilder, Row, postgres::PgRow};
use uuid::Uuid;

use super::{
    OutputMountPrefixes, RepoError, TaskRepository, absolute_http_url_from_file_path,
    externalize_http_visible_path, validation_error,
};

impl TaskRepository {
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
              and rf.file_path like '%/data/zlm/www/%'
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
              and rf.file_path like '%/data/zlm/www/%'
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
              and ta.file_path like '%/data/zlm/www/%'
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
              and ta.file_path like '%/data/zlm/www/%'
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

    async fn count_record_files(&self, filter: &RecordListFilter) -> Result<u64, RepoError> {
        let mut builder = QueryBuilder::<Postgres>::new(
            "select count(*) as total from record_files rf join task_attempts ta on ta.id = rf.attempt_id join media_nodes n on n.id = ta.node_id join tasks t on t.id = rf.task_id where 1 = 1 and rf.file_path like '%/data/zlm/www/%'",
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
            "select count(*) as total from transcode_artifacts ta join tasks t on t.id = ta.task_id join media_nodes n on n.id = ta.node_id where 1 = 1 and ta.file_path like '%/data/zlm/www/%'",
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
            file_path: externalize_http_visible_path(raw_file_path, "file_path", &prefixes)?,
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
            file_path: externalize_http_visible_path(raw_file_path, "file_path", &prefixes)?,
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
