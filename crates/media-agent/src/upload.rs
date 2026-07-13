use std::{
    collections::BTreeSet,
    future::Future,
    io::SeekFrom,
    path::{Component, Path as FsPath, PathBuf},
    process::Stdio,
    time::Duration,
};
#[cfg(target_os = "linux")]
use std::{
    ffi::{CStr, CString, OsStr},
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::ffi::OsStrExt,
    },
};

use anyhow::Context;
use axum::{
    Json,
    body::{Body, Bytes},
    extract::{
        Multipart, Path as AxumPath, State,
        multipart::{Field, MultipartError},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{Datelike, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    process::Command,
    time::timeout,
};
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
    pub chunk_idle_timeout: Duration,
}

impl UploadConfig {
    pub fn from_settings(settings: &AgentSettings) -> anyhow::Result<Self> {
        let explicit_media_base_url = settings
            .public_media_base_url
            .trim()
            .trim_end_matches('/')
            .to_string();
        let public_media_base_url = if !explicit_media_base_url.is_empty() {
            Some(explicit_media_base_url)
        } else {
            let public_media_addr = settings
                .public_media_addr
                .trim()
                .parse::<std::net::SocketAddr>()
                .context("AGENT_PUBLIC_MEDIA_ADDR must be a socket address")?;
            public_media_addr.ip().is_loopback().then(|| {
                let tls_configured = !settings.public_media_tls_cert_path.trim().is_empty()
                    && !settings.public_media_tls_key_path.trim().is_empty();
                let scheme = if tls_configured { "https" } else { "http" };
                format!("{scheme}://{public_media_addr}")
            })
        };
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
            chunk_idle_timeout: Duration::from_secs(settings.management_chunk_idle_timeout_sec),
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
    headers: HeaderMap,
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

    let mut file = fs::File::open(&file_path)
        .await
        .context("open media file failed")?;
    let media_range = match parse_single_byte_range(
        headers
            .get(header::RANGE)
            .and_then(|value| value.to_str().ok()),
        metadata.len(),
    ) {
        Ok(range) => range,
        Err(RangeParseError::Invalid | RangeParseError::Unsatisfiable) => {
            return Ok(range_not_satisfiable_response(metadata.len()));
        }
    };

    let (status, content_length, content_range) = if let Some(range) = media_range {
        file.seek(SeekFrom::Start(range.start))
            .await
            .context("seek media file failed")?;
        (
            StatusCode::PARTIAL_CONTENT,
            range.len(),
            Some(format!(
                "bytes {}-{}/{}",
                range.start,
                range.end,
                metadata.len()
            )),
        )
    } else {
        (StatusCode::OK, metadata.len(), None)
    };

    let body = Body::from_stream(ReaderStream::new(file.take(content_length)));
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Ok(value) = HeaderValue::from_str(&content_length.to_string()) {
        headers.insert(header::CONTENT_LENGTH, value);
    }
    if let Some(content_range) = content_range {
        if let Ok(value) = HeaderValue::from_str(&content_range) {
            headers.insert(header::CONTENT_RANGE, value);
        }
    }
    if let Some(content_type) =
        content_type_from_extension(relative.extension().and_then(|value| value.to_str()))
    {
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    }
    Ok(response)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MediaByteRange {
    start: u64,
    end: u64,
}

impl MediaByteRange {
    fn len(self) -> u64 {
        self.end - self.start + 1
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RangeParseError {
    Invalid,
    Unsatisfiable,
}

fn parse_single_byte_range(
    header_value: Option<&str>,
    file_len: u64,
) -> Result<Option<MediaByteRange>, RangeParseError> {
    let Some(header_value) = header_value
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    if file_len == 0 {
        return Err(RangeParseError::Unsatisfiable);
    }
    let spec = header_value
        .strip_prefix("bytes=")
        .ok_or(RangeParseError::Invalid)?;
    if spec.contains(',') {
        return Err(RangeParseError::Invalid);
    }
    let (start, end) = spec.split_once('-').ok_or(RangeParseError::Invalid)?;
    let start = start.trim();
    let end = end.trim();
    if start.is_empty() {
        let suffix_len = end.parse::<u64>().map_err(|_| RangeParseError::Invalid)?;
        if suffix_len == 0 {
            return Err(RangeParseError::Unsatisfiable);
        }
        let start = file_len.saturating_sub(suffix_len);
        return Ok(Some(MediaByteRange {
            start,
            end: file_len - 1,
        }));
    }

    let start = start.parse::<u64>().map_err(|_| RangeParseError::Invalid)?;
    if start >= file_len {
        return Err(RangeParseError::Unsatisfiable);
    }
    let end = if end.is_empty() {
        file_len - 1
    } else {
        end.parse::<u64>()
            .map_err(|_| RangeParseError::Invalid)?
            .min(file_len - 1)
    };
    if end < start {
        return Err(RangeParseError::Unsatisfiable);
    }
    Ok(Some(MediaByteRange { start, end }))
}

fn range_not_satisfiable_response(file_len: u64) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
    let headers = response.headers_mut();
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Ok(value) = HeaderValue::from_str(&format!("bytes */{file_len}")) {
        headers.insert(header::CONTENT_RANGE, value);
    }
    response
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

#[cfg(target_os = "linux")]
struct SecureUploadDestination {
    root: PathBuf,
    target_path: PathBuf,
    parent_fd: OwnedFd,
    target_name: CString,
    temp_name: CString,
    committed: bool,
}

#[cfg(target_os = "linux")]
impl SecureUploadDestination {
    fn temp_handle_path(&self) -> PathBuf {
        PathBuf::from(format!(
            "/proc/self/fd/{}/{}",
            self.parent_fd.as_raw_fd(),
            self.temp_name.to_string_lossy()
        ))
    }

    fn remove_temp(&self) {
        unsafe {
            libc::unlinkat(self.parent_fd.as_raw_fd(), self.temp_name.as_ptr(), 0);
        }
    }

    fn commit(mut self) -> Result<PathBuf, UploadError> {
        verify_upload_path_beneath(&self.root, &self.target_path, false)?;
        let before = fstatat_no_follow(self.parent_fd.as_raw_fd(), &self.temp_name)
            .context("inspect upload temp file before commit failed")?;
        if before.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(UploadError::bad_request(
                "upload temp path must be a regular file",
            ));
        }

        atomic_rename_noreplace(
            self.parent_fd.as_raw_fd(),
            &self.temp_name,
            self.parent_fd.as_raw_fd(),
            &self.target_name,
        )
        .map_err(|error| UploadError::internal(format!("commit upload file failed: {error}")))?;

        let verified = (|| -> Result<(), UploadError> {
            let after = fstatat_no_follow(self.parent_fd.as_raw_fd(), &self.target_name)
                .context("inspect committed upload file failed")?;
            if after.st_mode & libc::S_IFMT != libc::S_IFREG
                || after.st_dev != before.st_dev
                || after.st_ino != before.st_ino
            {
                return Err(UploadError::bad_request(
                    "committed upload path identity changed",
                ));
            }
            verify_upload_path_beneath(&self.root, &self.target_path, true)?;
            if unsafe { libc::fsync(self.parent_fd.as_raw_fd()) } != 0 {
                return Err(UploadError::internal(format!(
                    "sync upload directory failed: {}",
                    io::Error::last_os_error()
                )));
            }
            Ok(())
        })();
        if let Err(error) = verified {
            unsafe {
                libc::unlinkat(self.parent_fd.as_raw_fd(), self.target_name.as_ptr(), 0);
            }
            return Err(error);
        }
        self.committed = true;
        Ok(self.target_path.clone())
    }
}

#[cfg(target_os = "linux")]
impl Drop for SecureUploadDestination {
    fn drop(&mut self) {
        if !self.committed {
            self.remove_temp();
        }
    }
}

#[cfg(target_os = "linux")]
async fn create_secure_upload_destination(
    work_root: &FsPath,
    relative: &FsPath,
    temp_file_name: &str,
) -> Result<(SecureUploadDestination, fs::File), UploadError> {
    fs::create_dir_all(work_root)
        .await
        .context("create upload root failed")?;
    let root_metadata = fs::symlink_metadata(work_root)
        .await
        .context("inspect upload root failed")?;
    if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
        return Err(UploadError::bad_request(
            "upload root must be a real directory",
        ));
    }
    let root = fs::canonicalize(work_root)
        .await
        .context("canonicalize upload root failed")?;
    let target_name = relative
        .file_name()
        .ok_or_else(|| UploadError::bad_request("upload target file name missing"))?;
    let parent_relative = relative
        .parent()
        .ok_or_else(|| UploadError::bad_request("upload target parent missing"))?;
    let target_name = cstring_component(target_name)?;
    let temp_name = cstring_component(OsStr::new(temp_file_name))?;
    verify_relative_directory_path(parent_relative)?;

    let root_name = CString::new(root.as_os_str().as_bytes())
        .map_err(|_| UploadError::bad_request("upload root contains NUL"))?;
    let root_fd = unsafe {
        libc::open(
            root_name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if root_fd < 0 {
        return Err(UploadError::internal(format!(
            "open upload root failed: {}",
            io::Error::last_os_error()
        )));
    }
    let mut parent_fd = unsafe { OwnedFd::from_raw_fd(root_fd) };
    for component in parent_relative.components() {
        let Component::Normal(component) = component else {
            return Err(UploadError::bad_request(
                "upload directory path must be relative",
            ));
        };
        let component = cstring_component(component)?;
        let created = unsafe { libc::mkdirat(parent_fd.as_raw_fd(), component.as_ptr(), 0o750) };
        if created != 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(UploadError::internal(format!(
                    "create upload directory failed: {error}"
                )));
            }
        }
        let child_fd = unsafe {
            libc::openat(
                parent_fd.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if child_fd < 0 {
            return Err(UploadError::bad_request(format!(
                "upload directory path is not a real directory: {}",
                io::Error::last_os_error()
            )));
        }
        parent_fd = unsafe { OwnedFd::from_raw_fd(child_fd) };
    }

    let temp_fd = unsafe {
        libc::openat(
            parent_fd.as_raw_fd(),
            temp_name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if temp_fd < 0 {
        return Err(UploadError::internal(format!(
            "create upload temp file failed: {}",
            io::Error::last_os_error()
        )));
    }
    let std_file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(temp_fd) });
    let target_path = root.join(relative);
    let destination = SecureUploadDestination {
        root,
        target_path,
        parent_fd,
        target_name,
        temp_name,
        committed: false,
    };
    Ok((destination, fs::File::from_std(std_file)))
}

#[cfg(target_os = "linux")]
fn cstring_component(value: &OsStr) -> Result<CString, UploadError> {
    if value.is_empty() || value.as_bytes().contains(&b'/') {
        return Err(UploadError::bad_request("upload path component is invalid"));
    }
    CString::new(value.as_bytes())
        .map_err(|_| UploadError::bad_request("upload path component contains NUL"))
}

#[cfg(target_os = "linux")]
fn verify_relative_directory_path(path: &FsPath) -> Result<(), UploadError> {
    if path
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
    {
        Ok(())
    } else {
        Err(UploadError::bad_request(
            "upload directory path must contain only normal relative components",
        ))
    }
}

#[cfg(target_os = "linux")]
fn fstatat_no_follow(parent_fd: i32, name: &CString) -> io::Result<libc::stat> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::zeroed();
    let result = unsafe {
        libc::fstatat(
            parent_fd,
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        Ok(unsafe { metadata.assume_init() })
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn atomic_rename_noreplace(
    old_directory_fd: RawFd,
    old_name: &CStr,
    new_directory_fd: RawFd,
    new_name: &CStr,
) -> io::Result<()> {
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            old_directory_fd,
            old_name.as_ptr(),
            new_directory_fd,
            new_name.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    match result {
        0 => Ok(()),
        -1 => Err(io::Error::last_os_error()),
        unexpected => Err(io::Error::other(format!(
            "renameat2 syscall returned unexpected status {unexpected}"
        ))),
    }
}

#[cfg(target_os = "linux")]
fn verify_upload_path_beneath(
    root: &FsPath,
    target: &FsPath,
    target_must_exist: bool,
) -> Result<(), UploadError> {
    let canonical_root =
        std::fs::canonicalize(root).context("canonicalize upload root during commit failed")?;
    let parent = target
        .parent()
        .ok_or_else(|| UploadError::internal("upload target parent missing"))?;
    let canonical_parent =
        std::fs::canonicalize(parent).context("canonicalize upload parent during commit failed")?;
    if !canonical_parent.starts_with(&canonical_root) {
        return Err(UploadError::bad_request(
            "upload target escaped the configured root",
        ));
    }
    let relative_parent = parent
        .strip_prefix(root)
        .map_err(|_| UploadError::bad_request("upload parent escaped configured root"))?;
    let mut current = root.to_path_buf();
    for component in relative_parent.components() {
        current.push(component.as_os_str());
        let metadata = std::fs::symlink_metadata(&current)
            .context("inspect upload directory during commit failed")?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(UploadError::bad_request(
                "upload directory path must not contain symbolic links",
            ));
        }
    }
    if target_must_exist {
        let metadata =
            std::fs::symlink_metadata(target).context("inspect committed upload target failed")?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(UploadError::bad_request(
                "committed upload target must be a regular file",
            ));
        }
        let canonical_target =
            std::fs::canonicalize(target).context("canonicalize committed upload target failed")?;
        if !canonical_target.starts_with(&canonical_root) {
            return Err(UploadError::bad_request(
                "committed upload target escaped configured root",
            ));
        }
    }
    Ok(())
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
    let target_file_name = relative
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| UploadError::internal("upload target file name missing"))?;
    let temp_file_name = format!("{target_file_name}.uploading-{upload_id}");
    #[cfg(target_os = "linux")]
    let (destination, mut file) =
        create_secure_upload_destination(&config.work_root, &relative, &temp_file_name).await?;
    #[cfg(not(target_os = "linux"))]
    compile_error!("media-agent secure upload writes require Linux");
    let temp_path = destination.temp_handle_path();
    let mut hasher = Sha256::new();
    let mut file_size = 0_u64;

    while let Some(chunk) =
        next_upload_chunk_with_idle_cleanup(field.chunk(), config.chunk_idle_timeout, &temp_path)
            .await?
    {
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
    file.sync_all().await.context("sync upload file failed")?;
    drop(file);

    if file_size == 0 {
        let _ = fs::remove_file(&temp_path).await;
        return Err(UploadError::bad_request("upload file must not be empty"));
    }

    let target_path = destination.commit()?;

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

async fn next_upload_chunk_with_idle_cleanup<F>(
    next_chunk: F,
    idle_timeout: Duration,
    partial_path: &FsPath,
) -> Result<Option<Bytes>, UploadError>
where
    F: Future<Output = Result<Option<Bytes>, MultipartError>>,
{
    match timeout(idle_timeout, next_chunk).await {
        Ok(Ok(chunk)) => Ok(chunk),
        Ok(Err(error)) => {
            let _ = fs::remove_file(partial_path).await;
            Err(error.into())
        }
        Err(_) => {
            let _ = fs::remove_file(partial_path).await;
            Err(UploadError::request_timeout(
                "upload body was idle for too long",
            ))
        }
    }
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

pub(crate) fn normalize_media_relative_path(value: &str) -> Result<PathBuf, UploadError> {
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

pub(crate) fn path_to_url(path: &FsPath) -> String {
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

    fn request_timeout(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::REQUEST_TIMEOUT,
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

    #[cfg(target_os = "linux")]
    use super::atomic_rename_noreplace;
    use super::{
        DEFAULT_UPLOAD_DURATION_SEC, UPLOAD_DURATION_ANALYZE_DURATION_US,
        UPLOAD_DURATION_PROBE_SIZE_BYTES, UploadConfig, content_type_from_extension,
        create_secure_upload_destination, delete_media_file, next_upload_chunk_with_idle_cleanup,
        normalize_media_relative_path, parse_single_byte_range, probe_duration_sec,
        probe_duration_sec_or_default, serve_media_file,
    };
    use crate::{AgentReadiness, AppState, config::AgentSettings};
    use axum::{
        body::to_bytes,
        extract::{Path as AxumPath, State},
        http::{HeaderMap, HeaderValue, StatusCode, header},
    };
    use chrono::Utc;
    use uuid::Uuid;

    #[test]
    fn upload_config_derives_loopback_public_media_base_from_its_listener() {
        let mut settings = AgentSettings {
            public_media_addr: "127.0.0.1:18081".to_string(),
            ..AgentSettings::default()
        };

        let config = UploadConfig::from_settings(&settings).expect("loopback upload config");
        assert_eq!(
            config.public_media_base_url.as_deref(),
            Some("http://127.0.0.1:18081")
        );

        settings.public_media_addr = "[::1]:18443".to_string();
        settings.public_media_tls_cert_path = "/etc/streamserver/public.pem".to_string();
        settings.public_media_tls_key_path = "/etc/streamserver/public.key".to_string();
        let config = UploadConfig::from_settings(&settings).expect("TLS loopback upload config");
        assert_eq!(
            config.public_media_base_url.as_deref(),
            Some("https://[::1]:18443")
        );
    }

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
            chunk_idle_timeout: Duration::from_secs(30),
        }
    }

    #[cfg(unix)]
    fn temp_upload_probe_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{name}-{}", uuid::Uuid::now_v7()))
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn atomic_rename_noreplace_moves_source_to_missing_target() {
        use std::{ffi::CString, os::fd::AsRawFd};

        let temp_root = temp_upload_probe_root("streamserver-atomic-rename-success");
        std::fs::create_dir_all(&temp_root).expect("temp root should be created");
        std::fs::write(temp_root.join("source"), b"source-content")
            .expect("source should be written");
        let directory = std::fs::File::open(&temp_root).expect("directory should open");

        atomic_rename_noreplace(
            directory.as_raw_fd(),
            &CString::new("source").unwrap(),
            directory.as_raw_fd(),
            &CString::new("target").unwrap(),
        )
        .expect("rename should succeed");

        assert!(!temp_root.join("source").exists());
        assert_eq!(
            std::fs::read(temp_root.join("target")).expect("target should be readable"),
            b"source-content"
        );
        let _ = std::fs::remove_dir_all(temp_root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn atomic_rename_noreplace_preserves_both_files_when_target_exists() {
        use std::{ffi::CString, io::ErrorKind, os::fd::AsRawFd};

        let temp_root = temp_upload_probe_root("streamserver-atomic-rename-existing");
        std::fs::create_dir_all(&temp_root).expect("temp root should be created");
        std::fs::write(temp_root.join("source"), b"source-content")
            .expect("source should be written");
        std::fs::write(temp_root.join("target"), b"target-content")
            .expect("target should be written");
        let directory = std::fs::File::open(&temp_root).expect("directory should open");

        let error = atomic_rename_noreplace(
            directory.as_raw_fd(),
            &CString::new("source").unwrap(),
            directory.as_raw_fd(),
            &CString::new("target").unwrap(),
        )
        .expect_err("rename must not replace an existing target");

        assert_eq!(error.kind(), ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read(temp_root.join("source")).expect("source should remain readable"),
            b"source-content"
        );
        assert_eq!(
            std::fs::read(temp_root.join("target")).expect("target should remain readable"),
            b"target-content"
        );
        let _ = std::fs::remove_dir_all(temp_root);
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
                zlm_hook_listener_available: true,
            },
            node_id: Uuid::now_v7(),
            upload: UploadConfig {
                work_root,
                max_bytes: 1024 * 1024,
                allowed_extensions: ["mp4"].into_iter().map(str::to_string).collect(),
                probe_timeout: Duration::from_secs(5),
                ffprobe_bin: "ffprobe".to_string(),
                public_media_base_url: None,
                chunk_idle_timeout: Duration::from_secs(30),
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
    async fn upload_chunk_idle_timeout_removes_partial_file() {
        let temp_root = temp_upload_probe_root("streamserver-upload-idle-cleanup");
        std::fs::create_dir_all(&temp_root).unwrap();
        let partial = temp_root.join("partial.uploading-test");
        std::fs::write(&partial, b"partial").unwrap();

        let result = next_upload_chunk_with_idle_cleanup(
            std::future::pending::<
                Result<Option<axum::body::Bytes>, axum::extract::multipart::MultipartError>,
            >(),
            Duration::from_millis(20),
            &partial,
        )
        .await;

        assert!(result.is_err());
        assert!(!partial.exists());
        let _ = std::fs::remove_dir_all(temp_root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn upload_chunk_timeout_is_idle_not_total_duration() {
        let temp_root = temp_upload_probe_root("streamserver-upload-idle-not-total");
        std::fs::create_dir_all(&temp_root).unwrap();
        let partial = temp_root.join("partial.uploading-test");
        std::fs::write(&partial, b"partial").unwrap();
        let started = std::time::Instant::now();

        for _ in 0..4 {
            let chunk = next_upload_chunk_with_idle_cleanup(
                async {
                    tokio::time::sleep(Duration::from_millis(15)).await;
                    Ok::<_, axum::extract::multipart::MultipartError>(Some(
                        axum::body::Bytes::from_static(b"chunk"),
                    ))
                },
                Duration::from_millis(40),
                &partial,
            )
            .await
            .unwrap();
            assert!(chunk.is_some());
        }

        assert!(started.elapsed() > Duration::from_millis(40));
        assert!(partial.exists());
        let _ = std::fs::remove_dir_all(temp_root);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn upload_rejects_date_directory_symlink_without_writing_outside_root() {
        use std::os::unix::fs::symlink;

        let temp_root = temp_upload_probe_root("streamserver-upload-symlink-parent");
        let work_root = temp_root.join("work");
        let outside = temp_root.join("outside");
        let node_id = Uuid::now_v7();
        let date_parent = work_root
            .join("uploads")
            .join(node_id.to_string())
            .join("2026")
            .join("07");
        std::fs::create_dir_all(&date_parent).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, date_parent.join("12")).unwrap();
        let relative = PathBuf::from("uploads")
            .join(node_id.to_string())
            .join("2026/07/12/clip.mp4");

        let result =
            create_secure_upload_destination(&work_root, &relative, "clip.mp4.uploading-test")
                .await;

        assert!(result.is_err());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(temp_root);
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

    #[test]
    fn byte_range_parser_accepts_common_media_ranges() {
        assert_eq!(
            parse_single_byte_range(Some("bytes=2-4"), 10)
                .expect("range should parse")
                .expect("range should exist"),
            super::MediaByteRange { start: 2, end: 4 }
        );
        assert_eq!(
            parse_single_byte_range(Some("bytes=6-"), 10)
                .expect("range should parse")
                .expect("range should exist"),
            super::MediaByteRange { start: 6, end: 9 }
        );
        assert_eq!(
            parse_single_byte_range(Some("bytes=-3"), 10)
                .expect("range should parse")
                .expect("range should exist"),
            super::MediaByteRange { start: 7, end: 9 }
        );
        assert!(parse_single_byte_range(Some("bytes=10-20"), 10).is_err());
        assert!(parse_single_byte_range(Some("items=0-1"), 10).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn serve_media_file_honors_byte_range_requests() {
        let temp_root = temp_upload_probe_root("streamserver-upload-range");
        let work_root = temp_root.join("work");
        let relative = PathBuf::from("uploads/node-a/2026/04/29/demo.mp4");
        let target = work_root.join(&relative);
        std::fs::create_dir_all(target.parent().expect("target parent should exist"))
            .expect("upload dir should be created");
        std::fs::write(&target, b"abcdef").expect("media file should be written");
        let state = test_app_state(work_root);
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, HeaderValue::from_static("bytes=2-4"));

        let response = serve_media_file(
            State(state),
            headers,
            AxumPath(relative.to_string_lossy().to_string()),
        )
        .await
        .expect("range request should pass");

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            response.headers().get(header::CONTENT_RANGE),
            Some(&HeaderValue::from_static("bytes 2-4/6"))
        );
        assert_eq!(
            response.headers().get(header::CONTENT_LENGTH),
            Some(&HeaderValue::from_static("3"))
        );
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should read");
        assert_eq!(&body[..], b"cde");

        let _ = std::fs::remove_dir_all(&temp_root);
    }
}
