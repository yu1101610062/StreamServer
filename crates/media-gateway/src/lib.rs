use std::{
    collections::HashMap,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use anyhow::Context;
use axum::{
    Json, Router,
    body::Body,
    extract::{Path as AxumPath, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{fs, io::AsyncWriteExt, sync::Mutex};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub public_base_url: String,
    pub work_root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct GatewayState {
    config: GatewayConfig,
    http: reqwest::Client,
    relays: Arc<Mutex<HashMap<Uuid, RelayEntry>>>,
    prefetches: Arc<Mutex<HashMap<Uuid, PrefetchState>>>,
}

#[derive(Debug, Clone)]
struct RelayEntry {
    source_url: String,
    token: String,
}

#[derive(Debug, Clone, Serialize)]
struct PrefetchState {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RelayRequest {
    task_id: Uuid,
    source_url: String,
}

#[derive(Debug, Deserialize)]
struct PrefetchRequest {
    task_id: Uuid,
    source_url: String,
    target_path: String,
}

#[derive(Debug, Deserialize)]
struct RelayQuery {
    token: Option<String>,
}

impl GatewayState {
    pub fn new(config: GatewayConfig) -> Self {
        Self {
            config,
            http: reqwest::Client::new(),
            relays: Arc::new(Mutex::new(HashMap::new())),
            prefetches: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

pub fn build_app(state: GatewayState) -> Router {
    Router::new()
        .route("/api/healthz", get(healthz))
        .route("/api/relays", post(create_relay))
        .route("/api/relays/{task_id}", delete(delete_relay))
        .route("/relay/{task_id}", get(relay_stream))
        .route("/api/prefetch", post(create_prefetch))
        .route("/api/prefetch/{task_id}", get(get_prefetch))
        .with_state(state)
}

pub fn safe_target_path(root: &Path, relative_path: &str) -> anyhow::Result<PathBuf> {
    let relative_path = relative_path.trim();
    anyhow::ensure!(!relative_path.is_empty(), "target_path must not be empty");
    anyhow::ensure!(
        !relative_path.starts_with("uploads/"),
        "target_path must not use uploads node-affinity paths"
    );

    let mut clean = PathBuf::new();
    for component in Path::new(relative_path).components() {
        match component {
            Component::Normal(value) => clean.push(value),
            Component::CurDir => {}
            _ => anyhow::bail!("target_path must be relative and stay under work_root"),
        }
    }
    anyhow::ensure!(
        !clean.as_os_str().is_empty(),
        "target_path must not be empty"
    );
    Ok(root.join(clean))
}

async fn healthz() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}

async fn create_relay(
    State(state): State<GatewayState>,
    Json(request): Json<RelayRequest>,
) -> impl IntoResponse {
    if !is_http_url(&request.source_url) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "source_url must start with http:// or https://"})),
        )
            .into_response();
    }

    let token = Uuid::now_v7().to_string();
    state.relays.lock().await.insert(
        request.task_id,
        RelayEntry {
            source_url: request.source_url,
            token: token.clone(),
        },
    );
    let base = state.config.public_base_url.trim_end_matches('/');
    Json(json!({
        "relay_url": format!("{base}/relay/{}?token={token}", request.task_id)
    }))
    .into_response()
}

async fn delete_relay(
    State(state): State<GatewayState>,
    AxumPath(task_id): AxumPath<Uuid>,
) -> impl IntoResponse {
    state.relays.lock().await.remove(&task_id);
    StatusCode::NO_CONTENT
}

async fn relay_stream(
    State(state): State<GatewayState>,
    AxumPath(task_id): AxumPath<Uuid>,
    Query(query): Query<RelayQuery>,
) -> Response {
    let Some(entry) = state.relays.lock().await.get(&task_id).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if query.token.as_deref() != Some(entry.token.as_str()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    match state.http.get(entry.source_url).send().await {
        Ok(upstream) if upstream.status().is_success() => {
            let status = upstream.status();
            let content_type = upstream.headers().get(header::CONTENT_TYPE).cloned();
            let mut response = Response::builder().status(status);
            if let Some(content_type) = content_type {
                response = response.header(header::CONTENT_TYPE, content_type);
            }
            response
                .body(Body::from_stream(upstream.bytes_stream()))
                .unwrap_or_else(|error| {
                    (
                        StatusCode::BAD_GATEWAY,
                        format!("failed to build relay response: {error}"),
                    )
                        .into_response()
                })
        }
        Ok(upstream) => (
            upstream.status(),
            format!("upstream returned {}", upstream.status()),
        )
            .into_response(),
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            format!("failed to connect upstream: {error}"),
        )
            .into_response(),
    }
}

async fn create_prefetch(
    State(state): State<GatewayState>,
    Json(request): Json<PrefetchRequest>,
) -> impl IntoResponse {
    if !is_http_url(&request.source_url) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "source_url must start with http:// or https://"})),
        )
            .into_response();
    }
    let Ok(final_path) = safe_target_path(&state.config.work_root, &request.target_path) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "target_path must be a non-upload relative path"})),
        )
            .into_response();
    };

    state.prefetches.lock().await.insert(
        request.task_id,
        PrefetchState {
            status: "pending".to_string(),
            source_url: None,
            failure_reason: None,
        },
    );

    let task_id = request.task_id;
    let target_path = request.target_path;
    let source_url = request.source_url;
    let http = state.http.clone();
    let prefetches = state.prefetches.clone();
    tokio::spawn(async move {
        let result = download_to_file(http, &source_url, &final_path).await;
        let mut prefetches = prefetches.lock().await;
        prefetches.insert(
            task_id,
            match result {
                Ok(()) => PrefetchState {
                    status: "ready".to_string(),
                    source_url: Some(target_path),
                    failure_reason: None,
                },
                Err(error) => PrefetchState {
                    status: "failed".to_string(),
                    source_url: None,
                    failure_reason: Some(error.to_string()),
                },
            },
        );
    });

    (StatusCode::ACCEPTED, Json(json!({"status": "pending"}))).into_response()
}

async fn get_prefetch(
    State(state): State<GatewayState>,
    AxumPath(task_id): AxumPath<Uuid>,
) -> impl IntoResponse {
    let prefetches = state.prefetches.lock().await;
    match prefetches.get(&task_id) {
        Some(status) => Json(status).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn download_to_file(
    http: reqwest::Client,
    source_url: &str,
    final_path: &Path,
) -> anyhow::Result<()> {
    let part_path = final_path.with_extension(format!(
        "{}.part",
        final_path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("download")
    ));
    if let Some(parent) = final_path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let mut response = http
        .get(source_url)
        .send()
        .await?
        .error_for_status()
        .context("source download failed")?;
    let mut file = fs::File::create(&part_path).await?;
    while let Some(chunk) = response.chunk().await? {
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    drop(file);
    fs::rename(&part_path, final_path).await?;
    Ok(())
}

fn is_http_url(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://")
}
