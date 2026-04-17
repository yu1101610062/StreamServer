#[cfg(test)]
#[path = "tests/artifact_cleanup.rs"]
mod tests;

use std::{
    collections::{HashMap, HashSet},
    ffi::CString,
    fs, io,
    net::IpAddr,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::{Duration, SystemTime},
};

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use media_domain::{PublishTargetKind, RecordFormat, TaskSpec, TaskType};
use serde_json::Value;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{info, warn};
use uuid::Uuid;

use crate::{
    config::{AgentArtifactCleanupSettings, AgentSettings},
    runtime::LocalRuntimeRegistry,
};

const ZLM_OUTPUT_MP4_ROOT: &str = "/data/zlm/www/output/mp4";
const ZLM_OUTPUT_HLS_ROOT: &str = "/data/zlm/www/output/hls";
const CLEANUP_HYSTERESIS_PERCENT: f64 = 5.0;
const CLEANUP_RECENT_WRITE_GRACE_PERIOD: Duration = Duration::from_secs(60);

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

    fn root(self) -> &'static str {
        match self {
            Self::Mp4 => ZLM_OUTPUT_MP4_ROOT,
            Self::Hls => ZLM_OUTPUT_HLS_ROOT,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ArtifactCleanupManager {
    inner: Arc<ArtifactCleanupManagerInner>,
}

#[derive(Debug)]
struct ArtifactCleanupManagerInner {
    cleanup: AgentArtifactCleanupSettings,
    registry: LocalRuntimeRegistry,
    layout: ArtifactCleanupLayout,
    state: RwLock<ArtifactCleanupState>,
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
    buckets: HashMap<ArtifactBucket, ArtifactBucketState>,
    max_disk_percent: Option<f64>,
    last_refresh_at: Option<DateTime<Utc>>,
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
    task_id: Uuid,
    last_write: SystemTime,
    paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArtifactCleanupStrategy {
    DeleteOldestThenReject,
    RejectOnly,
}

impl ArtifactCleanupStrategy {
    fn from_settings(settings: &AgentArtifactCleanupSettings) -> Self {
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
        let mut buckets = HashMap::new();
        for bucket in [ArtifactBucket::Mp4, ArtifactBucket::Hls] {
            let root = PathBuf::from(bucket.root());
            let node_dir = root.join(artifact_node_dir_name(settings, bucket));
            buckets.insert(bucket, ArtifactBucketLayout { root, node_dir });
        }
        Self { buckets }
    }
}

impl ArtifactCleanupManager {
    pub fn new(settings: &AgentSettings, registry: LocalRuntimeRegistry) -> Self {
        let layout = ArtifactCleanupLayout::from_settings(settings);
        let state = ArtifactCleanupState::unknown(&layout);
        Self {
            inner: Arc::new(ArtifactCleanupManagerInner {
                cleanup: settings.artifact_cleanup.clone(),
                registry,
                layout,
                state: RwLock::new(state),
            }),
        }
    }

    pub async fn refresh_now(&self) {
        let this = self.clone();
        match tokio::task::spawn_blocking(move || this.refresh_blocking()).await {
            Ok(next_state) => self.apply_state(next_state),
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

    fn refresh_blocking(&self) -> ArtifactCleanupState {
        let mut next_state = ArtifactCleanupState::default();
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

        let active_task_ids = self.inner.registry.tracked_task_ids();
        let strategy = ArtifactCleanupStrategy::from_settings(&self.inner.cleanup);
        let threshold = self.inner.cleanup.threshold_percent;
        let mut by_device: HashMap<u64, Vec<BucketObservation>> = HashMap::new();
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

            if self.inner.cleanup.enabled && disk_percent >= threshold {
                match strategy {
                    ArtifactCleanupStrategy::RejectOnly => {
                        blocked_reason = Some(format!(
                            "artifact volume usage {:.1}% exceeds threshold {:.1}%",
                            disk_percent, threshold
                        ));
                    }
                    ArtifactCleanupStrategy::DeleteOldestThenReject => {
                        let cleanup_result =
                            self.cleanup_volume(&observations, &active_task_ids, threshold);
                        disk_percent = cleanup_result.final_disk_percent;
                        if !cleanup_result.deleted_task_ids.is_empty() {
                            info!(
                                bucket_count = observations.len(),
                                deleted_tasks = ?cleanup_result.deleted_task_ids,
                                disk_percent = cleanup_result.final_disk_percent,
                                "artifact cleanup deleted old task directories"
                            );
                        }
                        if cleanup_result.final_disk_percent >= threshold {
                            blocked_reason = Some(cleanup_result.reason);
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
        next_state
    }

    fn cleanup_volume(
        &self,
        observations: &[BucketObservation],
        active_task_ids: &HashSet<Uuid>,
        threshold_percent: f64,
    ) -> CleanupVolumeResult {
        let target_percent = (threshold_percent - CLEANUP_HYSTERESIS_PERCENT).max(0.0);
        let mut disk_percent = observations
            .first()
            .map(|observation| observation.disk_percent)
            .unwrap_or_default();
        let candidates = collect_cleanup_candidates(observations, active_task_ids);
        let mut deleted_task_ids = Vec::new();

        for candidate in candidates {
            if disk_percent < target_percent {
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
            match sample_disk_percent(observations[0].root.as_path()) {
                Ok(value) => disk_percent = value,
                Err(error) => {
                    return CleanupVolumeResult {
                        deleted_task_ids,
                        final_disk_percent: 100.0,
                        reason: format!(
                            "artifact volume could not be resampled after cleanup: {error}"
                        ),
                    };
                }
            }
        }

        let reason = if disk_percent >= threshold_percent {
            if deleted_task_ids.is_empty() {
                format!(
                    "artifact volume usage {:.1}% exceeds threshold {:.1}% and no inactive task directories are eligible for cleanup",
                    disk_percent, threshold_percent
                )
            } else {
                format!(
                    "artifact volume usage {:.1}% remains above threshold {:.1}% after cleanup",
                    disk_percent, threshold_percent
                )
            }
        } else {
            String::new()
        };

        CleanupVolumeResult {
            deleted_task_ids,
            final_disk_percent: disk_percent,
            reason,
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
    final_disk_percent: f64,
    reason: String,
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
    let disk_percent = sample_disk_percent(&layout.root)?;
    Ok(BucketObservation {
        bucket,
        root: layout.root.clone(),
        node_dir: layout.node_dir.clone(),
        device_id: metadata.dev(),
        disk_percent,
    })
}

fn collect_cleanup_candidates(
    observations: &[BucketObservation],
    active_task_ids: &HashSet<Uuid>,
) -> Vec<CleanupCandidate> {
    let mut by_task_id = HashMap::<Uuid, CleanupCandidate>::new();
    let now = SystemTime::now();

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
    let path = CString::new(path.to_string_lossy().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid disk path"))?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    let rc = unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }

    let stat = unsafe { stat.assume_init() };
    let total = (stat.f_blocks as u64).saturating_mul(stat.f_frsize as u64);
    let free = (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64);
    if total == 0 {
        return Ok(0.0);
    }

    Ok(((total - free) as f64 / total as f64) * 100.0)
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
