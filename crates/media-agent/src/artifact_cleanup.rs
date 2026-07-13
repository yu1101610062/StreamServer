#[cfg(test)]
#[path = "tests/artifact_cleanup.rs"]
mod tests;

use std::{
    collections::{HashMap, HashSet},
    ffi::CString,
    fs,
    io::{self, Read},
    net::IpAddr,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::{Duration, SystemTime},
};

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use media_domain::{
    PublishTargetKind, RecordFormat, RuntimeHandle, RuntimeState, TaskSpec, TaskType,
};
use serde_json::Value;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{info, warn};
use uuid::Uuid;

#[cfg(test)]
use crate::runtime_registry::LocalRuntimeRegistry;
use crate::{
    config::{AgentArtifactCleanupSettings, AgentSettings},
    runtime::{RuntimeManagerHandle, RuntimeReadModel, StopTaskRequest},
};

const CLEANUP_HYSTERESIS_PERCENT: f64 = 5.0;
const CLEANUP_RECENT_WRITE_GRACE_PERIOD: Duration = Duration::from_secs(60);
const RUNNING_SEGMENT_RETAIN_COUNT: usize = 2;
const DISK_THRESHOLD_STOP_REASON: &str = "disk_threshold_exceeded";

// 产物清理按输出桶分别管理，避免 MP4 与 HLS 共享目录时误删对方的任务产物。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArtifactBucket {
    Mp4,
    Hls,
}

impl ArtifactBucket {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Hls => "hls",
        }
    }

    fn root(self, settings: &AgentSettings) -> &str {
        match self {
            Self::Mp4 => settings.zlm_output_mp4_root.as_str(),
            Self::Hls => settings.zlm_output_hls_root.as_str(),
        }
    }
}

#[derive(Clone)]
pub struct ArtifactCleanupManager {
    inner: Arc<ArtifactCleanupManagerInner>,
}

struct ArtifactCleanupManagerInner {
    cleanup: AgentArtifactCleanupSettings,
    runtime_read_model: Arc<dyn RuntimeReadModel>,
    executor: Option<RuntimeManagerHandle>,
    layout: ArtifactCleanupLayout,
    state: RwLock<ArtifactCleanupState>,
}

impl std::fmt::Debug for ArtifactCleanupManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ArtifactCleanupManager")
            .field("cleanup", &self.inner.cleanup)
            .field("layout", &self.inner.layout)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
struct ArtifactCleanupLayout {
    buckets: HashMap<ArtifactBucket, ArtifactBucketLayout>,
}

#[derive(Debug, Clone)]
struct ArtifactBucketLayout {
    root: PathBuf,
    node_dir: PathBuf,
}

#[derive(Debug, Clone, Default)]
struct ArtifactCleanupState {
    // 每个 bucket 单独记录是否允许新任务启动；Core 心跳会读取这个阻塞状态参与调度。
    buckets: HashMap<ArtifactBucket, ArtifactBucketState>,
    max_disk_percent: Option<f64>,
    last_refresh_at: Option<DateTime<Utc>>,
}

#[derive(Debug)]
struct ArtifactCleanupRefresh {
    state: ArtifactCleanupState,
    stop_requests: Vec<ArtifactCleanupStopRequest>,
}

#[derive(Debug, Clone)]
struct ArtifactCleanupStopRequest {
    request: StopTaskRequest,
    reason: String,
}

#[derive(Debug, Clone)]
struct ArtifactBucketState {
    disk_percent: Option<f64>,
    start_allowed: bool,
    reason: String,
}

#[derive(Debug, Clone)]
struct BucketObservation {
    bucket: ArtifactBucket,
    root: PathBuf,
    node_dir: PathBuf,
    device_id: u64,
    disk_percent: f64,
}

#[derive(Debug, Clone)]
struct CleanupCandidate {
    // 非活跃任务按最后写入时间排序，优先清理最旧的完整任务目录。
    task_id: Uuid,
    last_write: SystemTime,
    paths: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
struct RunningCleanupCandidate {
    // 运行中 HLS 只允许清理旧 segment，必须保留最近片段给播放器和写入中的 ffmpeg 使用。
    task_id: Uuid,
    bucket: ArtifactBucket,
    path: PathBuf,
    last_write: SystemTime,
    size_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
struct DiskSample {
    percent_used: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArtifactCleanupStrategy {
    DeleteOldestThenReject,
    RejectOnly,
}

impl ArtifactCleanupStrategy {
    fn from_settings(settings: &AgentArtifactCleanupSettings) -> Self {
        // 默认策略会先尝试释放空间再拒绝任务；reject_only 用于只做保护、不自动删文件的场景。
        match settings.strategy.trim() {
            "reject_only" => Self::RejectOnly,
            _ => Self::DeleteOldestThenReject,
        }
    }
}

impl ArtifactCleanupState {
    fn unknown(layout: &ArtifactCleanupLayout) -> Self {
        let mut buckets = HashMap::new();
        for bucket in layout.buckets.keys().copied() {
            buckets.insert(
                bucket,
                ArtifactBucketState::blocked(
                    None,
                    "initial artifact volume sample pending".to_string(),
                ),
            );
        }
        Self {
            buckets,
            max_disk_percent: None,
            last_refresh_at: None,
        }
    }
}

impl ArtifactBucketState {
    fn allowed(disk_percent: Option<f64>) -> Self {
        Self {
            disk_percent,
            start_allowed: true,
            reason: String::new(),
        }
    }

    fn blocked(disk_percent: Option<f64>, reason: String) -> Self {
        Self {
            disk_percent,
            start_allowed: false,
            reason,
        }
    }
}

impl ArtifactCleanupLayout {
    fn from_settings(settings: &AgentSettings) -> Self {
        // node_dir 带节点维度，防止多个 worker 共享网络盘时互相清理对方产物。
        let mut buckets = HashMap::new();
        for bucket in [ArtifactBucket::Mp4, ArtifactBucket::Hls] {
            let root = PathBuf::from(bucket.root(settings));
            let node_dir = root.join(artifact_node_dir_name(settings, bucket));
            buckets.insert(bucket, ArtifactBucketLayout { root, node_dir });
        }
        Self { buckets }
    }

    fn bucket_for_path(&self, path: &Path) -> Option<ArtifactBucket> {
        self.buckets
            .iter()
            .find_map(|(bucket, layout)| path.starts_with(&layout.root).then_some(*bucket))
    }
}

impl ArtifactCleanupManager {
    #[cfg(test)]
    pub fn new(settings: &AgentSettings, registry: LocalRuntimeRegistry) -> Self {
        Self::with_executor(settings, Arc::new(registry), None)
    }

    pub fn with_executor(
        settings: &AgentSettings,
        runtime_read_model: Arc<dyn RuntimeReadModel>,
        executor: Option<RuntimeManagerHandle>,
    ) -> Self {
        let layout = ArtifactCleanupLayout::from_settings(settings);
        let state = ArtifactCleanupState::unknown(&layout);
        Self {
            inner: Arc::new(ArtifactCleanupManagerInner {
                cleanup: settings.artifact_cleanup.clone(),
                runtime_read_model,
                executor,
                layout,
                state: RwLock::new(state),
            }),
        }
    }

    pub async fn refresh_now(&self) {
        let this = self.clone();
        match tokio::task::spawn_blocking(move || this.refresh_blocking()).await {
            Ok(refresh) => {
                self.apply_state(refresh.state);
                self.stop_active_artifact_tasks(refresh.stop_requests).await;
            }
            Err(error) => warn!(error = %error, "artifact cleanup refresh task panicked"),
        }
    }

    pub fn start_background(&self) {
        let this = self.clone();
        tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(
                this.inner.cleanup.check_interval_sec.max(1),
            ));
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                this.refresh_now().await;
            }
        });
    }

    pub fn current_disk_percent(&self) -> Option<f64> {
        self.inner
            .state
            .read()
            .expect("artifact cleanup state lock poisoned")
            .max_disk_percent
    }

    pub fn control_plane_block_reason(&self) -> Option<String> {
        if !self.inner.cleanup.enabled {
            return None;
        }

        self.inner
            .state
            .read()
            .expect("artifact cleanup state lock poisoned")
            .buckets
            .values()
            .find(|bucket_state| !bucket_state.start_allowed)
            .map(|bucket_state| bucket_state.reason.clone())
    }

    pub fn ensure_task_start_allowed(&self, resolved_spec: &Value) -> anyhow::Result<()> {
        let spec = serde_json::from_value::<TaskSpec>(resolved_spec.clone())
            .context("failed to decode resolved_spec for artifact cleanup")?;
        for bucket in artifact_buckets_for_task_spec(&spec) {
            let bucket_state = self
                .inner
                .state
                .read()
                .expect("artifact cleanup state lock poisoned")
                .buckets
                .get(&bucket)
                .cloned()
                .unwrap_or_else(|| {
                    ArtifactBucketState::blocked(
                        None,
                        format!("artifact bucket {} state is unavailable", bucket.as_str()),
                    )
                });
            if !bucket_state.start_allowed {
                bail!(
                    "artifact bucket {} is not ready: {}",
                    bucket.as_str(),
                    bucket_state.reason
                );
            }
        }
        Ok(())
    }

    fn refresh_blocking(&self) -> ArtifactCleanupRefresh {
        let mut next_state = ArtifactCleanupState::default();
        let mut stop_requests = Vec::new();
        let mut observed = Vec::new();

        for bucket in [ArtifactBucket::Mp4, ArtifactBucket::Hls] {
            let Some(layout) = self.inner.layout.buckets.get(&bucket) else {
                continue;
            };
            match inspect_bucket_root(bucket, layout) {
                Ok(observation) => {
                    next_state.max_disk_percent =
                        max_disk_percent(next_state.max_disk_percent, observation.disk_percent);
                    observed.push(observation);
                }
                Err(error) => {
                    next_state.buckets.insert(
                        bucket,
                        ArtifactBucketState::blocked(
                            None,
                            format!(
                                "artifact root {} is unavailable: {error}",
                                layout.root.display()
                            ),
                        ),
                    );
                }
            }
        }

        let active_handles = self.inner.runtime_read_model.active_handles();
        let strategy = ArtifactCleanupStrategy::from_settings(&self.inner.cleanup);
        let threshold = self.inner.cleanup.threshold_percent;
        let mut by_device: HashMap<u64, Vec<BucketObservation>> = HashMap::new();
        // MP4 与 HLS bucket 可能共用同一块磁盘；按 device 聚合后统一判断阈值，
        // 避免一个 bucket 清理后另一个 bucket 继续使用过期采样拒绝任务。
        for observation in observed {
            by_device
                .entry(observation.device_id)
                .or_default()
                .push(observation);
        }

        for observations in by_device.into_values() {
            let mut disk_percent = observations
                .first()
                .map(|observation| observation.disk_percent)
                .unwrap_or_default();
            let mut blocked_reason = None;

            if self.inner.cleanup.enabled {
                match strategy {
                    ArtifactCleanupStrategy::RejectOnly => {
                        // reject_only 不自动删除文件，只在超过阈值时停止相关活跃任务
                        // 并阻止新任务继续写入该卷。
                        if disk_percent >= threshold {
                            let reason = format!(
                                "artifact volume usage {:.1}% exceeds threshold {:.1}%",
                                disk_percent, threshold
                            );
                            stop_requests.extend(
                                self.stop_requests_for_active_artifact_tasks_for_volume(
                                    &observations,
                                    &active_handles,
                                    &reason,
                                ),
                            );
                            blocked_reason = Some(reason);
                        }
                    }
                    ArtifactCleanupStrategy::DeleteOldestThenReject => {
                        // 默认策略先尝试删除最旧产物，清理后仍超过阈值才进入阻塞。
                        if disk_percent >= threshold {
                            let cleanup_result = self.cleanup_volume_by_disk(
                                &observations,
                                &active_handles,
                                threshold,
                            );
                            disk_percent = cleanup_result.final_percent;
                            if !cleanup_result.deleted_task_ids.is_empty() {
                                let deleted_task_ids = &cleanup_result.deleted_task_ids;
                                info!(
                                    bucket_count = observations.len(),
                                    deleted_tasks = ?deleted_task_ids,
                                    disk_percent = cleanup_result.final_percent,
                                    "artifact cleanup deleted old task directories"
                                );
                            }
                            if !cleanup_result.deleted_running_paths.is_empty() {
                                info!(
                                    bucket_count = observations.len(),
                                    deleted_files = cleanup_result.deleted_running_paths.len(),
                                    disk_percent = cleanup_result.final_percent,
                                    "artifact cleanup deleted old running task segments"
                                );
                            }
                            if cleanup_result.final_percent >= threshold {
                                blocked_reason = Some(cleanup_result.reason);
                            }
                        }
                    }
                }
            }

            next_state.max_disk_percent =
                max_disk_percent(next_state.max_disk_percent, disk_percent);
            for observation in observations {
                let bucket_state = match &blocked_reason {
                    Some(reason) => {
                        ArtifactBucketState::blocked(Some(disk_percent), reason.clone())
                    }
                    None => ArtifactBucketState::allowed(Some(disk_percent)),
                };
                next_state.buckets.insert(observation.bucket, bucket_state);
            }
        }

        next_state.last_refresh_at = Some(Utc::now());
        ArtifactCleanupRefresh {
            state: next_state,
            stop_requests,
        }
    }

    fn cleanup_volume_by_disk(
        &self,
        observations: &[BucketObservation],
        active_handles: &[RuntimeHandle],
        threshold_percent: f64,
    ) -> CleanupVolumeResult {
        let target_percent = (threshold_percent - CLEANUP_HYSTERESIS_PERCENT).max(0.0);
        self.cleanup_volume_to_metric(
            observations,
            active_handles,
            CleanupMetricPolicy {
                threshold_percent,
                target_percent,
                metric_name: "artifact volume usage",
                candidate_label: "inactive task directories or running task segments",
            },
            || sample_disk_percent(observations[0].root.as_path()),
        )
    }

    fn cleanup_volume_to_metric(
        &self,
        observations: &[BucketObservation],
        active_handles: &[RuntimeHandle],
        policy: CleanupMetricPolicy<'_>,
        mut sample_metric_percent: impl FnMut() -> io::Result<f64>,
    ) -> CleanupVolumeResult {
        let mut metric_percent = sample_metric_percent().unwrap_or_else(|_| {
            observations
                .first()
                .map(|observation| observation.disk_percent)
                .unwrap_or_default()
        });
        let active_task_ids = active_handles
            .iter()
            .filter(|handle| handle.state != RuntimeState::Exited)
            .map(|handle| handle.task_id)
            .collect::<HashSet<_>>();
        let candidates = collect_cleanup_candidates(observations, &active_task_ids);
        let mut deleted_task_ids = Vec::new();
        let mut deleted_running_paths = Vec::new();

        // 先清理完整的非活跃任务目录；每删一个候选后都重新采样磁盘，
        // 这样可以尽早停止并减少对历史产物的破坏。
        for candidate in candidates {
            if metric_percent < policy.target_percent {
                break;
            }
            if let Err(error) = delete_cleanup_candidate(&candidate) {
                warn!(
                    task_id = %candidate.task_id,
                    error = %error,
                    "failed to delete artifact cleanup candidate"
                );
                continue;
            }
            deleted_task_ids.push(candidate.task_id);
            match sample_metric_percent() {
                Ok(value) => metric_percent = value,
                Err(error) => {
                    return CleanupVolumeResult {
                        deleted_task_ids,
                        deleted_running_paths,
                        final_percent: 100.0,
                        reason: format!(
                            "artifact volume could not be resampled after cleanup: {error}"
                        ),
                    };
                }
            }
        }

        if metric_percent >= policy.target_percent {
            // 完整任务目录不足以释放空间时，再清理运行中 HLS 的过期 segment。
            // MP4 和最新直播片段不进这里，避免破坏正在写入或播放的文件。
            let running_candidates = collect_running_cleanup_candidates(
                observations,
                active_handles,
                &self.inner.layout,
            );
            for candidate in running_candidates {
                if metric_percent < policy.target_percent {
                    break;
                }
                match fs::remove_file(&candidate.path) {
                    Ok(()) => {
                        deleted_running_paths.push(candidate.path.clone());
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        warn!(
                            task_id = %candidate.task_id,
                            bucket = candidate.bucket.as_str(),
                            path = %candidate.path.display(),
                            error = %error,
                            "failed to delete old running artifact segment"
                        );
                        continue;
                    }
                }
                match sample_metric_percent() {
                    Ok(value) => metric_percent = value,
                    Err(error) => {
                        return CleanupVolumeResult {
                            deleted_task_ids,
                            deleted_running_paths,
                            final_percent: 100.0,
                            reason: format!(
                                "artifact volume could not be resampled after running segment cleanup: {error}"
                            ),
                        };
                    }
                }
            }
        }

        let reason = if metric_percent >= policy.threshold_percent {
            if deleted_task_ids.is_empty() && deleted_running_paths.is_empty() {
                format!(
                    "{} {:.1}% exceeds threshold {:.1}% and no {} are eligible for cleanup",
                    policy.metric_name,
                    metric_percent,
                    policy.threshold_percent,
                    policy.candidate_label
                )
            } else {
                format!(
                    "{} {:.1}% remains above threshold {:.1}% after cleanup",
                    policy.metric_name, metric_percent, policy.threshold_percent
                )
            }
        } else {
            String::new()
        };

        CleanupVolumeResult {
            deleted_task_ids,
            deleted_running_paths,
            final_percent: metric_percent,
            reason,
        }
    }

    fn stop_requests_for_active_artifact_tasks_for_volume(
        &self,
        observations: &[BucketObservation],
        active_handles: &[RuntimeHandle],
        reason: &str,
    ) -> Vec<ArtifactCleanupStopRequest> {
        let volume_buckets = observations
            .iter()
            .map(|observation| observation.bucket)
            .collect::<HashSet<_>>();
        let mut stop_requests = Vec::new();

        for handle in active_handles {
            if matches!(handle.state, RuntimeState::Exited | RuntimeState::Stopping) {
                continue;
            }
            let handle_buckets = artifact_buckets_for_runtime_handle(handle, &self.inner.layout);
            if handle_buckets.is_disjoint(&volume_buckets) {
                continue;
            }
            let lease_token = handle
                .metadata
                .get("lease_token")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if lease_token.is_empty() {
                continue;
            }
            let request = StopTaskRequest {
                task_id: handle.task_id,
                attempt_no: handle.attempt_no,
                lease_token,
                reason: DISK_THRESHOLD_STOP_REASON.to_string(),
                grace_period_sec: 0,
                force_after_sec: 5,
            };
            stop_requests.push(ArtifactCleanupStopRequest {
                request,
                reason: reason.to_string(),
            });
        }

        stop_requests
    }

    async fn stop_active_artifact_tasks(&self, stop_requests: Vec<ArtifactCleanupStopRequest>) {
        if stop_requests.is_empty() {
            return;
        }
        let Some(executor) = self.inner.executor.as_ref().cloned() else {
            warn!(
                task_count = stop_requests.len(),
                "artifact cleanup cannot stop active tasks because no executor is attached"
            );
            return;
        };

        for stop_request in stop_requests {
            match executor.stop_task(stop_request.request.clone()).await {
                Ok(()) => {
                    warn!(
                        task_id = %stop_request.request.task_id,
                        attempt_no = stop_request.request.attempt_no,
                        reason = %stop_request.reason,
                        "artifact cleanup stopped active task after disk threshold was exceeded"
                    );
                }
                Err(error) => {
                    warn!(
                        task_id = %stop_request.request.task_id,
                        attempt_no = stop_request.request.attempt_no,
                        error = %error,
                        "artifact cleanup failed to stop active task after disk threshold was exceeded"
                    );
                }
            }
        }
    }

    fn apply_state(&self, next_state: ArtifactCleanupState) {
        let previous_state = {
            let mut state = self
                .inner
                .state
                .write()
                .expect("artifact cleanup state lock poisoned");
            let previous = state.clone();
            *state = next_state.clone();
            previous
        };

        for bucket in [ArtifactBucket::Mp4, ArtifactBucket::Hls] {
            let previous = previous_state.buckets.get(&bucket);
            let current = next_state.buckets.get(&bucket);
            if previous.is_some_and(|state| {
                current.is_some_and(|current| {
                    state.start_allowed == current.start_allowed
                        && state.reason == current.reason
                        && state.disk_percent == current.disk_percent
                })
            }) {
                continue;
            }
            if let Some(current) = current {
                if current.start_allowed {
                    info!(
                        bucket = bucket.as_str(),
                        disk_percent = current.disk_percent,
                        "artifact bucket is accepting new tasks"
                    );
                } else {
                    warn!(
                        bucket = bucket.as_str(),
                        disk_percent = current.disk_percent,
                        reason = %current.reason,
                        "artifact bucket is rejecting new tasks"
                    );
                }
            }
        }
    }

    #[cfg(test)]
    fn set_bucket_state_for_test(
        &self,
        bucket: ArtifactBucket,
        disk_percent: Option<f64>,
        start_allowed: bool,
        reason: impl Into<String>,
    ) {
        let mut state = self
            .inner
            .state
            .write()
            .expect("artifact cleanup state lock poisoned");
        state.buckets.insert(
            bucket,
            ArtifactBucketState {
                disk_percent,
                start_allowed,
                reason: reason.into(),
            },
        );
        state.max_disk_percent = match disk_percent {
            Some(value) => max_disk_percent(state.max_disk_percent, value),
            None => state.max_disk_percent,
        };
    }
}

#[derive(Debug)]
struct CleanupVolumeResult {
    deleted_task_ids: Vec<Uuid>,
    deleted_running_paths: Vec<PathBuf>,
    final_percent: f64,
    reason: String,
}

#[derive(Debug, Clone, Copy)]
struct CleanupMetricPolicy<'a> {
    threshold_percent: f64,
    target_percent: f64,
    metric_name: &'a str,
    candidate_label: &'a str,
}

pub fn artifact_buckets_for_task_spec(spec: &TaskSpec) -> Vec<ArtifactBucket> {
    let mut buckets = Vec::new();

    if spec.task_type == TaskType::StreamIngest && spec.record.enabled.unwrap_or(false) {
        match spec.record.format.unwrap_or(RecordFormat::Mp4) {
            RecordFormat::Mp4 => buckets.push(ArtifactBucket::Mp4),
            RecordFormat::Hls => buckets.push(ArtifactBucket::Hls),
            RecordFormat::Both => {
                buckets.push(ArtifactBucket::Mp4);
                buckets.push(ArtifactBucket::Hls);
            }
        }
    }

    if matches!(
        spec.task_type,
        TaskType::FileTranscode | TaskType::StreamBridge
    ) && spec.publish.kind == Some(PublishTargetKind::File)
    {
        let bucket = match spec.publish.format.as_deref().map(str::trim) {
            Some(value) if value.eq_ignore_ascii_case("hls") => ArtifactBucket::Hls,
            _ => ArtifactBucket::Mp4,
        };
        if !buckets.contains(&bucket) {
            buckets.push(bucket);
        }
    }

    buckets
}

fn inspect_bucket_root(
    bucket: ArtifactBucket,
    layout: &ArtifactBucketLayout,
) -> io::Result<BucketObservation> {
    let metadata = fs::metadata(&layout.root)?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a directory", layout.root.display()),
        ));
    }
    let disk = sample_disk(&layout.root)?;
    Ok(BucketObservation {
        bucket,
        root: layout.root.clone(),
        node_dir: layout.node_dir.clone(),
        device_id: metadata.dev(),
        disk_percent: disk.percent_used,
    })
}

fn collect_cleanup_candidates(
    observations: &[BucketObservation],
    active_task_ids: &HashSet<Uuid>,
) -> Vec<CleanupCandidate> {
    let mut by_task_id = HashMap::<Uuid, CleanupCandidate>::new();
    let now = SystemTime::now();

    // 同一任务可能在多个 bucket 下都有产物，按 task_id 聚合后作为一个删除单元，
    // 避免只删 MP4 或只删 HLS 导致数据库仍指向半残留目录。
    for observation in observations {
        let entries = match fs::read_dir(&observation.node_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                warn!(
                    bucket = observation.bucket.as_str(),
                    node_dir = %observation.node_dir.display(),
                    error = %error,
                    "failed to enumerate artifact cleanup node directory"
                );
                continue;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let Some(task_id) = path
                .file_name()
                .and_then(|value| value.to_str())
                .and_then(|value| Uuid::parse_str(value).ok())
            else {
                continue;
            };
            if active_task_ids.contains(&task_id) {
                continue;
            }
            let last_write = latest_file_write_time(&path).unwrap_or(SystemTime::UNIX_EPOCH);
            // 最近仍有写入的目录先跳过，给刚完成但尚未入库/回调的产物留缓冲时间。
            if now
                .duration_since(last_write)
                .is_ok_and(|age| age < CLEANUP_RECENT_WRITE_GRACE_PERIOD)
            {
                continue;
            }
            by_task_id
                .entry(task_id)
                .and_modify(|candidate| {
                    if candidate.last_write < last_write {
                        candidate.last_write = last_write;
                    }
                    candidate.paths.push(path.clone());
                })
                .or_insert_with(|| CleanupCandidate {
                    task_id,
                    last_write,
                    paths: vec![path],
                });
        }
    }

    let mut candidates = by_task_id.into_values().collect::<Vec<_>>();
    candidates.sort_by_key(|candidate| candidate.last_write);
    candidates
}

fn collect_running_cleanup_candidates(
    observations: &[BucketObservation],
    active_handles: &[RuntimeHandle],
    layout: &ArtifactCleanupLayout,
) -> Vec<RunningCleanupCandidate> {
    let active_by_task_id = active_handles
        .iter()
        .filter(|handle| handle.state != RuntimeState::Exited)
        .map(|handle| (handle.task_id, handle))
        .collect::<HashMap<_, _>>();
    let now = SystemTime::now();
    let mut candidates = Vec::new();

    for observation in observations {
        let entries = match fs::read_dir(&observation.node_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                warn!(
                    bucket = observation.bucket.as_str(),
                    node_dir = %observation.node_dir.display(),
                    error = %error,
                    "failed to enumerate artifact cleanup running node directory"
                );
                continue;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let Some(task_id) = path
                .file_name()
                .and_then(|value| value.to_str())
                .and_then(|value| Uuid::parse_str(value).ok())
            else {
                continue;
            };
            let Some(handle) = active_by_task_id.get(&task_id).copied() else {
                continue;
            };
            let handle_buckets = artifact_buckets_for_runtime_handle(handle, layout);
            if !handle_buckets.contains(&observation.bucket) {
                continue;
            }

            match observation.bucket {
                ArtifactBucket::Mp4 => {
                    candidates.extend(collect_running_mp4_candidates(
                        task_id,
                        observation.bucket,
                        &path,
                        now,
                    ));
                }
                ArtifactBucket::Hls => {
                    candidates.extend(collect_running_hls_candidates(
                        task_id,
                        observation.bucket,
                        &path,
                        now,
                    ));
                }
            }
        }
    }

    candidates.sort_by_key(|candidate| (candidate.last_write, candidate.size_bytes));
    candidates
}

fn collect_running_mp4_candidates(
    task_id: Uuid,
    bucket: ArtifactBucket,
    task_dir: &Path,
    now: SystemTime,
) -> Vec<RunningCleanupCandidate> {
    let mut files = list_files_with_extension(task_dir, "mp4")
        .into_iter()
        .filter(|path| {
            path.file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|name| !name.starts_with('.'))
        })
        .filter_map(|path| running_candidate_from_path(task_id, bucket, path, now))
        .collect::<Vec<_>>();
    files.sort_by_key(|candidate| candidate.last_write);
    retain_oldest_beyond_latest(files)
}

fn collect_running_hls_candidates(
    task_id: Uuid,
    bucket: ArtifactBucket,
    task_dir: &Path,
    now: SystemTime,
) -> Vec<RunningCleanupCandidate> {
    let referenced_segments = hls_playlist_references(task_dir);
    let mut files = list_files_with_extension(task_dir, "ts")
        .into_iter()
        .filter(|path| !referenced_segments.contains(&normalize_existing_or_lexical_path(path)))
        .filter_map(|path| running_candidate_from_path(task_id, bucket, path, now))
        .collect::<Vec<_>>();
    files.sort_by_key(|candidate| candidate.last_write);
    retain_oldest_beyond_latest(files)
}

fn retain_oldest_beyond_latest(
    mut files: Vec<RunningCleanupCandidate>,
) -> Vec<RunningCleanupCandidate> {
    if files.len() <= RUNNING_SEGMENT_RETAIN_COUNT {
        return Vec::new();
    }
    let keep_from = files.len() - RUNNING_SEGMENT_RETAIN_COUNT;
    files.truncate(keep_from);
    files
}

fn running_candidate_from_path(
    task_id: Uuid,
    bucket: ArtifactBucket,
    path: PathBuf,
    now: SystemTime,
) -> Option<RunningCleanupCandidate> {
    let metadata = fs::metadata(&path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let last_write = metadata.modified().ok()?;
    if now
        .duration_since(last_write)
        .is_ok_and(|age| age < CLEANUP_RECENT_WRITE_GRACE_PERIOD)
    {
        return None;
    }
    Some(RunningCleanupCandidate {
        task_id,
        bucket,
        path,
        last_write,
        size_bytes: metadata.len(),
    })
}

fn list_files_with_extension(root: &Path, extension: &str) -> Vec<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                pending.push(path);
                continue;
            }
            if file_type.is_file()
                && path
                    .extension()
                    .and_then(|value| value.to_str())
                    .is_some_and(|value| value.eq_ignore_ascii_case(extension))
            {
                files.push(path);
            }
        }
    }
    files
}

fn hls_playlist_references(task_dir: &Path) -> HashSet<PathBuf> {
    let mut references = HashSet::new();
    for playlist in list_files_with_extension(task_dir, "m3u8") {
        let Some(parent) = playlist.parent() else {
            continue;
        };
        let Ok(mut file) = fs::File::open(&playlist) else {
            continue;
        };
        let mut contents = String::new();
        if file.read_to_string(&mut contents).is_err() {
            continue;
        }
        for line in contents.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if !line
                .rsplit_once('.')
                .map(|(_, extension)| extension.eq_ignore_ascii_case("ts"))
                .unwrap_or(false)
            {
                continue;
            }
            let path = Path::new(line);
            let resolved = if path.is_absolute() {
                path.to_path_buf()
            } else {
                parent.join(path)
            };
            references.insert(normalize_existing_or_lexical_path(&resolved));
        }
    }
    references
}

fn normalize_existing_or_lexical_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| {
        let mut normalized = PathBuf::new();
        for component in path.components() {
            normalized.push(component.as_os_str());
        }
        normalized
    })
}

fn artifact_buckets_for_runtime_handle(
    handle: &RuntimeHandle,
    layout: &ArtifactCleanupLayout,
) -> HashSet<ArtifactBucket> {
    let mut buckets = HashSet::new();
    if let Some(spec) = handle
        .metadata
        .get("resolved_spec")
        .cloned()
        .and_then(|value| serde_json::from_value::<TaskSpec>(value).ok())
    {
        buckets.extend(artifact_buckets_for_task_spec(&spec));
    }
    for output in &handle.outputs {
        add_bucket_for_path(output, &mut buckets, layout);
    }
    if let Some(recording) = handle.metadata.get("recording") {
        for key in ["root_path_mp4", "root_path_hls"] {
            if let Some(path) = recording.get(key).and_then(Value::as_str) {
                add_bucket_for_path(path, &mut buckets, layout);
            }
        }
    }
    buckets
}

fn add_bucket_for_path(
    path: &str,
    buckets: &mut HashSet<ArtifactBucket>,
    layout: &ArtifactCleanupLayout,
) {
    let path = Path::new(path);
    if let Some(bucket) = layout.bucket_for_path(path) {
        buckets.insert(bucket);
    }
}

fn latest_file_write_time(path: &Path) -> Option<SystemTime> {
    let metadata = fs::metadata(path).ok()?;
    let mut latest = metadata.modified().ok().unwrap_or(SystemTime::UNIX_EPOCH);
    if !metadata.is_dir() {
        return Some(latest);
    }

    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(dir).ok()?;
        for entry in entries.flatten() {
            let entry_path = entry.path();
            let entry_metadata = entry.metadata().ok()?;
            if let Ok(modified) = entry_metadata.modified() {
                if latest < modified {
                    latest = modified;
                }
            }
            if entry_metadata.is_dir() {
                stack.push(entry_path);
            }
        }
    }

    Some(latest)
}

fn delete_cleanup_candidate(candidate: &CleanupCandidate) -> io::Result<()> {
    for path in &candidate.paths {
        match fs::remove_dir_all(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn sample_disk_percent(path: &Path) -> io::Result<f64> {
    sample_disk(path).map(|sample| sample.percent_used)
}

fn sample_disk(path: &Path) -> io::Result<DiskSample> {
    let path = CString::new(path.to_string_lossy().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid disk path"))?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    let rc = unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }

    let stat = unsafe { stat.assume_init() };
    let total = stat.f_blocks.saturating_mul(stat.f_frsize);
    let free = stat.f_bavail.saturating_mul(stat.f_frsize);
    if total == 0 {
        return Ok(DiskSample { percent_used: 0.0 });
    }

    Ok(DiskSample {
        percent_used: ((total - free) as f64 / total as f64) * 100.0,
    })
}

fn sanitize_output_node_token(value: &str) -> String {
    let mut sanitized = String::new();
    let mut previous_was_separator = false;
    for value in value.trim().chars() {
        let mapped = match value {
            value if value.is_ascii_alphanumeric() => Some(value.to_ascii_lowercase()),
            '-' => Some('-'),
            '_' | '.' | ':' => Some('_'),
            _ => Some('_'),
        };
        let Some(mapped) = mapped else {
            continue;
        };
        if mapped == '_' {
            if previous_was_separator {
                continue;
            }
            previous_was_separator = true;
        } else {
            previous_was_separator = false;
        }
        sanitized.push(mapped);
    }
    let sanitized = sanitized.trim_matches('_').to_string();
    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}

fn artifact_node_token(settings: &AgentSettings) -> String {
    if settings
        .primary_interface_ip
        .trim()
        .parse::<IpAddr>()
        .is_ok()
    {
        return sanitize_output_node_token(&settings.primary_interface_ip);
    }
    if let Ok(url) = reqwest::Url::parse(settings.agent_stream_addr.trim()) {
        if let Some(host) = url.host_str() {
            if host.parse::<IpAddr>().is_ok() {
                return sanitize_output_node_token(host);
            }
        }
    }
    "unknown".to_string()
}

fn artifact_node_dir_name(settings: &AgentSettings, bucket: ArtifactBucket) -> String {
    format!("node-{}-{}", artifact_node_token(settings), bucket.as_str())
}

fn max_disk_percent(current: Option<f64>, next: f64) -> Option<f64> {
    Some(current.map_or(next, |current| current.max(next)))
}
