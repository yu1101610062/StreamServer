use std::{
    collections::{HashMap, VecDeque},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod prefetch;

const MAX_SOURCE_URL_BYTES: usize = 8192;
const MAX_TARGET_PATH_BYTES: usize = 1024;
const MAX_HLS_PLAYLIST_BYTES: usize = 4 * 1024 * 1024;
const MAX_HLS_RESOURCES_PER_RELAY: usize = 8192;

#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub public_base_url: String,
    pub work_root: PathBuf,
    pub ffmpeg_bin: PathBuf,
}

#[derive(Debug, Clone)]
pub struct GatewayRuntimeConfig {
    pub max_queued_prefetches: usize,
    pub max_active_downloads: usize,
    pub max_active_ffmpeg: usize,
    pub prefetch_queue_timeout: Duration,
    pub prefetch_execution_timeout: Duration,
    pub source_connect_timeout: Duration,
    pub source_read_idle_timeout: Duration,
    pub max_prefetch_records: usize,
    pub prefetch_terminal_retention: Duration,
    pub relay_cancel_wait: Duration,
    pub prefetch_cancel_wait: Duration,
    pub cancel_tombstone_ttl: Duration,
    pub max_active_relays: usize,
    pub max_relay_registrations: usize,
    pub relay_reconnect_grace: Duration,
    pub relay_unopened_ttl: Duration,
    pub ffprobe_bin: Option<PathBuf>,
}

impl Default for GatewayRuntimeConfig {
    fn default() -> Self {
        Self {
            max_queued_prefetches: 4096,
            max_active_downloads: 4,
            max_active_ffmpeg: 2,
            prefetch_queue_timeout: Duration::ZERO,
            prefetch_execution_timeout: Duration::from_secs(6 * 60 * 60),
            source_connect_timeout: Duration::from_secs(10),
            source_read_idle_timeout: Duration::from_secs(60),
            max_prefetch_records: 8192,
            prefetch_terminal_retention: Duration::from_secs(60 * 60),
            relay_cancel_wait: Duration::from_secs(5),
            prefetch_cancel_wait: Duration::from_secs(30),
            cancel_tombstone_ttl: Duration::from_secs(60 * 60),
            max_active_relays: 32,
            max_relay_registrations: 256,
            relay_reconnect_grace: Duration::from_secs(10 * 60),
            relay_unopened_ttl: Duration::from_secs(24 * 60 * 60),
            ffprobe_bin: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct GatewayState {
    config: GatewayConfig,
    runtime: GatewayRuntimeConfig,
    http: reqwest::Client,
    relays: Arc<Mutex<HashMap<Uuid, Arc<RelayEntry>>>>,
    relay_admission: Arc<Semaphore>,
    prefetches: Arc<Mutex<PrefetchRegistry>>,
    queue_notify: Arc<Notify>,
    workers_started: Arc<AtomicBool>,
    queue_high_water: Arc<AtomicUsize>,
    prefetch_status_queries: Arc<AtomicUsize>,
    started_at: Instant,
    tombstones: Arc<Mutex<HashMap<Uuid, Instant>>>,
}

#[derive(Debug)]
struct RelayEntry {
    task_id: Uuid,
    source_url: String,
    source_kind: Option<RelaySourceKind>,
    token: String,
    relay_url: String,
    cancellation: CancellationToken,
    active_requests: AtomicUsize,
    inactive: Notify,
    activity: StdMutex<RelayActivity>,
    hls_resources: Mutex<HlsResourceMap>,
}

#[derive(Debug)]
struct RelayActivity {
    last_activity_at: Instant,
    opened: bool,
}

#[derive(Debug, Default)]
struct HlsResourceMap {
    by_id: HashMap<Uuid, String>,
    by_url: HashMap<String, Uuid>,
    lru: VecDeque<Uuid>,
}

impl HlsResourceMap {
    fn id_for_url(&mut self, url: String) -> Uuid {
        if let Some(id) = self.by_url.get(&url).copied() {
            self.touch(id);
            return id;
        }
        while self.by_id.len() >= MAX_HLS_RESOURCES_PER_RELAY {
            let Some(id) = self.lru.pop_front() else {
                break;
            };
            if let Some(old_url) = self.by_id.remove(&id) {
                self.by_url.remove(&old_url);
            }
        }
        let id = Uuid::now_v7();
        self.by_id.insert(id, url.clone());
        self.by_url.insert(url, id);
        self.lru.push_back(id);
        id
    }

    fn get(&mut self, id: Uuid) -> Option<String> {
        let value = self.by_id.get(&id).cloned();
        if value.is_some() {
            self.touch(id);
        }
        value
    }

    fn touch(&mut self, id: Uuid) {
        if let Some(position) = self.lru.iter().position(|candidate| *candidate == id) {
            self.lru.remove(position);
        }
        self.lru.push_back(id);
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct PrefetchRequest {
    task_id: Uuid,
    source_url: String,
    target_path: String,
    #[serde(default)]
    source_kind: Option<prefetch::PrefetchSourceKind>,
    #[serde(default)]
    start_offset_sec: Option<u32>,
    #[serde(default)]
    duration_sec: Option<u32>,
}

#[derive(Debug)]
struct PrefetchEntry {
    request: PrefetchRequest,
    job: prefetch::PrefetchJob,
    class: prefetch::ExecutionClass,
    lifecycle: Mutex<PrefetchLifecycle>,
    cancellation: CancellationToken,
    finished: Notify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrefetchPhase {
    Queued,
    Running,
    Ready,
    Failed,
    Canceled,
}

impl PrefetchPhase {
    fn is_terminal(self) -> bool {
        matches!(self, Self::Ready | Self::Failed | Self::Canceled)
    }
}

#[derive(Debug)]
struct PrefetchLifecycle {
    phase: PrefetchPhase,
    created_at: Instant,
    source_url: Option<String>,
    failure_reason: Option<String>,
    time_slice_applied: bool,
}

#[derive(Debug, Default)]
struct PrefetchRegistry {
    records: HashMap<Uuid, Arc<PrefetchEntry>>,
    download_queue: VecDeque<Uuid>,
    ffmpeg_queue: VecDeque<Uuid>,
    completed: VecDeque<(Instant, Uuid)>,
}

#[derive(Debug, Clone, Serialize)]
struct PrefetchState {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    queue_position: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    poll_after_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_reason: Option<String>,
    time_slice_applied: bool,
}

#[derive(Debug, Deserialize)]
struct RelayRequest {
    task_id: Uuid,
    source_url: String,
    #[serde(default)]
    source_kind: Option<RelaySourceKind>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RelaySourceKind {
    HttpFlv,
    HttpTs,
    Hls,
}

#[derive(Debug, Deserialize)]
struct RelayQuery {
    token: Option<String>,
}

impl GatewayState {
    pub fn new(config: GatewayConfig) -> Self {
        Self::with_runtime_config(config, GatewayRuntimeConfig::default())
    }

    pub fn with_runtime_config(config: GatewayConfig, runtime: GatewayRuntimeConfig) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(runtime.source_connect_timeout)
            .build()
            .expect("media gateway HTTP client configuration must be valid");
        Self {
            config,
            relay_admission: Arc::new(Semaphore::new(runtime.max_active_relays)),
            runtime,
            http,
            relays: Arc::new(Mutex::new(HashMap::new())),
            prefetches: Arc::new(Mutex::new(PrefetchRegistry::default())),
            queue_notify: Arc::new(Notify::new()),
            workers_started: Arc::new(AtomicBool::new(false)),
            queue_high_water: Arc::new(AtomicUsize::new(0)),
            prefetch_status_queries: Arc::new(AtomicUsize::new(0)),
            started_at: Instant::now(),
            tombstones: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn start_workers(&self) {
        if self.workers_started.swap(true, Ordering::AcqRel) {
            return;
        }
        for _ in 0..self.runtime.max_active_downloads {
            let state = self.clone();
            tokio::spawn(async move {
                state.worker_loop(prefetch::ExecutionClass::Download).await;
            });
        }
        for _ in 0..self.runtime.max_active_ffmpeg {
            let state = self.clone();
            tokio::spawn(async move {
                state.worker_loop(prefetch::ExecutionClass::Ffmpeg).await;
            });
        }
    }

    async fn worker_loop(&self, class: prefetch::ExecutionClass) {
        loop {
            let entry = self.next_prefetch(class).await;
            let mut lifecycle = entry.lifecycle.lock().await;
            if lifecycle.phase != PrefetchPhase::Queued || entry.cancellation.is_cancelled() {
                if !lifecycle.phase.is_terminal() {
                    lifecycle.phase = PrefetchPhase::Canceled;
                    lifecycle.failure_reason = Some("prefetch canceled".to_string());
                    drop(lifecycle);
                    self.record_prefetch_completion(entry.request.task_id).await;
                    entry.finished.notify_waiters();
                }
                continue;
            }
            if !self.runtime.prefetch_queue_timeout.is_zero()
                && lifecycle.created_at.elapsed() > self.runtime.prefetch_queue_timeout
            {
                lifecycle.phase = PrefetchPhase::Failed;
                lifecycle.failure_reason = Some("prefetch queue timeout".to_string());
                drop(lifecycle);
                self.record_prefetch_completion(entry.request.task_id).await;
                entry.finished.notify_waiters();
                continue;
            }
            lifecycle.phase = PrefetchPhase::Running;
            drop(lifecycle);

            let execution = prefetch::execute_prefetch(
                self.http.clone(),
                &self.config.ffmpeg_bin,
                self.runtime.ffprobe_bin.as_deref(),
                entry.job.clone(),
                entry.cancellation.clone(),
            );
            tokio::pin!(execution);
            let mut execution_timed_out = false;
            let result = if self.runtime.prefetch_execution_timeout.is_zero() {
                execution.await
            } else {
                tokio::select! {
                    result = &mut execution => result,
                    _ = tokio::time::sleep(self.runtime.prefetch_execution_timeout) => {
                        execution_timed_out = true;
                        entry.cancellation.cancel();
                        execution.await
                    }
                }
            };

            let mut lifecycle = entry.lifecycle.lock().await;
            if execution_timed_out {
                lifecycle.phase = PrefetchPhase::Failed;
                lifecycle.failure_reason = Some("prefetch execution timeout".to_string());
            } else if entry.cancellation.is_cancelled() {
                lifecycle.phase = PrefetchPhase::Canceled;
                lifecycle.failure_reason = Some("prefetch canceled".to_string());
            } else {
                match result {
                    Ok(outcome) => {
                        lifecycle.phase = PrefetchPhase::Ready;
                        lifecycle.source_url = Some(entry.request.target_path.clone());
                        lifecycle.failure_reason = None;
                        lifecycle.time_slice_applied = outcome.time_slice_applied();
                    }
                    Err(error) => {
                        lifecycle.phase = PrefetchPhase::Failed;
                        lifecycle.failure_reason = Some(error.to_string());
                    }
                }
            }
            drop(lifecycle);
            self.record_prefetch_completion(entry.request.task_id).await;
            entry.finished.notify_waiters();
        }
    }

    async fn next_prefetch(&self, class: prefetch::ExecutionClass) -> Arc<PrefetchEntry> {
        loop {
            let notified = self.queue_notify.notified();
            let next = {
                let mut registry = self.prefetches.lock().await;
                let queue = match class {
                    prefetch::ExecutionClass::Download => &mut registry.download_queue,
                    prefetch::ExecutionClass::Ffmpeg => &mut registry.ffmpeg_queue,
                };
                queue
                    .pop_front()
                    .and_then(|task_id| registry.records.get(&task_id).cloned())
            };
            if let Some(entry) = next {
                return entry;
            }
            notified.await;
        }
    }

    async fn record_prefetch_completion(&self, task_id: Uuid) {
        self.prefetches
            .lock()
            .await
            .completed
            .push_back((Instant::now(), task_id));
    }

    async fn prune_tombstones(&self) {
        let ttl = self.runtime.cancel_tombstone_ttl;
        self.tombstones
            .lock()
            .await
            .retain(|_, created_at| created_at.elapsed() < ttl);
    }

    async fn add_tombstone(&self, task_id: Uuid) {
        self.prune_tombstones().await;
        self.tombstones.lock().await.insert(task_id, Instant::now());
    }

    async fn is_tombstoned(&self, task_id: Uuid) -> bool {
        self.prune_tombstones().await;
        self.tombstones.lock().await.contains_key(&task_id)
    }

    async fn prune_prefetch_records(&self, force_capacity: bool) {
        let mut registry = self.prefetches.lock().await;
        while let Some((completed_at, task_id)) = registry.completed.front().copied() {
            let expired = completed_at.elapsed() >= self.runtime.prefetch_terminal_retention;
            let over_capacity =
                force_capacity && registry.records.len() >= self.runtime.max_prefetch_records;
            if !expired && !over_capacity {
                break;
            }
            registry.completed.pop_front();
            registry.records.remove(&task_id);
        }
    }

    async fn prune_relays(&self) {
        let reconnect_grace = self.runtime.relay_reconnect_grace;
        let unopened_ttl = self.runtime.relay_unopened_ttl;
        self.relays.lock().await.retain(|_, entry| {
            if entry.active_requests.load(Ordering::Acquire) != 0 {
                return true;
            }
            let activity = entry.activity.lock().expect("relay activity lock poisoned");
            let ttl = if activity.opened {
                reconnect_grace
            } else {
                unopened_ttl
            };
            activity.last_activity_at.elapsed() < ttl
        });
    }
}

pub fn build_app(state: GatewayState) -> Router {
    state.start_workers();
    Router::new()
        .route("/api/healthz", get(healthz))
        .route("/api/readyz", get(readyz))
        .route("/api/status", get(gateway_status))
        .route("/api/relays", post(create_relay))
        .route("/api/relays/{task_id}", delete(delete_relay))
        .route("/relay/{task_id}", get(relay_stream))
        .route(
            "/relay/{task_id}/hls/{resource_id}",
            get(relay_hls_resource),
        )
        .route("/api/prefetch", post(create_prefetch))
        .route(
            "/api/prefetch/{task_id}",
            get(get_prefetch).delete(delete_prefetch),
        )
        .route("/api/tasks/{task_id}", delete(delete_task))
        .route("/api/tasks/{task_id}/reset", post(reset_task))
        .layer(DefaultBodyLimit::max(64 * 1024))
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

async fn readyz(State(state): State<GatewayState>) -> impl IntoResponse {
    if state.workers_started.load(Ordering::Acquire) {
        (StatusCode::OK, Json(json!({"status": "ready"})))
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "not_ready"})),
        )
    }
}

async fn gateway_status(State(state): State<GatewayState>) -> impl IntoResponse {
    state.prune_prefetch_records(false).await;
    state.prune_relays().await;
    let registry = state.prefetches.lock().await;
    let entries: Vec<_> = registry.records.values().cloned().collect();
    let queued_downloads = registry.download_queue.len();
    let queued_ffmpeg = registry.ffmpeg_queue.len();
    drop(registry);
    let mut queued = 0usize;
    let mut running = 0usize;
    let mut ready = 0usize;
    let mut failed = 0usize;
    let mut canceled = 0usize;
    let mut active_downloads = 0usize;
    let mut active_ffmpeg = 0usize;
    for entry in &entries {
        match entry.lifecycle.lock().await.phase {
            PrefetchPhase::Queued => queued += 1,
            PrefetchPhase::Running => {
                running += 1;
                match entry.class {
                    prefetch::ExecutionClass::Download => active_downloads += 1,
                    prefetch::ExecutionClass::Ffmpeg => active_ffmpeg += 1,
                }
            }
            PrefetchPhase::Ready => ready += 1,
            PrefetchPhase::Failed => failed += 1,
            PrefetchPhase::Canceled => canceled += 1,
        }
    }
    let relays: Vec<_> = state.relays.lock().await.values().cloned().collect();
    let active_relay_requests = relays
        .iter()
        .map(|entry| entry.active_requests.load(Ordering::Acquire))
        .sum::<usize>();
    Json(json!({
        "prefetch": {
            "records": entries.len(),
            "queued": queued,
            "running": running,
            "ready": ready,
            "failed": failed,
            "canceled": canceled,
            "queued_downloads": queued_downloads,
            "queued_ffmpeg": queued_ffmpeg,
            "active_downloads": active_downloads,
            "active_ffmpeg": active_ffmpeg,
            "queue_high_water": state.queue_high_water.load(Ordering::Acquire),
            "status_queries": state.prefetch_status_queries.load(Ordering::Acquire)
        },
        "relay": {
            "registrations": relays.len(),
            "active_requests": active_relay_requests
        },
        "uptime_ms": state.started_at.elapsed().as_millis()
    }))
}

async fn create_relay(
    State(state): State<GatewayState>,
    Json(request): Json<RelayRequest>,
) -> Response {
    if !valid_source_url(&request.source_url) {
        return bad_request("source_url must be an HTTP URL no longer than 8192 bytes");
    }
    if state.is_tombstoned(request.task_id).await {
        return conflict("task is canceled; reset it before creating a relay");
    }
    state.prune_relays().await;
    let mut relays = state.relays.lock().await;
    if let Some(existing) = relays.get(&request.task_id) {
        if existing.source_url == request.source_url || existing.relay_url == request.source_url {
            return Json(json!({"relay_url": existing.relay_url})).into_response();
        }
        return conflict("task already has a relay for another source");
    }
    if is_gateway_relay_url(&state.config.public_base_url, &request.source_url) {
        return conflict("gateway relay URL cannot be used as an upstream source");
    }
    if relays.len() >= state.runtime.max_relay_registrations {
        return service_unavailable("relay registration capacity is full", Some(30));
    }
    let token = Uuid::now_v7().to_string();
    let base = state.config.public_base_url.trim_end_matches('/');
    let relay_url = format!("{base}/relay/{}?token={token}", request.task_id);
    let now = Instant::now();
    relays.insert(
        request.task_id,
        Arc::new(RelayEntry {
            task_id: request.task_id,
            source_url: request.source_url,
            source_kind: request.source_kind,
            token,
            relay_url: relay_url.clone(),
            cancellation: CancellationToken::new(),
            active_requests: AtomicUsize::new(0),
            inactive: Notify::new(),
            activity: StdMutex::new(RelayActivity {
                last_activity_at: now,
                opened: false,
            }),
            hls_resources: Mutex::new(HlsResourceMap::default()),
        }),
    );
    Json(json!({"relay_url": relay_url})).into_response()
}

async fn delete_relay(
    State(state): State<GatewayState>,
    AxumPath(task_id): AxumPath<Uuid>,
) -> Response {
    state.add_tombstone(task_id).await;
    match cancel_relay(&state, task_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(()) => service_unavailable("relay is still stopping", Some(1)),
    }
}

async fn relay_stream(
    State(state): State<GatewayState>,
    AxumPath(task_id): AxumPath<Uuid>,
    Query(query): Query<RelayQuery>,
    headers: HeaderMap,
) -> Response {
    let Some(entry) = authenticated_relay(&state, task_id, &query).await else {
        return relay_auth_failure(&state, task_id).await;
    };
    let source_url = entry.source_url.clone();
    let force_playlist = entry.source_kind == Some(RelaySourceKind::Hls);
    relay_upstream(state, entry, source_url, headers, force_playlist).await
}

async fn relay_hls_resource(
    State(state): State<GatewayState>,
    AxumPath((task_id, resource_id)): AxumPath<(Uuid, Uuid)>,
    Query(query): Query<RelayQuery>,
    headers: HeaderMap,
) -> Response {
    let Some(entry) = authenticated_relay(&state, task_id, &query).await else {
        return relay_auth_failure(&state, task_id).await;
    };
    let Some(source_url) = entry.hls_resources.lock().await.get(resource_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    relay_upstream(state, entry, source_url, headers, false).await
}

async fn authenticated_relay(
    state: &GatewayState,
    task_id: Uuid,
    query: &RelayQuery,
) -> Option<Arc<RelayEntry>> {
    let entry = state.relays.lock().await.get(&task_id).cloned()?;
    if entry.cancellation.is_cancelled() || query.token.as_deref() != Some(entry.token.as_str()) {
        return None;
    }
    Some(entry)
}

async fn relay_auth_failure(state: &GatewayState, task_id: Uuid) -> Response {
    let entry = state.relays.lock().await.get(&task_id).cloned();
    match entry {
        Some(entry) if !entry.cancellation.is_cancelled() => {
            StatusCode::UNAUTHORIZED.into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn relay_upstream(
    state: GatewayState,
    entry: Arc<RelayEntry>,
    source_url: String,
    request_headers: HeaderMap,
    force_playlist: bool,
) -> Response {
    let permit = match state.relay_admission.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return service_unavailable("active relay capacity is full", Some(1)),
    };
    let guard = RelayActivityGuard::new(entry.clone(), permit);
    let mut request = state.http.get(&source_url);
    if let Some(range) = request_headers.get(header::RANGE) {
        request = request.header(header::RANGE, range);
    }
    let upstream = tokio::select! {
        _ = entry.cancellation.cancelled() => return StatusCode::NOT_FOUND.into_response(),
        response = request.send() => match response {
            Ok(response) => response,
            Err(error) => return (StatusCode::BAD_GATEWAY, format!("failed to connect upstream: {error}")).into_response(),
        }
    };
    if !upstream.status().is_success() {
        return (
            upstream.status(),
            format!("upstream returned {}", upstream.status()),
        )
            .into_response();
    }

    let content_type = upstream.headers().get(header::CONTENT_TYPE).cloned();
    let is_playlist = force_playlist
        || content_type
            .as_ref()
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("mpegurl") || value.contains("vnd.apple.mpegurl"))
        || reqwest::Url::parse(&source_url)
            .ok()
            .is_some_and(|url| url.path().to_ascii_lowercase().ends_with(".m3u8"));
    if is_playlist {
        return relay_playlist(&state, entry, source_url, upstream, guard).await;
    }
    relay_body(entry, upstream, content_type, guard)
}

async fn relay_playlist(
    state: &GatewayState,
    entry: Arc<RelayEntry>,
    source_url: String,
    upstream: reqwest::Response,
    _guard: RelayActivityGuard,
) -> Response {
    if upstream
        .content_length()
        .is_some_and(|length| length > MAX_HLS_PLAYLIST_BYTES as u64)
    {
        return (StatusCode::BAD_GATEWAY, "HLS playlist is too large").into_response();
    }
    let bytes = tokio::select! {
        _ = entry.cancellation.cancelled() => return StatusCode::NOT_FOUND.into_response(),
        bytes = upstream.bytes() => match bytes {
            Ok(bytes) => bytes,
            Err(error) => return (StatusCode::BAD_GATEWAY, format!("failed to read HLS playlist: {error}")).into_response(),
        }
    };
    if bytes.len() > MAX_HLS_PLAYLIST_BYTES {
        return (StatusCode::BAD_GATEWAY, "HLS playlist is too large").into_response();
    }
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return (StatusCode::BAD_GATEWAY, "HLS playlist is not UTF-8").into_response();
    };
    match rewrite_hls_playlist(state, &entry, &source_url, text).await {
        Ok(rewritten) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")],
            rewritten,
        )
            .into_response(),
        Err(error) => (StatusCode::BAD_GATEWAY, error).into_response(),
    }
}

fn relay_body(
    entry: Arc<RelayEntry>,
    upstream: reqwest::Response,
    content_type: Option<axum::http::HeaderValue>,
    guard: RelayActivityGuard,
) -> Response {
    let status = upstream.status();
    let content_range = upstream.headers().get(header::CONTENT_RANGE).cloned();
    let accept_ranges = upstream.headers().get(header::ACCEPT_RANGES).cloned();
    let content_length = upstream.headers().get(header::CONTENT_LENGTH).cloned();
    let cancellation = entry.cancellation.clone();
    let stream = upstream.bytes_stream();
    let body_stream = async_stream::stream! {
        let _guard = guard;
        tokio::pin!(stream);
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => break,
                chunk = stream.next() => match chunk {
                    Some(chunk) => yield chunk,
                    None => break,
                }
            }
        }
    };
    let mut response = Response::builder().status(status);
    for (name, value) in [
        (header::CONTENT_TYPE, content_type),
        (header::CONTENT_RANGE, content_range),
        (header::ACCEPT_RANGES, accept_ranges),
        (header::CONTENT_LENGTH, content_length),
    ] {
        if let Some(value) = value {
            response = response.header(name, value);
        }
    }
    response
        .body(Body::from_stream(body_stream))
        .unwrap_or_else(|error| {
            (
                StatusCode::BAD_GATEWAY,
                format!("failed to build relay response: {error}"),
            )
                .into_response()
        })
}

async fn rewrite_hls_playlist(
    state: &GatewayState,
    entry: &RelayEntry,
    playlist_url: &str,
    playlist: &str,
) -> Result<String, String> {
    let base_url = reqwest::Url::parse(playlist_url)
        .map_err(|error| format!("invalid HLS playlist URL: {error}"))?;
    let mut resources = entry.hls_resources.lock().await;
    let mut output = String::with_capacity(playlist.len() + 256);
    for line in playlist.lines() {
        let rewritten = if line.starts_with('#') {
            rewrite_hls_uri_attributes(state, entry, &base_url, line, &mut resources)?
        } else if line.trim().is_empty() {
            line.to_string()
        } else {
            gateway_hls_url(
                state,
                entry,
                resolve_hls_url(&base_url, line.trim())?,
                &mut resources,
            )
        };
        output.push_str(&rewritten);
        output.push('\n');
    }
    Ok(output)
}

fn rewrite_hls_uri_attributes(
    state: &GatewayState,
    entry: &RelayEntry,
    base_url: &reqwest::Url,
    line: &str,
    resources: &mut HlsResourceMap,
) -> Result<String, String> {
    let mut output = line.to_string();
    let mut search_from = 0usize;
    while let Some(relative_start) = output[search_from..].find("URI=\"") {
        let value_start = search_from + relative_start + 5;
        let Some(relative_end) = output[value_start..].find('"') else {
            return Err("malformed HLS URI attribute".to_string());
        };
        let value_end = value_start + relative_end;
        let original = output[value_start..value_end].to_string();
        let rewritten = gateway_hls_url(
            state,
            entry,
            resolve_hls_url(base_url, &original)?,
            resources,
        );
        output.replace_range(value_start..value_end, &rewritten);
        search_from = value_start + rewritten.len() + 1;
    }
    Ok(output)
}

fn resolve_hls_url(base_url: &reqwest::Url, value: &str) -> Result<String, String> {
    let resolved = base_url
        .join(value)
        .map_err(|error| format!("invalid HLS resource URL: {error}"))?;
    if !matches!(resolved.scheme(), "http" | "https") {
        return Err("HLS resource URL must use HTTP".to_string());
    }
    Ok(resolved.to_string())
}

fn gateway_hls_url(
    state: &GatewayState,
    entry: &RelayEntry,
    upstream_url: String,
    resources: &mut HlsResourceMap,
) -> String {
    let id = resources.id_for_url(upstream_url);
    let base = state.config.public_base_url.trim_end_matches('/');
    format!(
        "{base}/relay/{}/hls/{id}?token={}",
        entry.task_id, entry.token
    )
}

fn is_gateway_relay_url(public_base_url: &str, source_url: &str) -> bool {
    let (Ok(base), Ok(source)) = (
        reqwest::Url::parse(public_base_url),
        reqwest::Url::parse(source_url),
    ) else {
        return false;
    };
    if base.origin() != source.origin() {
        return false;
    }
    let relay_path = format!("{}/relay/", base.path().trim_end_matches('/'));
    source.path().starts_with(&relay_path)
}

struct RelayActivityGuard {
    entry: Arc<RelayEntry>,
    _permit: OwnedSemaphorePermit,
}

impl RelayActivityGuard {
    fn new(entry: Arc<RelayEntry>, permit: OwnedSemaphorePermit) -> Self {
        entry.active_requests.fetch_add(1, Ordering::AcqRel);
        let mut activity = entry.activity.lock().expect("relay activity lock poisoned");
        activity.opened = true;
        activity.last_activity_at = Instant::now();
        drop(activity);
        Self {
            entry,
            _permit: permit,
        }
    }
}

impl Drop for RelayActivityGuard {
    fn drop(&mut self) {
        if let Ok(mut activity) = self.entry.activity.lock() {
            activity.last_activity_at = Instant::now();
        }
        if self.entry.active_requests.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.entry.inactive.notify_waiters();
        }
    }
}

async fn create_prefetch(
    State(state): State<GatewayState>,
    Json(mut request): Json<PrefetchRequest>,
) -> Response {
    if !valid_source_url(&request.source_url) {
        return bad_request("source_url must be an HTTP URL no longer than 8192 bytes");
    }
    if request.target_path.len() > MAX_TARGET_PATH_BYTES {
        return bad_request("target_path must not exceed 1024 bytes");
    }
    let Ok(final_path) = safe_target_path(&state.config.work_root, &request.target_path) else {
        return bad_request("target_path must be a non-upload relative path");
    };
    request.start_offset_sec = request.start_offset_sec.filter(|value| *value > 0);
    if request.duration_sec == Some(0) {
        return bad_request("duration_sec must be greater than 0");
    }
    if (request.start_offset_sec.is_some() || request.duration_sec.is_some())
        && request.source_kind.is_none()
    {
        return bad_request("source_kind is required for time-slice prefetch");
    }
    if state.is_tombstoned(request.task_id).await {
        return conflict("task is canceled; reset it before prefetching");
    }
    state.prune_prefetch_records(true).await;

    let mut registry = state.prefetches.lock().await;
    if let Some(existing) = registry.records.get(&request.task_id).cloned() {
        if existing.request != request {
            return conflict("task already has a different prefetch request");
        }
        drop(registry);
        return Json(prefetch_response(&state, &existing).await).into_response();
    }
    if registry.records.len() >= state.runtime.max_prefetch_records {
        return service_unavailable("prefetch record capacity is full", Some(30));
    }
    let queued = registry.download_queue.len() + registry.ffmpeg_queue.len();
    if queued >= state.runtime.max_queued_prefetches {
        return service_unavailable("prefetch queue is full", Some(30));
    }
    let job = prefetch::PrefetchJob {
        source_url: request.source_url.clone(),
        final_path,
        source_kind: request.source_kind,
        start_offset_sec: request.start_offset_sec,
        duration_sec: request.duration_sec,
        read_idle_timeout: state.runtime.source_read_idle_timeout,
    };
    let class = job.execution_class();
    let entry = Arc::new(PrefetchEntry {
        request: request.clone(),
        job,
        class,
        lifecycle: Mutex::new(PrefetchLifecycle {
            phase: PrefetchPhase::Queued,
            created_at: Instant::now(),
            source_url: None,
            failure_reason: None,
            time_slice_applied: false,
        }),
        cancellation: CancellationToken::new(),
        finished: Notify::new(),
    });
    registry.records.insert(request.task_id, entry.clone());
    match class {
        prefetch::ExecutionClass::Download => registry.download_queue.push_back(request.task_id),
        prefetch::ExecutionClass::Ffmpeg => registry.ffmpeg_queue.push_back(request.task_id),
    }
    let queue_depth = registry.download_queue.len() + registry.ffmpeg_queue.len();
    state
        .queue_high_water
        .fetch_max(queue_depth, Ordering::AcqRel);
    drop(registry);
    state.queue_notify.notify_waiters();
    let response = prefetch_response(&state, &entry).await;
    (StatusCode::ACCEPTED, Json(response)).into_response()
}

async fn get_prefetch(
    State(state): State<GatewayState>,
    AxumPath(task_id): AxumPath<Uuid>,
) -> Response {
    state
        .prefetch_status_queries
        .fetch_add(1, Ordering::Relaxed);
    state.prune_prefetch_records(false).await;
    let entry = state.prefetches.lock().await.records.get(&task_id).cloned();
    match entry {
        Some(entry) => Json(prefetch_response(&state, &entry).await).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn prefetch_response(state: &GatewayState, entry: &PrefetchEntry) -> PrefetchState {
    let lifecycle = entry.lifecycle.lock().await;
    let (status, phase, poll_after_ms) = match lifecycle.phase {
        PrefetchPhase::Queued => ("pending", Some("queued"), Some(30_000)),
        PrefetchPhase::Running => ("pending", Some("running"), Some(5_000)),
        PrefetchPhase::Ready => ("ready", Some("ready"), None),
        PrefetchPhase::Failed => ("failed", Some("failed"), None),
        PrefetchPhase::Canceled => ("canceled", Some("canceled"), None),
    };
    let queue_position = if lifecycle.phase == PrefetchPhase::Queued {
        let registry = state.prefetches.lock().await;
        let queue = match entry.class {
            prefetch::ExecutionClass::Download => &registry.download_queue,
            prefetch::ExecutionClass::Ffmpeg => &registry.ffmpeg_queue,
        };
        queue
            .iter()
            .position(|task_id| *task_id == entry.request.task_id)
            .map(|position| position + 1)
    } else {
        None
    };
    PrefetchState {
        status: status.to_string(),
        phase: phase.map(str::to_string),
        queue_position,
        poll_after_ms,
        source_url: lifecycle.source_url.clone(),
        failure_reason: lifecycle.failure_reason.clone(),
        time_slice_applied: lifecycle.time_slice_applied,
    }
}

async fn delete_prefetch(
    State(state): State<GatewayState>,
    AxumPath(task_id): AxumPath<Uuid>,
) -> Response {
    state.add_tombstone(task_id).await;
    match cancel_prefetch(&state, task_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(()) => service_unavailable("prefetch is still stopping", Some(1)),
    }
}

async fn delete_task(
    State(state): State<GatewayState>,
    AxumPath(task_id): AxumPath<Uuid>,
) -> Response {
    state.add_tombstone(task_id).await;
    let relay_result = cancel_relay(&state, task_id).await;
    let prefetch_result = cancel_prefetch(&state, task_id).await;
    if relay_result.is_ok() && prefetch_result.is_ok() {
        StatusCode::NO_CONTENT.into_response()
    } else {
        service_unavailable("gateway task is still stopping", Some(1))
    }
}

async fn reset_task(
    State(state): State<GatewayState>,
    AxumPath(task_id): AxumPath<Uuid>,
) -> Response {
    let relay = state.relays.lock().await.get(&task_id).cloned();
    if relay.is_some_and(|relay| relay.active_requests.load(Ordering::Acquire) != 0) {
        return conflict("relay still has active requests");
    }
    let entry = state.prefetches.lock().await.records.get(&task_id).cloned();
    if let Some(entry) = entry {
        if !entry.lifecycle.lock().await.phase.is_terminal() {
            return conflict("prefetch is still active");
        }
        let mut registry = state.prefetches.lock().await;
        registry.records.remove(&task_id);
        registry
            .completed
            .retain(|(_, completed_task_id)| *completed_task_id != task_id);
    }
    state.relays.lock().await.remove(&task_id);
    state.tombstones.lock().await.remove(&task_id);
    StatusCode::NO_CONTENT.into_response()
}

async fn cancel_relay(state: &GatewayState, task_id: Uuid) -> Result<(), ()> {
    let entry = state.relays.lock().await.get(&task_id).cloned();
    let Some(entry) = entry else {
        return Ok(());
    };
    entry.cancellation.cancel();
    let deadline = Instant::now() + state.runtime.relay_cancel_wait;
    loop {
        let notified = entry.inactive.notified();
        if entry.active_requests.load(Ordering::Acquire) == 0 {
            let mut relays = state.relays.lock().await;
            if relays
                .get(&task_id)
                .is_some_and(|candidate| Arc::ptr_eq(candidate, &entry))
            {
                relays.remove(&task_id);
            }
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || tokio::time::timeout(remaining, notified).await.is_err() {
            return Err(());
        }
    }
}

async fn cancel_prefetch(state: &GatewayState, task_id: Uuid) -> Result<(), ()> {
    let entry = {
        let mut registry = state.prefetches.lock().await;
        registry
            .download_queue
            .retain(|candidate| *candidate != task_id);
        registry
            .ffmpeg_queue
            .retain(|candidate| *candidate != task_id);
        registry.records.get(&task_id).cloned()
    };
    let Some(entry) = entry else {
        return Ok(());
    };
    entry.cancellation.cancel();
    {
        let mut lifecycle = entry.lifecycle.lock().await;
        if lifecycle.phase == PrefetchPhase::Queued {
            lifecycle.phase = PrefetchPhase::Canceled;
            lifecycle.failure_reason = Some("prefetch canceled".to_string());
            drop(lifecycle);
            state.record_prefetch_completion(task_id).await;
            entry.finished.notify_waiters();
            return Ok(());
        }
        if lifecycle.phase.is_terminal() {
            return Ok(());
        }
    }
    let deadline = Instant::now() + state.runtime.prefetch_cancel_wait;
    loop {
        let notified = entry.finished.notified();
        if entry.lifecycle.lock().await.phase.is_terminal() {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || tokio::time::timeout(remaining, notified).await.is_err() {
            return Err(());
        }
    }
}

fn valid_source_url(value: &str) -> bool {
    value.len() <= MAX_SOURCE_URL_BYTES
        && reqwest::Url::parse(value)
            .ok()
            .is_some_and(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some())
}

fn bad_request(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error": message}))).into_response()
}

fn conflict(message: &str) -> Response {
    (StatusCode::CONFLICT, Json(json!({"error": message}))).into_response()
}

fn service_unavailable(message: &str, retry_after: Option<u64>) -> Response {
    let mut response = (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": message})),
    )
        .into_response();
    if let Some(seconds) = retry_after {
        if let Ok(value) = seconds.to_string().parse() {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
    }
    response
}
