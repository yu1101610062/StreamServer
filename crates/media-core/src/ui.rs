use axum::{
    Json, Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use serde::Serialize;
use std::{
    env,
    path::{Component, Path as FsPath, PathBuf},
    sync::OnceLock,
};
use tokio::fs;

use crate::{
    AppState, PeerAddress, auth::ApiPermission, authorize_business_request, error::AppError,
    repository::TaskPreview,
};
use media_domain::TaskSpec;

const UI_DIR_ENV: &str = "STREAMSERVER_UI_DIR";

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(root_redirect))
        .route("/login", get(shell))
        .route("/overview", get(shell))
        .route("/api-docs", get(shell))
        .route("/tasks", get(shell))
        .route("/tasks/{*rest}", get(shell))
        .route("/streams", get(shell))
        .route("/multicast", get(shell))
        .route("/records", get(shell))
        .route("/file-artifacts", get(shell))
        .route("/security", get(shell))
        .route("/nodes", get(shell))
        .route("/debug", get(shell))
        .route("/debug/{*rest}", get(shell))
        .route("/assets/{*path}", get(asset))
        .route("/favicon.ico", get(favicon))
}

pub(crate) async fn current_session(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<CurrentSessionResponse>, AppError> {
    let principal = state.auth.session(&headers)?;
    Ok(Json(CurrentSessionResponse {
        auth_enabled: state.auth.enabled(),
        auth_mode: state.auth.mode(),
        subject: principal.subject().to_string(),
        role: principal.role(),
        must_change_password: principal.must_change_password(),
        permissions: principal
            .granted_permissions()
            .into_iter()
            .map(ApiPermission::as_str)
            .collect(),
        environment: state.environment.clone(),
    }))
}

pub(crate) async fn preview_task(
    State(state): State<AppState>,
    PeerAddress(peer): PeerAddress,
    headers: HeaderMap,
    Json(task): Json<TaskSpec>,
) -> Result<Json<TaskPreview>, AppError> {
    authorize_business_request(&state, &headers, peer, ApiPermission::TaskWrite).await?;
    Ok(Json(state.repository.preview_task_spec(task).await?))
}

async fn root_redirect() -> Redirect {
    Redirect::temporary("/overview")
}

async fn shell() -> Response {
    let index_path = ui_dir().join("index.html");
    match fs::read_to_string(&index_path).await {
        Ok(html) => {
            let mut response = Html(html).into_response();
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("no-cache, no-store, must-revalidate"),
            );
            response
        }
        Err(_) => missing_ui_response(index_path),
    }
}

async fn asset(Path(path): Path<String>) -> Response {
    let Some(relative_path) = sanitize_asset_path(&path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let assets_path = ui_dir().join("assets").join(&relative_path);
    match fs::read(&assets_path).await {
        Ok(bytes) => asset_response(content_type_for(&assets_path), bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let legacy_path = ui_dir().join(&relative_path);
            match fs::read(&legacy_path).await {
                Ok(bytes) => asset_response(content_type_for(&legacy_path), bytes),
                Err(legacy_error) if legacy_error.kind() == std::io::ErrorKind::NotFound => {
                    StatusCode::NOT_FOUND.into_response()
                }
                Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            }
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn favicon() -> impl IntoResponse {
    StatusCode::NO_CONTENT
}

fn asset_response(content_type: &'static str, body: Vec<u8>) -> Response {
    let mut response = Body::from(body).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-store, must-revalidate"),
    );
    response
}

fn missing_ui_response(index_path: PathBuf) -> Response {
    let html = format!(
        r#"<!doctype html>
<html lang="zh-CN">
  <head>
    <meta charset="utf-8" />
    <title>StreamServer Console Unavailable</title>
  </head>
  <body style="font-family: sans-serif; padding: 32px;">
    <h1>控制台静态资源不可用</h1>
    <p>未找到前端构建产物：<code>{}</code></p>
    <p>请先在 <code>crates/media-core/frontend</code> 下执行 <code>npm run build</code>，或确认运行环境中的 <code>{}</code> 指向正确的构建目录。</p>
  </body>
</html>"#,
        index_path.display(),
        UI_DIR_ENV,
    );
    let mut response = Html(html).into_response();
    *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
    response
}

fn ui_dir() -> &'static PathBuf {
    static UI_DIR: OnceLock<PathBuf> = OnceLock::new();
    UI_DIR.get_or_init(|| {
        env::var(UI_DIR_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ui"))
    })
}

fn sanitize_asset_path(path: &str) -> Option<PathBuf> {
    let candidate = FsPath::new(path);
    let mut sanitized = PathBuf::new();
    for component in candidate.components() {
        match component {
            Component::Normal(value) => sanitized.push(value),
            Component::CurDir => {}
            _ => return None,
        }
    }
    if sanitized.as_os_str().is_empty() {
        None
    } else {
        Some(sanitized)
    }
}

fn content_type_for(path: &FsPath) -> &'static str {
    match path.extension().and_then(|value| value.to_str()) {
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("html") => "text/html; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("map") => "application/json",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct CurrentSessionResponse {
    auth_enabled: bool,
    auth_mode: crate::config::AuthMode,
    subject: String,
    role: crate::auth::ApiRole,
    must_change_password: bool,
    permissions: Vec<&'static str>,
    environment: String,
}
