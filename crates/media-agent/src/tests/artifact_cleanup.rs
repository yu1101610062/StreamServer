use std::{
    collections::HashSet,
    ffi::CString,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use chrono::Utc;
use media_domain::{
    CommonSpec, ExposeSpec, InputSpec, ProcessSpec, PublishSpec, PublishTargetKind, RecordFormat,
    RecordSpec, RecoverySpec, ResourceSpec, RuntimeHandle, RuntimeState, ScheduleSpec, StreamSpec,
    TaskSpec, TaskType, WorkerKind,
};
use serde_json::json;
use uuid::Uuid;

use crate::runtime::{
    AdoptFilter, ExecutorError, LocalExecutor, StartTaskRequest, StopTaskRequest,
    TaskRecordingControlRequest,
};

use super::*;

const TEST_ZLM_OUTPUT_MP4_ROOT: &str = "/data/zlm/www/output/mp4";

fn sample_spec() -> TaskSpec {
    TaskSpec {
        task_type: TaskType::StreamIngest,
        name: "artifact-cleanup-test".to_string(),
        priority: 50,
        common: CommonSpec::default(),
        input: InputSpec::default(),
        stream: StreamSpec::default(),
        expose: ExposeSpec::default(),
        process: ProcessSpec::default(),
        publish: PublishSpec::default(),
        record: RecordSpec::default(),
        recovery: RecoverySpec::default(),
        schedule: ScheduleSpec::default(),
        resource: ResourceSpec::default(),
    }
}

fn set_mtime(path: &Path, seconds_ago: i64) {
    let now = SystemTime::now()
        .checked_sub(Duration::from_secs(seconds_ago as u64))
        .expect("valid mtime");
    let duration = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("timestamp after epoch");
    let spec = [
        libc::timespec {
            tv_sec: duration.as_secs() as libc::time_t,
            tv_nsec: duration.subsec_nanos() as libc::c_long,
        },
        libc::timespec {
            tv_sec: duration.as_secs() as libc::time_t,
            tv_nsec: duration.subsec_nanos() as libc::c_long,
        },
    ];
    let path = CString::new(path.to_string_lossy().as_bytes()).expect("valid path");
    let rc = unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), spec.as_ptr(), 0) };
    assert_eq!(rc, 0, "mtime should be updated");
}

#[test]
fn artifact_buckets_follow_task_spec_outputs() {
    let mut spec = sample_spec();
    spec.record.enabled = Some(true);
    spec.record.format = Some(RecordFormat::Mp4);
    assert_eq!(
        artifact_buckets_for_task_spec(&spec),
        vec![ArtifactBucket::Mp4]
    );

    spec.record.format = Some(RecordFormat::Hls);
    assert_eq!(
        artifact_buckets_for_task_spec(&spec),
        vec![ArtifactBucket::Hls]
    );

    spec.record.format = Some(RecordFormat::Both);
    assert_eq!(
        artifact_buckets_for_task_spec(&spec),
        vec![ArtifactBucket::Mp4, ArtifactBucket::Hls]
    );

    let mut bridge = TaskSpec {
        task_type: TaskType::StreamBridge,
        name: "bridge-file".to_string(),
        priority: 50,
        common: CommonSpec::default(),
        input: InputSpec::default(),
        stream: StreamSpec::default(),
        expose: ExposeSpec::default(),
        process: ProcessSpec::default(),
        publish: PublishSpec::default(),
        record: RecordSpec::default(),
        recovery: RecoverySpec::default(),
        schedule: ScheduleSpec::default(),
        resource: ResourceSpec::default(),
    };
    bridge.publish.kind = Some(PublishTargetKind::File);
    assert_eq!(
        artifact_buckets_for_task_spec(&bridge),
        vec![ArtifactBucket::Mp4]
    );

    bridge.publish.format = Some("hls".to_string());
    assert_eq!(
        artifact_buckets_for_task_spec(&bridge),
        vec![ArtifactBucket::Hls]
    );
}

#[test]
fn collect_cleanup_candidates_groups_task_dirs_and_skips_active_tasks() {
    let temp_root =
        std::env::temp_dir().join(format!("streamserver-artifact-cleanup-{}", Uuid::now_v7()));
    let mp4_node_dir = temp_root.join("mp4").join("node-test-mp4");
    let hls_node_dir = temp_root.join("hls").join("node-test-hls");
    fs::create_dir_all(&mp4_node_dir).expect("mp4 node dir should exist");
    fs::create_dir_all(&hls_node_dir).expect("hls node dir should exist");

    let stale_task_id = Uuid::now_v7();
    let active_task_id = Uuid::now_v7();

    let stale_mp4_file = mp4_node_dir
        .join(stale_task_id.to_string())
        .join("record")
        .join("clip.mp4");
    let stale_hls_file = hls_node_dir
        .join(stale_task_id.to_string())
        .join("record")
        .join("clip.m3u8");
    let active_file = mp4_node_dir
        .join(active_task_id.to_string())
        .join("record")
        .join("active.mp4");

    fs::create_dir_all(stale_mp4_file.parent().expect("mp4 parent")).expect("mp4 parent dir");
    fs::create_dir_all(stale_hls_file.parent().expect("hls parent")).expect("hls parent dir");
    fs::create_dir_all(active_file.parent().expect("active parent")).expect("active parent dir");
    fs::write(&stale_mp4_file, b"old mp4").expect("stale mp4 file");
    fs::write(&stale_hls_file, b"old hls").expect("stale hls file");
    fs::write(&active_file, b"active").expect("active file");

    set_mtime(
        stale_mp4_file
            .parent()
            .and_then(Path::parent)
            .expect("stale mp4 task dir"),
        120,
    );
    set_mtime(stale_mp4_file.parent().expect("stale mp4 record dir"), 120);
    set_mtime(&stale_mp4_file, 120);
    set_mtime(
        stale_hls_file
            .parent()
            .and_then(Path::parent)
            .expect("stale hls task dir"),
        90,
    );
    set_mtime(stale_hls_file.parent().expect("stale hls record dir"), 90);
    set_mtime(&stale_hls_file, 90);
    set_mtime(
        active_file
            .parent()
            .and_then(Path::parent)
            .expect("active task dir"),
        120,
    );
    set_mtime(active_file.parent().expect("active record dir"), 120);
    set_mtime(&active_file, 120);

    let active_task_ids = HashSet::from([active_task_id]);
    let candidates = collect_cleanup_candidates(
        &[
            BucketObservation {
                bucket: ArtifactBucket::Mp4,
                root: temp_root.join("mp4"),
                node_dir: mp4_node_dir.clone(),
                device_id: 1,
                disk_percent: 91.0,
            },
            BucketObservation {
                bucket: ArtifactBucket::Hls,
                root: temp_root.join("hls"),
                node_dir: hls_node_dir.clone(),
                device_id: 1,
                disk_percent: 91.0,
            },
        ],
        &active_task_ids,
    );

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].task_id, stale_task_id);
    assert_eq!(candidates[0].paths.len(), 2);
    assert!(
        candidates[0]
            .paths
            .iter()
            .any(|path| path.starts_with(&mp4_node_dir))
    );
    assert!(
        candidates[0]
            .paths
            .iter()
            .any(|path| path.starts_with(&hls_node_dir))
    );

    let _ = fs::remove_dir_all(&temp_root);
}

#[test]
fn task_start_rejection_uses_cached_bucket_state() {
    let manager =
        ArtifactCleanupManager::new(&AgentSettings::default(), LocalRuntimeRegistry::new());
    manager.set_bucket_state_for_test(
        ArtifactBucket::Mp4,
        Some(92.0),
        false,
        "artifact volume usage 92.0% exceeds threshold 85.0%",
    );

    let resolved_spec = json!({
        "type": "stream_ingest",
        "name": "record-only-live",
        "common": {"created_by": "tester"},
        "input": {"kind": "http_ts", "source_mode": "live", "url": "http://example.com/live.ts"},
        "stream": {"app": "objective", "name": "objective-1"},
        "record": {"enabled": true, "format": "mp4"},
        "resource": {}
    });

    let error = manager
        .ensure_task_start_allowed(&resolved_spec)
        .expect_err("mp4 bucket should be rejected");
    assert!(
        error
            .to_string()
            .contains("artifact bucket mp4 is not ready")
    );
}

#[test]
fn running_cleanup_deletes_only_safe_old_segments() {
    let temp_root =
        std::env::temp_dir().join(format!("streamserver-running-cleanup-{}", Uuid::now_v7()));
    let task_id = Uuid::now_v7();
    let mp4_task_dir = temp_root
        .join("mp4")
        .join("node-test-mp4")
        .join(task_id.to_string());
    let hls_task_dir = temp_root
        .join("hls")
        .join("node-test-hls")
        .join(task_id.to_string());
    fs::create_dir_all(&mp4_task_dir).expect("mp4 task dir");
    fs::create_dir_all(&hls_task_dir).expect("hls task dir");

    let old_mp4 = mp4_task_dir.join("001.mp4");
    let second_old_mp4 = mp4_task_dir.join("002.mp4");
    let retained_mp4 = mp4_task_dir.join("003.mp4");
    let latest_mp4 = mp4_task_dir.join("004.mp4");
    let hidden_mp4 = mp4_task_dir.join(".tmp.mp4");
    for path in [
        &old_mp4,
        &second_old_mp4,
        &retained_mp4,
        &latest_mp4,
        &hidden_mp4,
    ] {
        fs::write(path, b"mp4").expect("mp4 file");
        set_mtime(path, 120);
    }

    let playlist = hls_task_dir.join("index.m3u8");
    let old_ts = hls_task_dir.join("index-00001.ts");
    let referenced_ts = hls_task_dir.join("index-00002.ts");
    let retained_ts = hls_task_dir.join("index-00003.ts");
    let latest_ts = hls_task_dir.join("index-00004.ts");
    fs::write(&playlist, "#EXTM3U\nindex-00002.ts\n").expect("playlist");
    for path in [&old_ts, &referenced_ts, &retained_ts, &latest_ts] {
        fs::write(path, b"ts").expect("ts file");
        set_mtime(path, 120);
    }
    set_mtime(&playlist, 120);

    let mut spec = sample_spec();
    spec.record.enabled = Some(true);
    spec.record.format = Some(RecordFormat::Both);
    let handle = RuntimeHandle {
        runtime_id: Uuid::now_v7(),
        task_id,
        attempt_no: 1,
        worker_kind: WorkerKind::Ffmpeg,
        pid: Some(1234),
        started_at: Utc::now(),
        last_progress_at: None,
        state: RuntimeState::Running,
        command_line: None,
        outputs: vec![
            mp4_task_dir.to_string_lossy().to_string(),
            hls_task_dir.to_string_lossy().to_string(),
        ],
        metadata: json!({
            "lease_token": "lease-1",
            "resolved_spec": spec
        }),
    };

    let observations = [
        BucketObservation {
            bucket: ArtifactBucket::Mp4,
            root: temp_root.join("mp4"),
            node_dir: temp_root.join("mp4").join("node-test-mp4"),
            device_id: 1,
            disk_percent: 90.0,
        },
        BucketObservation {
            bucket: ArtifactBucket::Hls,
            root: temp_root.join("hls"),
            node_dir: temp_root.join("hls").join("node-test-hls"),
            device_id: 1,
            disk_percent: 90.0,
        },
    ];

    let mut settings = AgentSettings::default();
    settings.zlm_output_mp4_root = temp_root.join("mp4").to_string_lossy().to_string();
    settings.zlm_output_hls_root = temp_root.join("hls").to_string_lossy().to_string();
    let layout = ArtifactCleanupLayout::from_settings(&settings);
    let candidates = collect_running_cleanup_candidates(&observations, &[handle], &layout);
    let candidate_paths = candidates
        .iter()
        .map(|candidate| candidate.path.clone())
        .collect::<HashSet<_>>();

    assert!(candidate_paths.contains(&old_mp4));
    assert!(candidate_paths.contains(&second_old_mp4));
    assert!(candidate_paths.contains(&old_ts));
    assert!(!candidate_paths.contains(&retained_mp4));
    assert!(!candidate_paths.contains(&latest_mp4));
    assert!(!candidate_paths.contains(&hidden_mp4));
    assert!(!candidate_paths.contains(&referenced_ts));
    assert!(!candidate_paths.contains(&retained_ts));
    assert!(!candidate_paths.contains(&latest_ts));

    let _ = fs::remove_dir_all(temp_root);
}

#[derive(Debug, Default)]
struct RecordingExecutor {
    stops: Mutex<Vec<StopTaskRequest>>,
}

impl LocalExecutor for RecordingExecutor {
    fn start_task(&self, _request: &StartTaskRequest) -> Result<RuntimeHandle, ExecutorError> {
        Err(ExecutorError::InvalidRequest("unused".to_string()))
    }

    fn stop_task(&self, request: &StopTaskRequest) -> Result<(), ExecutorError> {
        self.stops.lock().expect("stops lock").push(request.clone());
        Ok(())
    }

    fn set_task_recording(
        &self,
        _request: &TaskRecordingControlRequest,
    ) -> Result<RuntimeHandle, ExecutorError> {
        Err(ExecutorError::InvalidRequest("unused".to_string()))
    }

    fn adopt_orphans(&self, _filter: &AdoptFilter) -> Vec<RuntimeHandle> {
        Vec::new()
    }
}

#[test]
fn reject_only_stop_action_targets_only_artifact_tasks_on_volume() {
    let executor = Arc::new(RecordingExecutor::default());
    let manager = ArtifactCleanupManager::with_executor(
        &AgentSettings::default(),
        LocalRuntimeRegistry::new(),
        Some(executor.clone()),
    );
    let artifact_task = Uuid::now_v7();
    let no_output_task = Uuid::now_v7();
    let observations = [BucketObservation {
        bucket: ArtifactBucket::Mp4,
        root: PathBuf::from(TEST_ZLM_OUTPUT_MP4_ROOT),
        node_dir: PathBuf::from(TEST_ZLM_OUTPUT_MP4_ROOT).join("node-test-mp4"),
        device_id: 1,
        disk_percent: 95.0,
    }];
    let handles = vec![
        RuntimeHandle {
            runtime_id: Uuid::now_v7(),
            task_id: artifact_task,
            attempt_no: 1,
            worker_kind: WorkerKind::Ffmpeg,
            pid: Some(1234),
            started_at: Utc::now(),
            last_progress_at: None,
            state: RuntimeState::Running,
            command_line: None,
            outputs: vec![format!(
                "{TEST_ZLM_OUTPUT_MP4_ROOT}/node-test-mp4/{artifact_task}/out.mp4"
            )],
            metadata: json!({
                "lease_token": "lease-artifact",
                "resolved_spec": {
                    "type": "stream_ingest",
                    "name": "record",
                    "input": {},
                    "stream": {},
                    "record": {"enabled": true, "format": "mp4"},
                    "resource": {}
                }
            }),
        },
        RuntimeHandle {
            runtime_id: Uuid::now_v7(),
            task_id: no_output_task,
            attempt_no: 1,
            worker_kind: WorkerKind::Ffmpeg,
            pid: Some(1235),
            started_at: Utc::now(),
            last_progress_at: None,
            state: RuntimeState::Running,
            command_line: None,
            outputs: vec!["rtmp://127.0.0.1/live/a".to_string()],
            metadata: json!({
                "lease_token": "lease-live",
                "resolved_spec": {
                    "type": "stream_ingest",
                    "name": "live",
                    "input": {},
                    "stream": {},
                    "record": {"enabled": false},
                    "resource": {}
                }
            }),
        },
    ];

    manager.stop_active_artifact_tasks_for_volume(&observations, &handles, "threshold");

    let stops = executor.stops.lock().expect("stops lock");
    assert_eq!(stops.len(), 1);
    assert_eq!(stops[0].task_id, artifact_task);
    assert_eq!(stops[0].reason, DISK_THRESHOLD_STOP_REASON);
}
