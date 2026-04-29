use std::{
    collections::BTreeSet,
    path::{Component, Path as FsPath, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::Context;
use axum::{
    Json,
    body::Body,
    extract::{Multipart, Path as AxumPath, State, multipart::Field},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{Datelike, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::{fs, io::AsyncWriteExt, process::Command, time::timeout};
use tokio_util::io::ReaderStream;
use tracing::warn;
use uuid::Uuid;

use crate::{AppState, config::AgentSettings};

const DEFAULT_UPLOAD_DURATION_SEC: u64 = 0;
const UPLOAD_DURATION_PROBE_SIZE_BYTES: &str = "104857600";
const UPLOAD_DURATION_ANALYZE_DURATION_US: &str = "100000000";

#[derive(Debug, Clone)]
pub(crate) struct UploadConfig {
    pub work_root: PathBuf,
    pub max_bytes: u64,
    pub allowed_extensions: BTreeSet<String>,
    pub probe_timeout: Duration,
    pub ffprobe_bin: String,
    pub public_media_base_url: Option<String>,
}

impl UploadConfig {
    pub fn from_settings(settings: &AgentSettings) -> anyhow::Result<Self> {
        let explicit_media_base_url = settings
            .public_media_base_url
            .trim()
            .trim_end_matches('/')
            .to_string();
        let public_media_base_url =
            (!explicit_media_base_url.is_empty()).then_some(explicit_media_base_url);
        let allowed_extensions = settings
            .upload_allowed_extensions
            .iter()
            .map(|value| value.trim().trim_start_matches('.').to_ascii_lowercase())
            .filter(|value| !value.is_empty())
            .collect::<BTreeSet<_>>();
        anyhow::ensure!(
            !allowed_extensions.is_empty(),
            "UPLOAD_ALLOWED_EXTENSIONS must not be empty"
        );

        Ok(Self {
            work_root: normalize_filesystem_path(&settings.work_root)?,
            max_bytes: settings.upload_max_bytes,
            allowed_extensions,
            probe_timeout: Duration::from_secs(settings.upload_probe_timeout_sec),
            ffprobe_bin: settings.ffprobe_bin.trim().to_string(),
            public_media_base_url,
        })
    }
}

#[derive(Debug, Serialize)]
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

pub(crate) async fn upload_media(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<Json<UploadMediaResponse>, UploadError> {
    let mut uploaded = None;
    while let Some(field) = multipart.next_field().await? {
        let name = field.name().map(str::to_string).unwrap_or_default();
        if name != "file" {
            continue;
        }
        if uploaded.is_some() {
            return Err(UploadError::bad_request("only one file field is supported"));
        }

        let file_name = sanitize_file_name(field.file_name().unwrap_or("upload.bin"));
        let content_type = field.content_type().map(str::to_string);
        uploaded = Some(
            persist_uploaded_file(
                &state.upload,
                state.node_id,
                field,
                file_name,
                content_type,
                &headers,
            )
            .await?,
        );
    }

    uploaded
        .map(Json)
        .ok_or_else(|| UploadError::bad_request("file field is required"))
}

pub(crate) async fn serve_media_file(
    State(state): State<AppState>,
    AxumPath(path): AxumPath<String>,
) -> Result<Response, UploadError> {
    let relative = normalize_media_relative_path(&path)?;
    let root = fs::canonicalize(&state.upload.work_root)
        .await
        .map_err(|_| UploadError::not_found("media file not found"))?;
    let file_path = fs::canonicalize(state.upload.work_root.join(&relative))
        .await
        .map_err(|_| UploadError::not_found("media file not found"))?;
    if !file_path.starts_with(&root) {
        return Err(UploadError::bad_request(
            "media path must stay under upload root",
        ));
    }

    let metadata = fs::metadata(&file_path)
        .await
        .map_err(|_| UploadError::not_found("media file not found"))?;
    if !metadata.is_file() {
        return Err(UploadError::not_found("media file not found"));
    }

    let file = fs::File::open(&file_path)
        .await
        .context("open media file failed")?;
    let body = Body::from_stream(ReaderStream::new(file));
    let mut response = Response::new(body);
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Ok(value) = HeaderValue::from_str(&metadata.len().to_string()) {
        headers.insert(header::CONTENT_LENGTH, value);
    }
    if let Some(content_type) =
        content_type_from_extension(relative.extension().and_then(|value| value.to_str()))
    {
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    }
    Ok(response)
}

pub(crate) async fn delete_media_file(
    State(state): State<AppState>,
    AxumPath(path): AxumPath<String>,
) -> Result<StatusCode, UploadError> {
    let relative = normalize_media_relative_path(&path)?;
    let root = fs::canonicalize(&state.upload.work_root)
        .await
        .context("canonicalize upload root failed")?;
    let target_path = state.upload.work_root.join(&relative);
    let metadata = match fs::symlink_metadata(&target_path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(StatusCode::NO_CONTENT);
        }
        Err(error) => {
            return Err(UploadError::internal(format!(
                "inspect media file failed: {error}"
            )));
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(UploadError::bad_request(
            "media delete path must not be a symbolic link",
        ));
    }
    if !metadata.is_file() {
        return Err(UploadError::bad_request(
            "media delete path must reference a regular file",
        ));
    }
    let canonical = fs::canonicalize(&target_path)
        .await
        .context("canonicalize media file failed")?;
    if !canonical.starts_with(&root) {
        return Err(UploadError::bad_request(
            "media path must stay under upload root",
        ));
    }
    match fs::remove_file(&canonical).await {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(StatusCode::NO_CONTENT),
        Err(error) => Err(UploadError::internal(format!(
            "delete media file failed: {error}"
        ))),
    }
}

async fn persist_uploaded_file(
    config: &UploadConfig,
    node_id: Uuid,
    mut field: Field<'_>,
    file_name: String,
    content_type: Option<String>,
    headers: &HeaderMap,
) -> Result<UploadMediaResponse, UploadError> {
    let extension = extension_from_file_name(&file_name)
        .filter(|extension| config.allowed_extensions.contains(extension))
        .ok_or_else(|| UploadError::bad_request("unsupported media file extension"))?;
    let upload_id = Uuid::now_v7();
    let now = Utc::now();
    let relative = PathBuf::from("uploads")
        .join(node_id.to_string())
        .join(format!("{:04}", now.year()))
        .join(format!("{:02}", now.month()))
        .join(format!("{:02}", now.day()))
        .join(format!("{upload_id}.{extension}"));
    let source_url = path_to_url(&relative);
    let target_path = config.work_root.join(&relative);
    let parent = target_path
        .parent()
        .ok_or_else(|| UploadError::internal("upload target parent missing"))?;
    fs::create_dir_all(parent)
        .await
        .context("create upload directory failed")?;

    let temp_path = target_path.with_extension(format!("{extension}.uploading-{upload_id}"));
    let mut file = fs::File::create(&temp_path)
        .await
        .context("create upload temp file failed")?;
    let mut hasher = Sha256::new();
    let mut file_size = 0_u64;

    while let Some(chunk) = field.chunk().await? {
        let next_size = file_size
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| UploadError::bad_request("upload file is too large"))?;
        if next_size > config.max_bytes {
            let _ = fs::remove_file(&temp_path).await;
            return Err(UploadError::bad_request("upload file is too large"));
        }
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .context("write upload file failed")?;
        file_size = next_size;
    }
    file.flush().await.context("flush upload file failed")?;
    drop(file);

    if file_size == 0 {
        let _ = fs::remove_file(&temp_path).await;
        return Err(UploadError::bad_request("upload file must not be empty"));
    }

    if let Err(error) = fs::rename(&temp_path, &target_path).await {
        let _ = fs::remove_file(&temp_path).await;
        return Err(UploadError::internal(format!(
            "commit upload file failed: {error}"
        )));
    }

    let duration_sec = probe_duration_sec_or_default(config, &target_path).await;

    let content_type = content_type
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            content_type_from_extension(Some(extension.as_str()))
                .unwrap_or("application/octet-stream")
                .to_string()
        });
    let http_url = build_http_url(config, headers, &source_url)?;

    Ok(UploadMediaResponse {
        id: upload_id.to_string(),
        file_name,
        source_url,
        http_url,
        duration_sec,
        file_size,
        sha256: format!("{:x}", hasher.finalize()),
        content_type,
        created_at: now.timestamp_millis(),
    })
}

async fn probe_duration_sec_or_default(config: &UploadConfig, path: &FsPath) -> u64 {
    match probe_duration_sec(config, path).await {
        Ok(duration_sec) => duration_sec,
        Err(error) => {
            warn!(
                status = %error.status,
                error = %error.message,
                path = %path.display(),
                "media upload duration probe failed; using default duration"
            );
            DEFAULT_UPLOAD_DURATION_SEC
        }
    }
}

async fn probe_duration_sec(config: &UploadConfig, path: &FsPath) -> Result<u64, UploadError> {
    let output = timeout(
        config.probe_timeout,
        Command::new(&config.ffprobe_bin)
            .arg("-v")
            .arg("error")
            .arg("-probesize")
            .arg(UPLOAD_DURATION_PROBE_SIZE_BYTES)
            .arg("-analyzeduration")
            .arg(UPLOAD_DURATION_ANALYZE_DURATION_US)
            .arg("-show_entries")
            .arg("format=duration")
            .arg("-of")
            .arg("default=noprint_wrappers=1:nokey=1")
            .arg(path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    )
    .await
    .map_err(|_| UploadError::bad_request("media duration probe timeout"))?
    .context("run ffprobe failed")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(UploadError::bad_request(format!(
            "media duration probe failed: {}",
            stderr.trim()
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let duration = stdout
        .lines()
        .find_map(|line| line.trim().parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(|| UploadError::bad_request("media duration probe failed"))?;
    Ok(duration.ceil() as u64)
}

fn build_http_url(
    config: &UploadConfig,
    headers: &HeaderMap,
    source_url: &str,
) -> Result<String, UploadError> {
    let base = if let Some(base) = config
        .public_media_base_url
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        base.to_string()
    } else {
        let host = header_value(headers, "x-forwarded-host")
            .or_else(|| header_value(headers, header::HOST.as_str()))
            .ok_or_else(|| UploadError::internal("request host missing"))?;
        let proto =
            header_value(headers, "x-forwarded-proto").unwrap_or_else(|| "http".to_string());
        format!("{}://{}", proto.trim_end_matches("://"), host)
    };
    Ok(format!(
        "{}/media/{}",
        base.trim_end_matches('/'),
        source_url
    ))
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn sanitize_file_name(value: &str) -> String {
    FsPath::new(value)
        .file_name()
        .and_then(|value| value.to_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("upload.bin")
        .to_string()
}

fn extension_from_file_name(value: &str) -> Option<String> {
    FsPath::new(value)
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|value| !value.is_empty())
}

fn normalize_media_relative_path(value: &str) -> Result<PathBuf, UploadError> {
    let trimmed = value.trim().trim_start_matches('/');
    if trimmed.is_empty() {
        return Err(UploadError::bad_request("media path must not be empty"));
    }

    let mut normalized = PathBuf::new();
    for component in FsPath::new(trimmed).components() {
        match component {
            Component::Normal(segment) => normalized.push(segment),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(UploadError::bad_request("media path must not contain '..'"));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(UploadError::bad_request("media path must be relative"));
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(UploadError::bad_request("media path must not be empty"));
    }
    Ok(normalized)
}

fn normalize_filesystem_path(value: &str) -> anyhow::Result<PathBuf> {
    let path = FsPath::new(value.trim());
    anyhow::ensure!(!path.as_os_str().is_empty(), "file path must not be empty");
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(normalize_path_components(path))
}

fn normalize_path_components(path: PathBuf) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn path_to_url(path: &FsPath) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(segment) => Some(segment.to_string_lossy().to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn content_type_from_extension(extension: Option<&str>) -> Option<&'static str> {
    match extension.map(|value| value.to_ascii_lowercase()) {
        Some(value) if value == "mp4" || value == "m4v" => Some("video/mp4"),
        Some(value) if value == "mov" => Some("video/quicktime"),
        Some(value) if value == "mkv" => Some("video/x-matroska"),
        Some(value) if value == "webm" => Some("video/webm"),
        Some(value) if matches!(value.as_str(), "ts" | "m2ts" | "mts") => Some("video/mp2t"),
        Some(value) if value == "flv" => Some("video/x-flv"),
        _ => None,
    }
}

#[derive(Debug)]
pub(crate) struct UploadError {
    status: StatusCode,
    message: String,
}

impl UploadError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
}

impl From<anyhow::Error> for UploadError {
    fn from(error: anyhow::Error) -> Self {
        Self::internal(error.to_string())
    }
}

impl From<axum::extract::multipart::MultipartError> for UploadError {
    fn from(error: axum::extract::multipart::MultipartError) -> Self {
        Self::bad_request(format!("invalid multipart payload: {error}"))
    }
}

impl IntoResponse for UploadError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({
                "code": if self.status.is_client_error() { "UPLOAD_BAD_REQUEST" } else { "UPLOAD_INTERNAL_ERROR" },
                "message": self.message,
            })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        path::{Path as FsPath, PathBuf},
        time::Duration,
    };

    use super::{
        DEFAULT_UPLOAD_DURATION_SEC, UPLOAD_DURATION_ANALYZE_DURATION_US,
        UPLOAD_DURATION_PROBE_SIZE_BYTES, UploadConfig, content_type_from_extension,
        delete_media_file, normalize_media_relative_path, probe_duration_sec,
        probe_duration_sec_or_default,
    };
    use crate::{AgentReadiness, AppState};
    use axum::extract::{Path as AxumPath, State};
    use chrono::Utc;
    use uuid::Uuid;

    #[test]
    fn normalize_media_relative_path_rejects_parent_segments() {
        let error = normalize_media_relative_path("../demo.mp4").expect_err("path should fail");

        assert!(error.message.contains(".."));
    }

    #[test]
    fn normalize_media_relative_path_cleans_current_dir_segments() {
        let path = normalize_media_relative_path("/uploads/./demo.mp4").expect("path should pass");

        assert_eq!(path.to_string_lossy(), "uploads/demo.mp4");
    }

    #[test]
    fn content_type_matches_supported_video_extensions() {
        assert_eq!(content_type_from_extension(Some("mp4")), Some("video/mp4"));
        assert_eq!(content_type_from_extension(Some("ts")), Some("video/mp2t"));
        assert_eq!(content_type_from_extension(Some("bin")), None);
    }

    #[cfg(unix)]
    fn make_executable(path: &FsPath) {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = std::fs::metadata(path)
            .expect("mock ffprobe metadata should exist")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).expect("mock ffprobe should be executable");
    }

    #[cfg(unix)]
    fn write_mock_ffprobe(path: &FsPath, body: &str) {
        std::fs::write(path, body).expect("mock ffprobe should be written");
        make_executable(path);
    }

    #[cfg(unix)]
    fn test_upload_config(temp_root: &FsPath, ffprobe_bin: &FsPath) -> UploadConfig {
        UploadConfig {
            work_root: temp_root.join("work"),
            max_bytes: 1024 * 1024,
            allowed_extensions: ["mp4", "ts"]
                .into_iter()
                .map(str::to_string)
                .collect::<BTreeSet<_>>(),
            probe_timeout: Duration::from_secs(5),
            ffprobe_bin: ffprobe_bin.to_string_lossy().to_string(),
            public_media_base_url: None,
        }
    }

    #[cfg(unix)]
    fn temp_upload_probe_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{name}-{}", uuid::Uuid::now_v7()))
    }

    #[cfg(unix)]
    fn test_app_state(work_root: PathBuf) -> AppState {
        AppState {
            started_at: Utc::now(),
            environment: "test".to_string(),
            readiness: AgentReadiness {
                ffmpeg_available: true,
                ffprobe_available: true,
                work_root_exists: true,
            },
            node_id: Uuid::now_v7(),
            upload: UploadConfig {
                work_root,
                max_bytes: 1024 * 1024,
                allowed_extensions: ["mp4"].into_iter().map(str::to_string).collect(),
                probe_timeout: Duration::from_secs(5),
                ffprobe_bin: "ffprobe".to_string(),
                public_media_base_url: None,
            },
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn upload_duration_probe_uses_larger_probe_window() {
        let temp_root = temp_upload_probe_root("streamserver-upload-duration-probe");
        std::fs::create_dir_all(&temp_root).expect("temp root should be created");
        let args_path = temp_root.join("ffprobe-args.txt");
        let ffprobe_bin = temp_root.join("mock-ffprobe.sh");
        write_mock_ffprobe(
            &ffprobe_bin,
            &format!(
                r#"#!/bin/sh
: > '{}'
for arg in "$@"; do
  printf '%s\n' "$arg" >> '{}'
done
printf '1.2\n'
"#,
                args_path.display(),
                args_path.display()
            ),
        );
        let media_path = temp_root.join("sample.ts");
        std::fs::write(&media_path, b"demo").expect("sample media should be written");
        let config = test_upload_config(&temp_root, &ffprobe_bin);

        let duration_sec = probe_duration_sec(&config, &media_path)
            .await
            .expect("duration probe should pass");

        assert_eq!(duration_sec, 2);
        let args = std::fs::read_to_string(&args_path).expect("args should be recorded");
        let arg_lines = args.lines().collect::<Vec<_>>();
        assert!(
            arg_lines
                .windows(2)
                .any(|window| window == ["-probesize", UPLOAD_DURATION_PROBE_SIZE_BYTES])
        );
        assert!(
            arg_lines
                .windows(2)
                .any(|window| window == ["-analyzeduration", UPLOAD_DURATION_ANALYZE_DURATION_US])
        );

        let _ = std::fs::remove_dir_all(&temp_root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn upload_duration_probe_failure_returns_default_and_keeps_file() {
        let temp_root = temp_upload_probe_root("streamserver-upload-duration-fallback");
        std::fs::create_dir_all(&temp_root).expect("temp root should be created");
        let ffprobe_bin = temp_root.join("mock-ffprobe-fail.sh");
        write_mock_ffprobe(
            &ffprobe_bin,
            r#"#!/bin/sh
echo 'pps decode failed' >&2
exit 1
"#,
        );
        let media_path = temp_root.join("sample.mp4");
        std::fs::write(&media_path, b"demo").expect("sample media should be written");
        let config = test_upload_config(&temp_root, &ffprobe_bin);

        let duration_sec = probe_duration_sec_or_default(&config, &media_path).await;

        assert_eq!(duration_sec, DEFAULT_UPLOAD_DURATION_SEC);
        assert!(media_path.exists());

        let _ = std::fs::remove_dir_all(&temp_root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn delete_media_file_removes_regular_file_and_ignores_missing() {
        let temp_root = temp_upload_probe_root("streamserver-upload-delete");
        let work_root = temp_root.join("work");
        let relative = PathBuf::from("uploads/node-a/2026/04/29/demo.mp4");
        let target = work_root.join(&relative);
        std::fs::create_dir_all(target.parent().expect("target parent should exist"))
            .expect("upload dir should be created");
        std::fs::write(&target, b"demo").expect("media file should be written");
        let state = test_app_state(work_root.clone());

        let status = delete_media_file(
            State(state.clone()),
            AxumPath(relative.to_string_lossy().to_string()),
        )
        .await
        .expect("delete should pass");
        assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
        assert!(!target.exists());

        let status = delete_media_file(
            State(state),
            AxumPath(relative.to_string_lossy().to_string()),
        )
        .await
        .expect("missing file should still pass");
        assert_eq!(status, axum::http::StatusCode::NO_CONTENT);

        let _ = std::fs::remove_dir_all(&temp_root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn delete_media_file_rejects_parent_segments() {
        let temp_root = temp_upload_probe_root("streamserver-upload-delete-parent");
        let work_root = temp_root.join("work");
        std::fs::create_dir_all(&work_root).expect("work root should be created");
        let state = test_app_state(work_root);

        let error = delete_media_file(State(state), AxumPath("../demo.mp4".to_string()))
            .await
            .expect_err("parent segments should fail");

        assert_eq!(error.status, axum::http::StatusCode::BAD_REQUEST);

        let _ = std::fs::remove_dir_all(&temp_root);
    }
}
