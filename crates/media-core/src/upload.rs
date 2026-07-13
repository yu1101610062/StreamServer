use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
};
use chrono::{DateTime, Utc};
use media_domain::Page;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    AppState, PeerAddress,
    agent_management::{AgentDeleteRequest, AgentManagementError, AgentUploadRequest},
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

    let required_bytes = required_content_length(&headers)?;
    let labels = parse_required_labels(query.required_labels.as_deref());
    let node = select_upload_node(&state, query.node_id, &labels, Some(required_bytes)).await?;
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| AppError::BadRequest("Content-Type must be valid ASCII".to_string()))?;
    let request = AgentUploadRequest::new(
        node.id,
        required_bytes,
        content_type,
        reqwest::Body::wrap_stream(body.into_data_stream()),
    )
    .map_err(|error| map_agent_management_error("prepare media-agent upload", error))?;
    let response = state
        .agent_management
        .as_ref()
        .ok_or_else(|| {
            AppError::Internal("authenticated Agent management is not configured".to_string())
        })?
        .upload(request)
        .await
        .map_err(|error| map_agent_management_error("proxy upload to media-agent", error))?;
    let status = response.status();
    if status.is_success() {
        let payload =
            serde_json::from_slice::<UploadMediaResponse>(response.body()).map_err(|_| {
                AppError::Internal("parse media-agent upload response failed".to_string())
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

    let message = String::from_utf8_lossy(response.body());
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

fn required_content_length(headers: &HeaderMap) -> Result<u64, AppError> {
    let value = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            AppError::BadRequest("a positive Content-Length header is required".to_string())
        })?;
    let value = value.parse::<u64>().map_err(|_| {
        AppError::BadRequest("Content-Length must be a positive integer".to_string())
    })?;
    if value == 0 {
        return Err(AppError::BadRequest(
            "Content-Length must be a positive integer".to_string(),
        ));
    }
    Ok(value)
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
                node_max_slot_usage(left_load)
                    .partial_cmp(&node_max_slot_usage(right_load))
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

fn node_max_slot_usage(load: &NodeLiveLoad) -> f64 {
    load.runtime_slot_loads
        .iter()
        .map(|slot_load| slot_load.slot_usage.clamp(0.0, 1.0))
        .fold(0.0, f64::max)
}

fn node_matches_required_labels(node: &NodeSummary, required_labels: &[String]) -> bool {
    required_labels
        .iter()
        .map(|label| label.trim())
        .filter(|label| !label.is_empty())
        .all(|required| node.labels.iter().any(|label| label == required))
}

async fn delete_agent_media_file(
    state: &AppState,
    target: &MediaUploadAssetDeleteTarget,
) -> Result<(), AppError> {
    let request = agent_delete_request(target)?;
    let response = state
        .agent_management
        .as_ref()
        .ok_or_else(|| {
            AppError::Internal("authenticated Agent management is not configured".to_string())
        })?
        .delete(request)
        .await
        .map_err(|error| map_agent_management_error("delete media file on media-agent", error))?;
    if response.status().is_success() {
        return Ok(());
    }

    let status = response.status();
    let message = String::from_utf8_lossy(response.body());
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

fn agent_delete_request(
    target: &MediaUploadAssetDeleteTarget,
) -> Result<AgentDeleteRequest, AppError> {
    let max_bytes = u64::try_from(target.file_size)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            AppError::Internal("media upload asset has an invalid file size".to_string())
        })?;
    let source_url = target.source_url.as_str();
    if source_url != source_url.trim() {
        return Err(AppError::Internal(
            "media upload asset has a non-canonical source path".to_string(),
        ));
    }
    let path = match source_url.strip_prefix('/') {
        Some(path) if !path.starts_with('/') => path,
        Some(_) => {
            return Err(AppError::Internal(
                "media upload asset has a non-canonical source path".to_string(),
            ));
        }
        None => source_url,
    };
    AgentDeleteRequest::new(target.node_id, path, max_bytes)
        .map_err(|error| map_agent_management_error("prepare media-agent delete", error))
}

fn map_agent_management_error(context: &str, error: AgentManagementError) -> AppError {
    let code = error.safe_code();
    if matches!(
        error,
        AgentManagementError::InvalidRequest | AgentManagementError::InvalidCapabilityRequest
    ) {
        AppError::BadRequest(format!("{context} rejected ({code})"))
    } else {
        AppError::Internal(format!("{context} failed ({code})"))
    }
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
        agent_delete_request, node_matches_required_labels, required_content_length,
        upload_node_available, upload_node_has_space, uploaded_file_node_id,
    };
    use crate::control_plane::NodeLiveLoad;
    use crate::repository::{MediaUploadAssetDeleteTarget, NodeSummary};
    use axum::http::{HeaderMap, HeaderValue, header};
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
    fn upload_node_available_ignores_untrusted_reported_http_base_url() {
        let load = NodeLiveLoad {
            connected: true,
            running_tasks: 0,
            starting_tasks: 0,
            stopping_tasks: 0,
            orphaned_tasks: 0,
            runtime_slot_loads: Vec::new(),
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
        assert!(upload_node_available(&node, &load));
        node.agent_http_base_url = "http://attacker.invalid:65535".to_string();
        assert!(upload_node_available(&node, &load));
    }

    #[test]
    fn upload_requires_a_positive_content_length() {
        let mut headers = HeaderMap::new();
        assert!(required_content_length(&headers).is_err());

        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
        assert!(required_content_length(&headers).is_err());

        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("4096"));
        assert_eq!(required_content_length(&headers).unwrap(), 4096);

        headers.insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_static("not-a-number"),
        );
        assert!(required_content_length(&headers).is_err());
    }

    #[test]
    fn delete_capability_scope_requires_one_canonical_managed_asset_path() {
        let node_id = Uuid::now_v7();
        let target = |source_url: String| MediaUploadAssetDeleteTarget {
            node_id,
            source_url,
            file_size: 4096,
            file_deleted: false,
        };
        assert!(
            agent_delete_request(&target(format!("/uploads/{node_id}/2026/07/12/clip.mp4")))
                .is_ok()
        );
        assert!(
            agent_delete_request(&target(format!("uploads/{node_id}/2026/07/12/clip.mp4"))).is_ok()
        );
        for invalid in [
            format!("//uploads/{node_id}/clip.mp4"),
            format!(" /uploads/{node_id}/clip.mp4"),
            format!("/uploads/{}/clip.mp4", Uuid::now_v7()),
            format!("/uploads/{node_id}/../clip.mp4"),
            format!("/uploads/{node_id}/clip.mp4?token=secret"),
        ] {
            assert!(
                agent_delete_request(&target(invalid.clone())).is_err(),
                "accepted invalid asset source path {invalid:?}"
            );
        }
        assert!(
            agent_delete_request(&MediaUploadAssetDeleteTarget {
                node_id,
                source_url: format!("/uploads/{node_id}/clip.mp4"),
                file_size: 0,
                file_deleted: false,
            })
            .is_err()
        );
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

    fn sample_load(total: u64, available: u64) -> NodeLiveLoad {
        NodeLiveLoad {
            connected: true,
            running_tasks: 0,
            starting_tasks: 0,
            stopping_tasks: 0,
            orphaned_tasks: 0,
            runtime_slot_loads: Vec::new(),
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
            runtime_slot_loads: None,
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
