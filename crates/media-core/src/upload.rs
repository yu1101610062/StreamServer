use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, header},
};
use chrono::{DateTime, Utc};
use media_domain::Page;
use reqwest::{StatusCode, Url};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    AppState, PeerAddress,
    auth::ApiPermission,
    authorize_business_request,
    control_plane::NodeLiveLoad,
    error::AppError,
    repository::{
        MediaUploadAssetDeleteTarget, MediaUploadAssetListFilter, MediaUploadAssetSummary,
        NewMediaUploadAsset, NodeSummary,
    },
};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UploadMediaResponse {
    pub id: String,
    pub file_name: String,
    pub source_url: String,
    pub http_url: String,
    pub duration_sec: u64,
    pub file_size: u64,
    pub sha256: String,
    pub content_type: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub(crate) struct UploadMediaQuery {
    #[serde(default)]
    node_id: Option<Uuid>,
    #[serde(default)]
    required_labels: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub(crate) struct DeleteMediaUploadAssetQuery {
    #[serde(default)]
    delete_file: bool,
}

pub(crate) async fn upload_media(
    State(state): State<AppState>,
    Query(query): Query<UploadMediaQuery>,
    PeerAddress(peer): PeerAddress,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<UploadMediaResponse>, AppError> {
    authorize_business_request(&state, &headers, peer, ApiPermission::TaskWrite).await?;
    ensure_multipart(&headers)?;

    let required_bytes = content_length(&headers);
    let labels = parse_required_labels(query.required_labels.as_deref());
    let node = select_upload_node(&state, query.node_id, &labels, required_bytes).await?;
    let url = agent_upload_url(&node)?;
    let mut request = state
        .http_client
        .post(url)
        .body(reqwest::Body::wrap_stream(body.into_data_stream()));
    if let Some(value) = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    {
        request = request.header(header::CONTENT_TYPE.as_str(), value);
    }
    if let Some(value) = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
    {
        request = request.header(header::CONTENT_LENGTH.as_str(), value);
    }

    let response = request.send().await.map_err(|error| {
        AppError::Internal(format!("proxy upload to media-agent failed: {error}"))
    })?;
    let status = response.status();
    if status.is_success() {
        let payload = response
            .json::<UploadMediaResponse>()
            .await
            .map_err(|error| {
                AppError::Internal(format!("parse media-agent upload response failed: {error}"))
            })?;
        let asset = NewMediaUploadAsset {
            id: Uuid::parse_str(payload.id.trim()).map_err(|error| {
                AppError::Internal(format!("media-agent returned invalid upload id: {error}"))
            })?,
            node_id: node.id,
            file_name: payload.file_name.clone(),
            source_url: payload.source_url.clone(),
            http_url: payload.http_url.clone(),
            duration_sec: i64::try_from(payload.duration_sec).map_err(|_| {
                AppError::Internal("media-agent upload durationSec is too large".to_string())
            })?,
            file_size: i64::try_from(payload.file_size).map_err(|_| {
                AppError::Internal("media-agent upload fileSize is too large".to_string())
            })?,
            sha256: payload.sha256.clone(),
            content_type: payload.content_type.clone(),
            created_by: String::new(),
            created_at: upload_created_at(payload.created_at),
        };
        state.repository.insert_media_upload_asset(asset).await?;
        return Ok(Json(payload));
    }

    let message = response
        .text()
        .await
        .unwrap_or_else(|error| format!("read media-agent upload error failed: {error}"));
    if status.is_client_error() {
        Err(AppError::BadRequest(format!(
            "media-agent upload rejected: {message}"
        )))
    } else if status == StatusCode::SERVICE_UNAVAILABLE {
        Err(AppError::Internal(format!(
            "media-agent upload unavailable: {message}"
        )))
    } else {
        Err(AppError::Internal(format!(
            "media-agent upload failed with status {status}: {message}"
        )))
    }
}

pub(crate) async fn list_media_upload_assets(
    State(state): State<AppState>,
    PeerAddress(peer): PeerAddress,
    headers: HeaderMap,
    Query(filter): Query<MediaUploadAssetListFilter>,
) -> Result<Json<Page<MediaUploadAssetSummary>>, AppError> {
    authorize_business_request(&state, &headers, peer, ApiPermission::TaskWrite).await?;
    validate_asset_status(filter.status.as_deref())?;
    Ok(Json(
        state.repository.list_media_upload_assets(filter).await?,
    ))
}

pub(crate) async fn get_media_upload_asset(
    State(state): State<AppState>,
    PeerAddress(peer): PeerAddress,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<MediaUploadAssetSummary>, AppError> {
    authorize_business_request(&state, &headers, peer, ApiPermission::TaskWrite).await?;
    let asset = state
        .repository
        .get_media_upload_asset(id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("media upload asset {id} was not found")))?;
    Ok(Json(asset))
}

pub(crate) async fn delete_media_upload_asset(
    State(state): State<AppState>,
    PeerAddress(peer): PeerAddress,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(query): Query<DeleteMediaUploadAssetQuery>,
) -> Result<Json<MediaUploadAssetSummary>, AppError> {
    let principal =
        authorize_business_request(&state, &headers, peer, ApiPermission::TaskWrite).await?;
    let target = state
        .repository
        .get_media_upload_asset_delete_target(id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("media upload asset {id} was not found")))?;

    if query.delete_file && !target.file_deleted {
        delete_agent_media_file(&state, &target).await?;
    }

    let deleted_by = principal.subject();
    Ok(Json(
        state
            .repository
            .mark_media_upload_asset_deleted(id, query.delete_file, deleted_by)
            .await?,
    ))
}

fn ensure_multipart(headers: &HeaderMap) -> Result<(), AppError> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if content_type.starts_with("multipart/form-data") {
        Ok(())
    } else {
        Err(AppError::BadRequest(
            "Content-Type must be multipart/form-data".to_string(),
        ))
    }
}

fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
}

fn parse_required_labels(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn validate_asset_status(value: Option<&str>) -> Result<(), AppError> {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        Some("active" | "deleted" | "all") | None => Ok(()),
        Some(_) => Err(AppError::BadRequest(
            "status must be active, deleted or all".to_string(),
        )),
    }
}

fn upload_created_at(value: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(value).unwrap_or_else(Utc::now)
}

async fn select_upload_node(
    state: &AppState,
    requested_node_id: Option<Uuid>,
    required_labels: &[String],
    required_bytes: Option<u64>,
) -> Result<NodeSummary, AppError> {
    let live_loads = state.control_plane.current_node_loads().await;
    let mut candidates = state
        .repository
        .list_nodes()
        .await?
        .into_iter()
        .filter_map(|node| {
            if requested_node_id.is_some_and(|node_id| node.id != node_id) {
                return None;
            }
            if !node_matches_required_labels(&node, required_labels) {
                return None;
            }
            let load = live_loads.get(&node.id)?;
            (upload_node_available(&node, load) && upload_node_has_space(load, required_bytes))
                .then_some((node, load.clone()))
        })
        .collect::<Vec<_>>();

    candidates.sort_by(|(left_node, left_load), (right_node, right_load)| {
        right_load
            .upload_disk_available_bytes
            .cmp(&left_load.upload_disk_available_bytes)
            .then_with(|| {
                left_load
                    .slot_usage
                    .partial_cmp(&right_load.slot_usage)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| left_load.running_tasks.cmp(&right_load.running_tasks))
            .then_with(|| left_node.id.cmp(&right_node.id))
    });

    candidates
        .into_iter()
        .next()
        .map(|(node, _)| node)
        .ok_or_else(|| {
            if requested_node_id.is_some() || !required_labels.is_empty() {
                AppError::BadRequest(
                    "no media-agent matches upload node selection requirements".to_string(),
                )
            } else {
                AppError::Internal(
                    "no connected media-agent is available for media upload".to_string(),
                )
            }
        })
}

fn upload_node_available(node: &NodeSummary, load: &NodeLiveLoad) -> bool {
    node.healthy
        && node.control_connected
        && load.connected
        && load.ffmpeg_alive
        && !load.artifact_cleanup_blocked
        && !node.agent_http_base_url.trim().is_empty()
}

fn upload_node_has_space(load: &NodeLiveLoad, required_bytes: Option<u64>) -> bool {
    let Some(required_bytes) = required_bytes else {
        return true;
    };
    if load.upload_disk_total_bytes == 0 {
        return true;
    }
    load.upload_disk_available_bytes >= required_bytes
}

fn node_matches_required_labels(node: &NodeSummary, required_labels: &[String]) -> bool {
    required_labels
        .iter()
        .map(|label| label.trim())
        .filter(|label| !label.is_empty())
        .all(|required| node.labels.iter().any(|label| label == required))
}

fn agent_upload_url(node: &NodeSummary) -> Result<Url, AppError> {
    let mut url = Url::parse(node.agent_http_base_url.trim()).map_err(|error| {
        AppError::Internal(format!(
            "invalid agent_http_base_url for {}: {error}",
            node.id
        ))
    })?;
    let base_path = url.path().trim_end_matches('/');
    url.set_path(&format!("{base_path}/internal/uploads/media"));
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

async fn delete_agent_media_file(
    state: &AppState,
    target: &MediaUploadAssetDeleteTarget,
) -> Result<(), AppError> {
    let url = agent_delete_url(
        &target.agent_http_base_url,
        &target.source_url,
        target.node_id,
    )?;
    let response = state
        .http_client
        .delete(url)
        .send()
        .await
        .map_err(|error| {
            AppError::Internal(format!("delete media file on media-agent failed: {error}"))
        })?;
    if response.status().is_success() {
        return Ok(());
    }

    let status = response.status();
    let message = response
        .text()
        .await
        .unwrap_or_else(|error| format!("read media-agent delete error failed: {error}"));
    if status.is_client_error() {
        Err(AppError::BadRequest(format!(
            "media-agent file delete rejected: {message}"
        )))
    } else {
        Err(AppError::Internal(format!(
            "media-agent file delete failed with status {status}: {message}"
        )))
    }
}

fn agent_delete_url(base_url: &str, source_url: &str, node_id: Uuid) -> Result<Url, AppError> {
    let mut url = Url::parse(base_url.trim()).map_err(|error| {
        AppError::Internal(format!(
            "invalid agent_http_base_url for {}: {error}",
            node_id
        ))
    })?;
    let base_path = url.path().trim_end_matches('/');
    let source_url = source_url.trim().trim_start_matches('/');
    url.set_path(&format!("{base_path}/internal/uploads/media/{source_url}"));
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

pub(crate) fn uploaded_file_node_id(source_url: &str) -> Option<Uuid> {
    let trimmed = source_url.trim().trim_start_matches('/');
    let mut parts = trimmed.split('/');
    match (parts.next(), parts.next()) {
        (Some("uploads"), Some(node_id)) => Uuid::parse_str(node_id).ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        agent_delete_url, agent_upload_url, node_matches_required_labels, upload_node_available,
        upload_node_has_space, uploaded_file_node_id,
    };
    use crate::control_plane::NodeLiveLoad;
    use crate::repository::NodeSummary;
    use chrono::Utc;
    use uuid::Uuid;

    #[test]
    fn uploaded_file_node_id_parses_node_segment() {
        let node_id = Uuid::now_v7();
        let source_url = format!("/uploads/{node_id}/2026/04/29/demo.mp4");

        assert_eq!(uploaded_file_node_id(&source_url), Some(node_id));
    }

    #[test]
    fn uploaded_file_node_id_ignores_non_managed_path() {
        assert_eq!(uploaded_file_node_id("vod/demo.mp4"), None);
    }

    #[test]
    fn agent_upload_url_uses_reported_agent_http_base_url() {
        let mut node = sample_node("http://172.17.13.196:8081");
        let url = agent_upload_url(&node).expect("upload url should build");
        assert_eq!(
            url.as_str(),
            "http://172.17.13.196:8081/internal/uploads/media"
        );

        node.agent_http_base_url = "http://172.17.13.196:18081/api".to_string();
        let url = agent_upload_url(&node).expect("upload url should append path");
        assert_eq!(
            url.as_str(),
            "http://172.17.13.196:18081/api/internal/uploads/media"
        );
    }

    #[test]
    fn upload_node_available_requires_reported_agent_http_base_url() {
        let load = NodeLiveLoad {
            connected: true,
            running_tasks: 0,
            starting_tasks: 0,
            stopping_tasks: 0,
            orphaned_tasks: 0,
            slot_usage: 0.0,
            cpu_percent: 1.0,
            mem_percent: 1.0,
            disk_percent: 1.0,
            upload_disk_total_bytes: 1_000,
            upload_disk_available_bytes: 800,
            upload_disk_used_percent: 20.0,
            zlm_alive: true,
            ffmpeg_alive: true,
            gpu_runtime: Vec::new(),
            artifact_cleanup_blocked: false,
        };

        let mut node = sample_node("http://172.17.13.196:8081");
        assert!(upload_node_available(&node, &load));

        node.agent_http_base_url.clear();
        assert!(!upload_node_available(&node, &load));
    }

    #[test]
    fn upload_node_space_uses_reported_upload_disk_when_known() {
        let load = sample_load(1_000, 800);

        assert!(upload_node_has_space(&load, Some(800)));
        assert!(!upload_node_has_space(&load, Some(801)));

        let unknown = sample_load(0, 0);
        assert!(upload_node_has_space(&unknown, Some(10_000)));
    }

    #[test]
    fn required_labels_match_node_labels() {
        let mut node = sample_node("http://172.17.13.196:8081");
        node.labels = vec!["objective".to_string(), "room-a".to_string()];

        assert!(node_matches_required_labels(
            &node,
            &["objective".to_string()]
        ));
        assert!(!node_matches_required_labels(
            &node,
            &["subjective".to_string()]
        ));
    }

    #[test]
    fn agent_delete_url_uses_reported_agent_http_base_url() {
        let node_id = Uuid::now_v7();
        let url = agent_delete_url(
            "http://172.17.13.196:8081/api",
            "/uploads/demo-node/2026/04/29/demo.mp4",
            node_id,
        )
        .expect("delete url should build");

        assert_eq!(
            url.as_str(),
            "http://172.17.13.196:8081/api/internal/uploads/media/uploads/demo-node/2026/04/29/demo.mp4"
        );
    }

    fn sample_load(total: u64, available: u64) -> NodeLiveLoad {
        NodeLiveLoad {
            connected: true,
            running_tasks: 0,
            starting_tasks: 0,
            stopping_tasks: 0,
            orphaned_tasks: 0,
            slot_usage: 0.0,
            cpu_percent: 1.0,
            mem_percent: 1.0,
            disk_percent: 1.0,
            upload_disk_total_bytes: total,
            upload_disk_available_bytes: available,
            upload_disk_used_percent: if total == 0 {
                0.0
            } else {
                ((total - available) as f64 / total as f64) * 100.0
            },
            zlm_alive: true,
            ffmpeg_alive: true,
            gpu_runtime: Vec::new(),
            artifact_cleanup_blocked: false,
        }
    }

    fn sample_node(agent_http_base_url: &str) -> NodeSummary {
        let now = Utc::now();
        NodeSummary {
            id: Uuid::now_v7(),
            node_name: "node-a".to_string(),
            hostname: "worker-a".to_string(),
            labels: Vec::new(),
            zlm_api_base: "http://127.0.0.1/index/api".to_string(),
            agent_stream_addr: "http://172.17.13.196:80".to_string(),
            agent_http_base_url: agent_http_base_url.to_string(),
            zlm_rtmp_port: 1935,
            zlm_rtsp_port: 554,
            network_mode: "host".to_string(),
            interfaces: Vec::new(),
            healthy: true,
            control_connected: true,
            media_alive: true,
            last_seen_at: Some(now),
            control_last_seen_at: Some(now),
            media_last_seen_at: Some(now),
            created_at: now,
            updated_at: now,
            ffmpeg_protocols: Vec::new(),
            ffmpeg_formats: Vec::new(),
            ffmpeg_encoders: Vec::new(),
            ffmpeg_decoders: Vec::new(),
            zlm_api_list: Vec::new(),
            zlm_version: None,
            gpu: Vec::new(),
            gpu_devices: Vec::new(),
            capability_captured_at: None,
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
        }
    }
}
