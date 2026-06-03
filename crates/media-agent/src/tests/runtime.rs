use super::*;
use crate::runtime_stop::{StaleAttemptCleanupContext, cleanup_stale_attempt_runtimes};
use std::os::unix::process::ExitStatusExt;

fn test_settings(work_root: &str) -> AgentSettings {
    AgentSettings {
        http_addr: "127.0.0.1:8081".to_string(),
        node_id: String::new(),
        node_name: "node-1".to_string(),
        core_endpoint: "http://127.0.0.1:50051".to_string(),
        cert_path: String::new(),
        key_path: String::new(),
        ca_path: String::new(),
        tls_domain_name: String::new(),
        ffmpeg_bin: "ffmpeg".to_string(),
        ffprobe_bin: "ffprobe".to_string(),
        zlm_api_base: String::new(),
        zlm_rtmp_port: 1935,
        zlm_rtsp_port: 554,
        zlm_api_secret: String::new(),
        zlm_auto_close_on_no_reader_enabled: false,
        allow_enhanced_rtmp_expose: true,
        hls_record_segment_sec: 60,
        agent_stream_addr: "http://127.0.0.1:8081".to_string(),
        primary_interface_name: String::new(),
        primary_interface_ip: String::new(),
        output_mount_relative_prefix_mp4: "output".to_string(),
        output_mount_relative_prefix_hls: "output".to_string(),
        zlm_output_mp4_root: "/data/zlm/www/output/mp4".to_string(),
        zlm_output_hls_root: "/data/zlm/www/output/hls".to_string(),
        multicast_interface_name: String::new(),
        multicast_interface_ip: String::new(),
        network_mode: "bridge".to_string(),
        acceleration_mode: "cpu".to_string(),
        labels: Vec::new(),
        max_runtime_slots: 2,
        work_root: work_root.to_string(),
        upload_max_bytes: 1024 * 1024 * 1024,
        upload_allowed_extensions: vec!["mp4".to_string()],
        upload_probe_timeout_sec: 30,
        public_media_base_url: String::new(),
        artifact_cleanup: crate::config::AgentArtifactCleanupSettings::default(),
    }
}

fn build_stream_ingest_plan(
    settings: &AgentSettings,
    request: &StartTaskRequest,
    spec: &TaskSpec,
) -> Result<ProcessPlan, ExecutorError> {
    build_stream_ingest_plan_with_capability_hints(
        settings,
        request,
        spec,
        RuntimeCapabilityHints::default(),
    )
}

fn build_file_to_live_plan(
    settings: &AgentSettings,
    request: &StartTaskRequest,
    spec: &TaskSpec,
) -> Result<ProcessPlan, ExecutorError> {
    build_stream_ingest_realtime_plan(settings, request, spec, RuntimeCapabilityHints::default())
}

fn build_file_to_live_plan_with_capability_hints(
    settings: &AgentSettings,
    request: &StartTaskRequest,
    spec: &TaskSpec,
    capability_hints: RuntimeCapabilityHints,
) -> Result<ProcessPlan, ExecutorError> {
    build_stream_ingest_realtime_plan(settings, request, spec, capability_hints)
}

fn success_exit_status() -> std::process::ExitStatus {
    ExitStatusExt::from_raw(0)
}

fn continuous_stream_ingest_handle() -> RuntimeHandle {
    let resolved_spec = json!({
        "type": "stream_ingest",
        "name": "continuous-ingest",
        "common": {"created_by": "tester"},
        "input": {
            "kind": "http_mp4",
            "source_mode": "vod",
            "url": "http://vod.example.com/archive.mp4",
            "loop_enabled": true
        },
        "stream": {"app": "live", "name": "continuous-stream"},
        "process": {"mode": "copy_or_transcode"},
        "expose": {
            "enable_rtsp": true,
            "enable_rtmp": false,
            "enable_http_ts": false,
            "enable_http_fmp4": false,
            "enable_hls": false
        },
        "record": {"enabled": false},
        "recovery": {"policy": "auto"},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    });
    RuntimeHandle {
        runtime_id: Uuid::now_v7(),
        task_id: Uuid::now_v7(),
        attempt_no: 1,
        worker_kind: WorkerKind::Ffmpeg,
        pid: Some(1234),
        started_at: Utc::now(),
        last_progress_at: Some(Utc::now()),
        state: RuntimeState::Running,
        command_line: Some("ffmpeg -i input".to_string()),
        outputs: vec!["rtsp://127.0.0.1:554/live/continuous-stream".to_string()],
        metadata: json!({
            "task_type": "stream_ingest",
            "stream_online": true,
            "resolved_spec": resolved_spec,
        }),
    }
}

fn sticky_live_ingest_handle(stream_online: bool) -> RuntimeHandle {
    let resolved_spec = json!({
        "type": "stream_ingest",
        "name": "sticky-live-ingest",
        "common": {"created_by": "tester"},
        "input": {
            "kind": "udp_mpegts_multicast",
            "source_mode": "live",
            "url": "udp://239.0.0.1:1234"
        },
        "stream": {"app": "live", "name": "sticky-live"},
        "process": {"mode": "copy_or_transcode"},
        "record": {"enabled": false},
        "recovery": {"policy": "auto"},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    });
    RuntimeHandle {
        runtime_id: Uuid::now_v7(),
        task_id: Uuid::now_v7(),
        attempt_no: 1,
        worker_kind: WorkerKind::Ffmpeg,
        pid: Some(1234),
        started_at: Utc::now(),
        last_progress_at: stream_online.then(Utc::now),
        state: if stream_online {
            RuntimeState::Running
        } else {
            RuntimeState::Starting
        },
        command_line: Some("ffmpeg -i input".to_string()),
        outputs: vec!["rtsp://127.0.0.1:554/live/sticky-live".to_string()],
        metadata: json!({
            "task_type": "stream_ingest",
            "execution_mode": "managed",
            "lease_token": "lease",
            "stream_online": stream_online,
            "resolved_spec": resolved_spec,
        }),
    }
}

fn sticky_live_recording_ingest_handle(stream_online: bool) -> RuntimeHandle {
    let mut handle = sticky_live_ingest_handle(stream_online);
    handle.metadata["resolved_spec"]["record"] = json!({"enabled": true});
    handle
}

fn mock_audio_stream(
    index: Option<u32>,
    codec_name: &str,
    sample_rate: Option<u32>,
    channels: Option<u32>,
    extradata_size: Option<u64>,
) -> MockFfprobeAudioStream {
    MockFfprobeAudioStream {
        index,
        codec_name: codec_name.to_string(),
        sample_rate,
        channels,
        extradata_size,
    }
}

fn create_mock_ffprobe_binary(
    root: &Path,
    video_codec_name: &str,
    audio_codec_name: Option<&str>,
) -> String {
    create_mock_ffprobe_binary_with_profile(
        root,
        "mov,mp4,m4a,3gp,3g2,mj2",
        video_codec_name,
        audio_codec_name,
        Some(48_000),
        Some(2),
        Some(32),
        audio_codec_name.map(|_| 2),
    )
}

fn create_mock_ffprobe_binary_with_format(
    root: &Path,
    format_name: &str,
    video_codec_name: &str,
    audio_codec_name: Option<&str>,
) -> String {
    create_mock_ffprobe_binary_with_profile(
        root,
        format_name,
        video_codec_name,
        audio_codec_name,
        Some(48_000),
        Some(2),
        Some(32),
        audio_codec_name.map(|_| 2),
    )
}

fn create_mock_ffprobe_binary_with_profile(
    root: &Path,
    format_name: &str,
    video_codec_name: &str,
    audio_codec_name: Option<&str>,
    audio_sample_rate: Option<u32>,
    audio_channels: Option<u32>,
    video_extradata_size: Option<u64>,
    audio_extradata_size: Option<u64>,
) -> String {
    create_mock_ffprobe_binary_with_video_profile(
        root,
        format_name,
        video_codec_name,
        None,
        audio_codec_name,
        audio_sample_rate,
        audio_channels,
        video_extradata_size,
        audio_extradata_size,
    )
}

fn create_mock_ffprobe_binary_with_video_profile(
    _root: &Path,
    format_name: &str,
    video_codec_name: &str,
    video_pix_fmt: Option<&str>,
    audio_codec_name: Option<&str>,
    audio_sample_rate: Option<u32>,
    audio_channels: Option<u32>,
    video_extradata_size: Option<u64>,
    audio_extradata_size: Option<u64>,
) -> String {
    let audio_streams = audio_codec_name
        .map(|codec| {
            vec![mock_audio_stream(
                None,
                codec,
                audio_sample_rate,
                audio_channels,
                audio_extradata_size,
            )]
        })
        .unwrap_or_default();

    register_mock_ffprobe_binary(
        "single",
        MockFfprobeBinary {
            format_name: format_name.to_string(),
            video_codec_name: video_codec_name.to_string(),
            video_pix_fmt: video_pix_fmt.map(str::to_string),
            video_extradata_size,
            audio_streams,
            recorded_args_path: None,
            sleep_ms: 0,
        },
    )
}

fn create_mock_ffprobe_binary_with_audio_streams(
    _root: &Path,
    format_name: &str,
    video_codec_name: &str,
    audio_streams: &[(&str, Option<u32>, Option<u32>, Option<u64>)],
) -> String {
    let audio_streams = audio_streams
        .iter()
        .enumerate()
        .map(|(offset, (codec, sample_rate, channels, extradata_size))| {
            mock_audio_stream(
                Some((offset + 1) as u32),
                codec,
                *sample_rate,
                *channels,
                *extradata_size,
            )
        })
        .collect();

    register_mock_ffprobe_binary(
        "multi-audio",
        MockFfprobeBinary {
            format_name: format_name.to_string(),
            video_codec_name: video_codec_name.to_string(),
            video_pix_fmt: None,
            video_extradata_size: Some(32),
            audio_streams,
            recorded_args_path: None,
            sleep_ms: 0,
        },
    )
}

fn create_recording_mock_ffprobe_binary_with_profile(
    _root: &Path,
    recorded_args_path: &Path,
    format_name: &str,
    video_codec_name: &str,
    audio_codec_name: Option<&str>,
    audio_sample_rate: Option<u32>,
    audio_channels: Option<u32>,
    video_extradata_size: Option<u64>,
    audio_extradata_size: Option<u64>,
) -> String {
    let audio_streams = audio_codec_name
        .map(|codec| {
            vec![mock_audio_stream(
                None,
                codec,
                audio_sample_rate,
                audio_channels,
                audio_extradata_size,
            )]
        })
        .unwrap_or_default();

    register_mock_ffprobe_binary(
        "recording",
        MockFfprobeBinary {
            format_name: format_name.to_string(),
            video_codec_name: video_codec_name.to_string(),
            video_pix_fmt: None,
            video_extradata_size,
            audio_streams,
            recorded_args_path: Some(recorded_args_path.to_path_buf()),
            sleep_ms: 0,
        },
    )
}

fn create_slow_mock_ffprobe_binary(
    _root: &Path,
    sleep_ms: u64,
    video_codec_name: &str,
    audio_codec_name: Option<&str>,
) -> String {
    let audio_streams = audio_codec_name
        .map(|codec| {
            vec![mock_audio_stream(
                None,
                codec,
                Some(48_000),
                Some(2),
                Some(2),
            )]
        })
        .unwrap_or_default();

    register_mock_ffprobe_binary(
        "slow",
        MockFfprobeBinary {
            format_name: "mpegts".to_string(),
            video_codec_name: video_codec_name.to_string(),
            video_pix_fmt: None,
            video_extradata_size: Some(32),
            audio_streams,
            recorded_args_path: None,
            sleep_ms,
        },
    )
}

#[test]
fn registry_tracks_and_filters_snapshots() {
    let registry = LocalRuntimeRegistry::new();
    let handle = RuntimeHandle {
        runtime_id: Uuid::now_v7(),
        task_id: Uuid::now_v7(),
        attempt_no: 1,
        worker_kind: WorkerKind::Ffmpeg,
        pid: Some(1234),
        started_at: Utc::now(),
        last_progress_at: None,
        state: RuntimeState::Running,
        command_line: Some("ffmpeg -i input".to_string()),
        outputs: vec!["rtmp://output".to_string()],
        metadata: json!({ "source": "test", "lease_token": "lease-a" }),
    };
    registry.track(handle.clone());

    let snapshots = registry.snapshots(&AdoptFilter {
        session_epoch: 1,
        runtimes: vec![AdoptRuntimeFilter {
            task_id: handle.task_id,
            attempt_no: handle.attempt_no,
            lease_token: "lease-a".to_string(),
            worker_kind: WorkerKind::Ffmpeg,
        }],
    });

    assert_eq!(snapshots, vec![handle]);
}

#[test]
fn registry_replaces_duplicate_task_attempt_and_reports_state_counts() {
    let registry = LocalRuntimeRegistry::new();
    let task_id = Uuid::now_v7();
    let replacement = RuntimeHandle {
        runtime_id: Uuid::now_v7(),
        task_id,
        attempt_no: 1,
        worker_kind: WorkerKind::Ffmpeg,
        pid: Some(1002),
        started_at: Utc::now(),
        last_progress_at: None,
        state: RuntimeState::Starting,
        command_line: None,
        outputs: Vec::new(),
        metadata: json!({ "lease_token": "lease-a" }),
    };
    registry.track(RuntimeHandle {
        runtime_id: Uuid::now_v7(),
        task_id,
        attempt_no: 1,
        worker_kind: WorkerKind::Ffmpeg,
        pid: Some(1001),
        started_at: Utc::now(),
        last_progress_at: None,
        state: RuntimeState::Running,
        command_line: None,
        outputs: Vec::new(),
        metadata: json!({ "lease_token": "lease-a" }),
    });
    registry.track(replacement.clone());
    registry.track(RuntimeHandle {
        runtime_id: Uuid::now_v7(),
        task_id: Uuid::now_v7(),
        attempt_no: 1,
        worker_kind: WorkerKind::ZlmProxy,
        pid: None,
        started_at: Utc::now(),
        last_progress_at: None,
        state: RuntimeState::Orphaned,
        command_line: None,
        outputs: Vec::new(),
        metadata: json!({ "lease_token": "lease-b" }),
    });

    assert_eq!(registry.count(), 2);
    assert_eq!(
        registry.find_by_task_attempt(task_id, 1),
        Some(replacement.clone())
    );

    let counts = registry.state_counts();
    assert_eq!(counts.starting, 1);
    assert_eq!(counts.running, 0);
    assert_eq!(counts.stopping, 0);
    assert_eq!(counts.orphaned, 1);
}

#[test]
fn build_file_transcode_plan_allocates_managed_output_path() {
    let settings = test_settings("/tmp/work");
    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::FileTranscode,
        resolved_spec: json!({
            "type": "file_transcode",
            "name": "test",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "input.mp4"},
            "process": {"mode": "copy_or_transcode"},
            "record": {},
            "publish": {
                "kind": "file"
            },
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_file_transcode_plan(&settings, &request, &spec).expect("plan should build");
    assert_eq!(plan.executable, "ffmpeg");
    assert!(plan.args.iter().any(|arg| arg == "pipe:1"));
    assert!(
        plan.output_target.starts_with(
            managed_output_dir(&settings, request.task_id, "mp4")
                .to_string_lossy()
                .as_ref()
        )
    );
    assert!(plan.output_target.ends_with(".mp4"));
}

#[test]
fn build_file_transcode_plan_hls_uses_configured_archive_segments() {
    let mut settings = test_settings("/tmp/work");
    settings.hls_record_segment_sec = 30;
    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::FileTranscode,
        resolved_spec: json!({
            "type": "file_transcode",
            "name": "test-hls",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "input.mp4"},
            "process": {"mode": "copy_or_transcode"},
            "record": {},
            "publish": {
                "kind": "file",
                "format": "hls"
            },
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_file_transcode_plan(&settings, &request, &spec).expect("plan should build");

    assert!(plan.output_target.ends_with(".m3u8"));
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-hls_time", "30"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-hls_list_size", "0"])
    );
    assert!(plan.args.iter().any(|arg| arg.ends_with("-%05d.ts")));
}

#[test]
fn managed_output_dir_uses_primary_interface_ip_and_bucket_layout() {
    let mut settings = test_settings("/tmp/work");
    settings.primary_interface_ip = "172.17.13.196".to_string();
    let task_id = Uuid::nil();

    assert_eq!(
        managed_output_dir(&settings, task_id, "mp4"),
        PathBuf::from("/data/zlm/www/output/mp4")
            .join("node-172_17_13_196-mp4")
            .join(task_id.to_string())
    );
    assert_eq!(
        managed_output_dir(&settings, task_id, "hls"),
        PathBuf::from("/data/zlm/www/output/hls")
            .join("node-172_17_13_196-hls")
            .join(task_id.to_string())
    );
}

#[test]
fn managed_output_dir_falls_back_to_stream_addr_ip() {
    let mut settings = test_settings("/tmp/work");
    settings.agent_stream_addr = "http://10.20.30.40:8081".to_string();
    let task_id = Uuid::nil();

    assert_eq!(
        managed_output_dir(&settings, task_id, "mp4"),
        PathBuf::from("/data/zlm/www/output/mp4")
            .join("node-10_20_30_40-mp4")
            .join(task_id.to_string())
    );
}

#[test]
fn managed_output_dir_uses_configured_output_roots() {
    let mut settings = test_settings("/tmp/work");
    settings.primary_interface_ip = "172.17.13.196".to_string();
    settings.zlm_output_mp4_root = "/opt/streamserver/worker/data/zlm/www/output/mp4".to_string();
    settings.zlm_output_hls_root = "/opt/streamserver/worker/data/zlm/www/output/hls".to_string();
    let task_id = Uuid::nil();

    assert_eq!(
        managed_output_dir(&settings, task_id, "mp4"),
        PathBuf::from("/opt/streamserver/worker/data/zlm/www/output/mp4")
            .join("node-172_17_13_196-mp4")
            .join(task_id.to_string())
    );
    assert_eq!(
        managed_output_dir(&settings, task_id, "hls"),
        PathBuf::from("/opt/streamserver/worker/data/zlm/www/output/hls")
            .join("node-172_17_13_196-hls")
            .join(task_id.to_string())
    );
}

#[test]
fn build_file_transcode_plan_copy_or_transcode_copies_hevc_aac_when_mp4_allows_it() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-copy-transcode-hevc-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin = create_mock_ffprobe_binary(&temp_root, "hevc", Some("aac"));

    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::FileTranscode,
        resolved_spec: json!({
            "type": "file_transcode",
            "name": "test-copy-hevc",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "input.mp4"},
            "process": {"mode": "copy_or_transcode"},
            "record": {},
            "publish": {
                "kind": "file",
                "format": "mp4"
            },
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_file_transcode_plan(&settings, &request, &spec).expect("plan should build");

    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:v", "copy"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:a", "copy"])
    );
    assert!(
        !plan
            .args
            .windows(2)
            .any(|window| window == ["-c:v", "libx264"])
    );
    assert!(!plan.args.windows(2).any(|window| window == ["-c:a", "aac"]));

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_file_transcode_plan_copy_or_transcode_copies_mpegts_aac_for_mp4_with_bsf() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-copy-transcode-mpegts-aac-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin =
        create_mock_ffprobe_binary_with_format(&temp_root, "mpegts", "h264", Some("aac"));

    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::FileTranscode,
        resolved_spec: json!({
            "type": "file_transcode",
            "name": "test-mpegts-aac-to-mp4",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "input.ts"},
            "process": {"mode": "copy_or_transcode"},
            "record": {},
            "publish": {
                "kind": "file",
                "format": "mp4"
            },
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_file_transcode_plan(&settings, &request, &spec).expect("plan should build");

    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:v", "copy"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:a", "copy"])
    );
    assert!(!plan.args.windows(2).any(|window| window == ["-c:a", "aac"]));
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-bsf:a", "aac_adtstoasc"])
    );

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_file_transcode_plan_maps_only_aac_audio_for_multi_audio_mp4() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-copy-transcode-multi-audio-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin = create_mock_ffprobe_binary_with_audio_streams(
        &temp_root,
        "mpegts",
        "hevc",
        &[
            ("mp3", Some(22_050), Some(2), Some(2)),
            ("truehd", Some(48_000), Some(6), Some(2)),
            ("dts", Some(48_000), Some(6), Some(2)),
            ("eac3", Some(48_000), Some(6), Some(2)),
            ("aac", Some(48_000), Some(2), Some(2)),
        ],
    );

    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::FileTranscode,
        resolved_spec: json!({
            "type": "file_transcode",
            "name": "test-multi-audio-to-mp4",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "input.ts"},
            "process": {"mode": "copy_or_transcode"},
            "record": {},
            "publish": {
                "kind": "file",
                "format": "mp4"
            },
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_file_transcode_plan(&settings, &request, &spec).expect("plan should build");

    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-map", "0:v?"])
    );
    assert!(plan.args.windows(2).any(|window| window == ["-map", "0:5"]));
    assert!(
        !plan
            .args
            .windows(2)
            .any(|window| window == ["-map", "0:a?"])
    );
    assert!(!plan.args.windows(2).any(|window| window == ["-map", "0:2"]));
    assert!(!plan.args.windows(2).any(|window| window == ["-map", "0:3"]));
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:a", "copy"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-bsf:a", "aac_adtstoasc"])
    );

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_file_transcode_plan_transcodes_first_audio_when_mp4_has_no_copy_safe_audio() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-copy-transcode-truehd-dts-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin = create_mock_ffprobe_binary_with_audio_streams(
        &temp_root,
        "mpegts",
        "hevc",
        &[
            ("truehd", Some(48_000), Some(6), Some(2)),
            ("dts", Some(48_000), Some(6), Some(2)),
        ],
    );

    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::FileTranscode,
        resolved_spec: json!({
            "type": "file_transcode",
            "name": "test-truehd-dts-to-mp4",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "input.ts"},
            "process": {"mode": "copy_or_transcode"},
            "record": {},
            "publish": {
                "kind": "file",
                "format": "mp4"
            },
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_file_transcode_plan(&settings, &request, &spec).expect("plan should build");

    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-map", "0:v?"])
    );
    assert!(plan.args.windows(2).any(|window| window == ["-map", "0:1"]));
    assert!(
        !plan
            .args
            .windows(2)
            .any(|window| window == ["-map", "0:a?"])
    );
    assert!(plan.args.windows(2).any(|window| window == ["-c:a", "aac"]));

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn resolve_video_families_keeps_hevc_input_probe_for_force_h264() {
    let temp_root = std::env::temp_dir().join(format!("streamserver-gpu-probe-{}", Uuid::now_v7()));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin = create_mock_ffprobe_binary(&temp_root, "hevc", None);

    let (input_family, output_family) = resolve_video_families(
        &settings,
        "/tmp/input.mp4",
        Some(DEFAULT_INPUT_PROBE_TIMEOUT_MS),
        VideoOutputPolicy::ForceH264,
    );

    assert_eq!(input_family, VideoCodecFamily::Hevc);
    assert_eq!(output_family, VideoCodecFamily::H264);

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn probe_input_media_profile_reads_video_and_audio_codecs() {
    let temp_root =
        std::env::temp_dir().join(format!("streamserver-media-profile-{}", Uuid::now_v7()));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin = create_mock_ffprobe_binary(&temp_root, "h264", Some("aac"));
    let spec: TaskSpec = serde_json::from_value(json!({
        "type": "file_transcode",
        "name": "probe-profile",
        "common": {"created_by": "tester"},
        "input": {"kind": "file", "url": "input.mp4"},
        "process": {"mode": "copy_or_transcode"},
        "publish": {"kind": "file"},
        "record": {},
        "recovery": {},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    }))
    .expect("spec should parse");

    let profile = probe_input_media_profile(&settings, &spec, "/tmp/input.mp4");

    assert!(profile.has_video);
    assert_eq!(profile.video_family, VideoCodecFamily::H264);
    assert_eq!(profile.video_codec_name.as_deref(), Some("h264"));
    assert_eq!(profile.video_pixel_format, None);
    assert!(profile.video_extradata_present);
    assert!(profile.has_audio);
    assert_eq!(profile.audio_codec_name.as_deref(), Some("aac"));
    assert_eq!(profile.audio_sample_rate, Some(48_000));
    assert_eq!(profile.audio_channels, Some(2));
    assert!(profile.audio_extradata_present);
    assert_eq!(profile.source_family, InputSourceFamily::Mp4Mov);

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn probe_input_media_profile_reads_all_audio_streams() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-media-profile-audio-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin = create_mock_ffprobe_binary_with_audio_streams(
        &temp_root,
        "mpegts",
        "hevc",
        &[
            ("mp3", Some(22_050), Some(2), Some(2)),
            ("truehd", Some(48_000), Some(6), Some(2)),
            ("dts", Some(48_000), Some(6), Some(2)),
            ("eac3", Some(48_000), Some(6), Some(2)),
            ("aac", Some(48_000), Some(2), Some(2)),
        ],
    );
    let spec: TaskSpec = serde_json::from_value(json!({
        "type": "stream_ingest",
        "name": "probe-all-audio",
        "common": {"created_by": "tester"},
        "input": {"kind": "file", "url": "input.ts"},
        "process": {"mode": "copy_or_transcode"},
        "stream": {"app": "live", "name": "probe-all-audio"},
        "record": {},
        "recovery": {},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    }))
    .expect("spec should parse");

    let profile = probe_input_media_profile(&settings, &spec, "/tmp/input.ts");

    assert_eq!(profile.audio_codec_name.as_deref(), Some("mp3"));
    assert_eq!(profile.audio_streams.len(), 5);
    assert_eq!(profile.audio_streams[0].index, 1);
    assert_eq!(profile.audio_streams[0].codec_name.as_deref(), Some("mp3"));
    assert_eq!(profile.audio_streams[4].index, 5);
    assert_eq!(profile.audio_streams[4].codec_name.as_deref(), Some("aac"));
    assert_eq!(profile.audio_streams[4].sample_rate, Some(48_000));
    assert_eq!(profile.source_family, InputSourceFamily::MpegTs);

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn probe_input_media_profile_reads_video_pixel_format() {
    let temp_root =
        std::env::temp_dir().join(format!("streamserver-media-pix-fmt-{}", Uuid::now_v7()));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin = create_mock_ffprobe_binary_with_video_profile(
        &temp_root,
        "mpegts",
        "hevc",
        Some("yuv420p10le"),
        Some("aac"),
        Some(48_000),
        Some(2),
        Some(32),
        Some(2),
    );
    let spec: TaskSpec = serde_json::from_value(json!({
        "type": "stream_ingest",
        "name": "probe-pix-fmt",
        "common": {"created_by": "tester"},
        "input": {"kind": "file", "url": "input.ts"},
        "process": {"mode": "copy_or_transcode"},
        "stream": {"app": "live", "name": "probe-pix-fmt"},
        "record": {},
        "recovery": {},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    }))
    .expect("spec should parse");

    let profile = probe_input_media_profile(&settings, &spec, "/tmp/input.ts");

    assert_eq!(profile.video_codec_name.as_deref(), Some("hevc"));
    assert_eq!(profile.video_pixel_format.as_deref(), Some("yuv420p10le"));
    assert_eq!(profile.source_family, InputSourceFamily::MpegTs);

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn probe_input_media_profile_times_out_and_returns_default_profile() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-media-profile-timeout-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin = create_slow_mock_ffprobe_binary(&temp_root, 500, "h264", Some("aac"));
    let spec: TaskSpec = serde_json::from_value(json!({
        "type": "stream_ingest",
        "name": "probe-timeout",
        "common": {"created_by": "tester"},
        "input": {
            "kind": "udp_mpegts_multicast",
            "url": "udp://@231.40.1.101:5001",
            "probe_timeout_ms": 100
        },
        "process": {"mode": "copy_or_transcode"},
        "stream": {"app": "live", "name": "probe-timeout", "vhost": "__defaultVhost__"},
        "publish": {},
        "record": {},
        "expose": {},
        "recovery": {},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    }))
    .expect("spec should parse");

    let started = Instant::now();
    let profile = probe_input_media_profile(&settings, &spec, "udp://@231.40.1.101:5001");

    assert!(started.elapsed() < Duration::from_millis(400));
    assert_eq!(profile.source_family, InputSourceFamily::MpegTs);
    assert!(!profile.has_video);
    assert!(!profile.has_audio);
    assert_eq!(profile.video_family, VideoCodecFamily::Unknown);
    assert!(!profile.video_extradata_present);
    assert!(!profile.audio_extradata_present);

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn resolve_video_families_times_out_to_unknown_input_family() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-video-family-timeout-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin = create_slow_mock_ffprobe_binary(&temp_root, 500, "hevc", None);

    let started = Instant::now();
    let (input_family, output_family) = resolve_video_families(
        &settings,
        "udp://@231.40.1.101:5001",
        Some(100),
        VideoOutputPolicy::ForceH264,
    );

    assert!(started.elapsed() < Duration::from_millis(400));
    assert_eq!(input_family, VideoCodecFamily::Unknown);
    assert_eq!(output_family, VideoCodecFamily::H264);

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn resolve_transcode_selection_uses_node_gpu_policy_without_decoder_probe() {
    let mut settings = test_settings("/tmp/work");
    settings.acceleration_mode = "gpu".to_string();

    let selection = resolve_transcode_selection_for_input_family(
        &settings,
        VideoCodecFamily::Hevc,
        VideoOutputPolicy::KeepSourceFamily,
        AudioOutputPolicy::CopyWhitelistedElseAac,
    );

    assert!(selection.input_args.is_empty());
    assert_eq!(selection.video_encoder, "hevc_nvenc");
    assert_eq!(selection.audio_encoder, "aac");
}

#[test]
fn build_file_to_live_plan_force_transcode_hevc_10bit_uses_h264_nvenc_yuv420p_compatibility() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-file-live-hevc10-h264-nvenc-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.acceleration_mode = "gpu".to_string();
    settings.ffprobe_bin = create_mock_ffprobe_binary_with_video_profile(
        &temp_root,
        "mpegts",
        "hevc",
        Some("yuv420p10le"),
        Some("aac"),
        Some(48_000),
        Some(2),
        Some(32),
        Some(2),
    );

    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "file-live-hevc10-force-h264",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "input.ts", "source_mode": "vod"},
            "stream": {"app": "live", "name": "stream"},
            "process": {"mode": "transcode", "bitrate": 6000, "fps": 25, "gop": 50},
            "record": {},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_file_to_live_plan_with_capability_hints(
        &settings,
        &request,
        &spec,
        RuntimeCapabilityHints {
            zlm_rtmp_enhanced_enabled: Some(true),
        },
    )
    .expect("plan should build");

    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:v", "h264_nvenc"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-vf", "format=yuv420p"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-pix_fmt", "yuv420p"])
    );

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_file_transcode_plan_rejects_publish_url_override() {
    let settings = test_settings("/tmp/work");
    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::FileTranscode,
        resolved_spec: json!({
            "type": "file_transcode",
            "name": "test",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "input.mp4"},
            "process": {"mode": "copy_or_transcode"},
            "record": {},
            "publish": {
                "kind": "file",
                "url": "/tmp/output.mp4"
            },
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let error = build_file_transcode_plan(&settings, &request, &spec)
        .expect_err("plan should reject publish url override");
    assert!(matches!(
        error,
        ExecutorError::InvalidRequest(message)
            if message.contains("publish.url must not be provided")
    ));
}

#[test]
fn build_multicast_bridge_plan_allocates_managed_file_output_path() {
    let settings = test_settings("/tmp/work");
    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamBridge,
        resolved_spec: json!({
            "type": "stream_bridge",
            "name": "bridge-test",
            "common": {"created_by": "tester"},
            "input": {"kind": "rtsp", "source_mode": "live", "url": "rtsp://example.com/live"},
            "process": {"mode": "passthrough"},
            "publish": {
                "kind": "file"
            },
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_multicast_bridge_plan(&settings, &request, &spec).expect("plan should build");

    assert!(
        plan.output_target.starts_with(
            managed_output_dir(&settings, request.task_id, "mp4")
                .to_string_lossy()
                .as_ref()
        )
    );
    assert!(plan.output_target.ends_with(".mp4"));
    assert!(plan.args.iter().any(|arg| arg == "mp4"));
}

#[test]
fn start_task_rejects_when_max_runtime_slots_are_exhausted() {
    let temp_root =
        std::env::temp_dir().join(format!("streamserver-runtime-slots-{}", Uuid::now_v7()));
    let registry = LocalRuntimeRegistry::new();
    let existing_handle = RuntimeHandle {
        runtime_id: Uuid::now_v7(),
        task_id: Uuid::now_v7(),
        attempt_no: 1,
        worker_kind: WorkerKind::Ffmpeg,
        pid: Some(1234),
        started_at: Utc::now(),
        last_progress_at: None,
        state: RuntimeState::Running,
        command_line: Some("ffmpeg -i input".to_string()),
        outputs: vec!["/data/zlm/www/artifacts/transcode/output.mp4".to_string()],
        metadata: json!({"task_type": "file_transcode"}),
    };
    registry.track(existing_handle.clone());

    let (priority_tx, _priority_rx) = mpsc::unbounded_channel();
    let (log_tx, _log_rx) = mpsc::channel(8);
    let mut settings = test_settings(temp_root.to_string_lossy().as_ref());
    settings.max_runtime_slots = 1;
    settings.ffmpeg_bin = "/definitely/missing-ffmpeg".to_string();
    let executor = ManagedProcessExecutor::new(
        settings,
        registry,
        RuntimeEventSink::new(priority_tx, log_tx),
    );
    executor
        .runtimes
        .write()
        .expect("runtime map lock poisoned")
        .insert(
            existing_handle.runtime_id,
            ManagedRuntime {
                pid: existing_handle.pid,
                companion_pids: Vec::new(),
                _slot_permit: executor.slot_limiter.attach_existing(),
                stop_requested: Arc::new(AtomicBool::new(false)),
                suppress_companion_events: Arc::new(AtomicBool::new(false)),
            },
        );
    let request = StartTaskRequest {
        task_id: Uuid::now_v7(),
        attempt_no: 1,
        task_type: TaskType::FileTranscode,
        resolved_spec: json!({
            "type": "file_transcode",
            "name": "test",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "input.mp4"},
            "process": {"mode": "copy_or_transcode"},
            "record": {},
            "publish": {
                "kind": "file",
                "url": "/data/zlm/www/artifacts/transcode/output.mp4"
            },
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let error = executor
        .start_task(&request)
        .expect_err("exhausted slots should reject the task before spawn");
    assert!(matches!(
        error,
        ExecutorError::InvalidRequest(message) if message.contains("max_runtime_slots")
    ));

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn start_task_releases_slot_after_spawn_failure() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-runtime-slot-release-{}",
        Uuid::now_v7()
    ));
    let registry = LocalRuntimeRegistry::new();
    let (priority_tx, _priority_rx) = mpsc::unbounded_channel();
    let (log_tx, _log_rx) = mpsc::channel(8);
    let mut settings = test_settings(temp_root.to_string_lossy().as_ref());
    settings.max_runtime_slots = 1;
    settings.ffmpeg_bin = "/definitely/missing-ffmpeg".to_string();
    let executor = ManagedProcessExecutor::new(
        settings,
        registry,
        RuntimeEventSink::new(priority_tx, log_tx),
    );
    let request = StartTaskRequest {
        task_id: Uuid::now_v7(),
        attempt_no: 1,
        task_type: TaskType::FileTranscode,
        resolved_spec: json!({
            "type": "file_transcode",
            "name": "slot-release",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "input.mp4"},
            "process": {"mode": "copy_or_transcode"},
            "record": {},
            "publish": {
                "kind": "file",
                "url": "/data/zlm/www/artifacts/transcode/output.mp4"
            },
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let first = executor
        .start_task(&request)
        .expect_err("spawn failure should bubble up");
    assert!(!matches!(
        first,
        ExecutorError::InvalidRequest(message) if message.contains("max_runtime_slots")
    ));

    let second = executor
        .start_task(&request)
        .expect_err("slot should be released after failed spawn");
    assert!(!matches!(
        second,
        ExecutorError::InvalidRequest(message) if message.contains("max_runtime_slots")
    ));

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn start_task_is_idempotent_for_same_attempt_and_lease() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-runtime-idempotent-{}",
        Uuid::now_v7()
    ));
    let registry = LocalRuntimeRegistry::new();
    let task_id = Uuid::now_v7();
    let existing = RuntimeHandle {
        runtime_id: Uuid::now_v7(),
        task_id,
        attempt_no: 2,
        worker_kind: WorkerKind::Ffmpeg,
        pid: Some(1234),
        started_at: Utc::now(),
        last_progress_at: None,
        state: RuntimeState::Running,
        command_line: Some("ffmpeg -i input".to_string()),
        outputs: vec!["/data/zlm/www/artifacts/transcode/output.mp4".to_string()],
        metadata: json!({
            "task_type": "file_transcode",
            "lease_token": "lease-idempotent"
        }),
    };
    registry.track(existing.clone());

    let (priority_tx, _priority_rx) = mpsc::unbounded_channel();
    let (log_tx, _log_rx) = mpsc::channel(8);
    let executor = ManagedProcessExecutor::new(
        test_settings(temp_root.to_string_lossy().as_ref()),
        registry,
        RuntimeEventSink::new(priority_tx, log_tx),
    );
    let request = StartTaskRequest {
        task_id,
        attempt_no: 2,
        task_type: TaskType::FileTranscode,
        resolved_spec: json!({
            "type": "file_transcode",
            "name": "test-idempotent",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "input.mp4"},
            "process": {"mode": "copy_or_transcode"},
            "record": {},
            "publish": {"kind": "file"},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease-idempotent".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let returned = executor
        .start_task(&request)
        .expect("same attempt and lease should reuse existing handle");
    assert_eq!(returned, existing);
}

#[test]
fn build_multicast_bridge_plan_renders_multicast_input_and_output() {
    let settings = test_settings("/tmp/work");
    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamBridge,
        resolved_spec: json!({
            "type": "stream_bridge",
            "name": "bridge",
            "common": {"created_by": "tester"},
            "input": {
                "kind": "udp_mpegts_multicast",
                "group": "239.10.10.10",
                "port": 5000,
                "interface_ip": "192.168.1.10",
                "ttl": 2,
                "reuse": true,
                "pkt_size": 1316
            },
            "process": {"mode": "passthrough"},
            "publish": {
                "kind": "udp_mpegts_multicast",
                "group": "239.20.20.20",
                "port": 6000,
                "interface_ip": "192.168.1.20",
                "ttl": 4,
                "reuse": true,
                "pkt_size": 1316
            },
            "record": {},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_multicast_bridge_plan(&settings, &request, &spec).expect("plan should build");

    assert_eq!(plan.executable, "ffmpeg");
    assert_eq!(
        plan.output_target,
        "udp://239.20.20.20:6000?localaddr=192.168.1.20&reuse=1&ttl=4&pkt_size=1316"
    );
    assert!(
        plan.args.iter().any(|arg| arg
            == "udp://239.10.10.10:5000?localaddr=192.168.1.10&reuse=1&ttl=2&pkt_size=1316")
    );
    assert!(
        plan.args.iter().any(|arg| arg
            == "udp://239.20.20.20:6000?localaddr=192.168.1.20&reuse=1&ttl=4&pkt_size=1316")
    );
    let fflags_index = plan
        .args
        .iter()
        .position(|arg| arg == "-fflags")
        .expect("multicast bridge should inject ffmpeg input flags");
    let wallclock_index = plan
        .args
        .iter()
        .position(|arg| arg == "-use_wallclock_as_timestamps")
        .expect("multicast bridge should inject wallclock timestamping");
    let input_index = plan
        .args
        .iter()
        .position(|arg| arg == "-i")
        .expect("ffmpeg args should contain input marker");
    assert!(wallclock_index < input_index);
    assert!(fflags_index < input_index);
    assert_eq!(
        plan.args.get(wallclock_index + 1).map(String::as_str),
        Some("1")
    );
    assert_eq!(
        plan.args.get(fflags_index + 1).map(String::as_str),
        Some("+genpts")
    );
}

#[test]
fn build_multicast_bridge_plan_stabilizes_live_mpegts_multicast_passthrough() {
    let settings = test_settings("/tmp/work");
    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamBridge,
        resolved_spec: json!({
            "type": "stream_bridge",
            "name": "bridge-live-to-mcast",
            "common": {"created_by": "tester"},
            "input": {
                "kind": "rtsp",
                "url": "rtsp://camera.example/live"
            },
            "process": {"mode": "passthrough"},
            "publish": {
                "kind": "udp_mpegts_multicast",
                "group": "239.20.20.20",
                "port": 6000,
                "interface_ip": "192.168.1.20",
                "ttl": 4,
                "reuse": true,
                "pkt_size": 1316
            },
            "record": {},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_multicast_bridge_plan(&settings, &request, &spec).expect("plan should build");

    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:v", "libx264"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:a", "copy"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-preset", "ultrafast"])
    );
    assert!(plan.args.windows(2).any(|window| window == ["-g", "24"]));
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-sc_threshold", "0"])
    );
}

#[test]
fn build_multicast_bridge_plan_copy_or_transcode_keeps_video_transcode_for_live_mpegts_multicast() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-bridge-mpegts-stable-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin = create_mock_ffprobe_binary(&temp_root, "h264", Some("aac"));

    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamBridge,
        resolved_spec: json!({
            "type": "stream_bridge",
            "name": "bridge-live-to-mcast-copy-or-transcode",
            "common": {"created_by": "tester"},
            "input": {
                "kind": "rtsp",
                "source_mode": "live",
                "url": "rtsp://camera.example/live"
            },
            "process": {"mode": "copy_or_transcode"},
            "publish": {
                "kind": "udp_mpegts_multicast",
                "group": "239.20.20.20",
                "port": 6000,
                "interface_ip": "192.168.1.20",
                "ttl": 4,
                "reuse": true,
                "pkt_size": 1316
            },
            "record": {},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_multicast_bridge_plan(&settings, &request, &spec).expect("plan should build");

    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:v", "libx264"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:a", "copy"])
    );

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_multicast_bridge_plan_pushes_live_input_to_external_rtmp_without_realtime_pacing() {
    let settings = test_settings("/tmp/work");
    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamBridge,
        resolved_spec: json!({
            "type": "stream_bridge",
            "name": "bridge-to-rtmp",
            "common": {"created_by": "tester"},
            "input": {
                "kind": "rtsp",
                "source_mode": "live",
                "url": "rtsp://camera.example/live"
            },
            "process": {"mode": "passthrough"},
            "publish": {
                "kind": "rtmp_push",
                "url": "rtmp://push.example.com/live/bridge-ingest"
            },
            "record": {},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_multicast_bridge_plan(&settings, &request, &spec).expect("plan should build");

    assert_eq!(
        plan.output_target,
        "rtmp://push.example.com/live/bridge-ingest"
    );
    assert!(plan.startup_probe.is_none());
    assert_eq!(
        plan.args
            .windows(2)
            .find(|window| *window == ["-f", "flv"])
            .map(|_| "flv"),
        Some("flv")
    );
    assert!(!plan.args.iter().any(|arg| arg == "-re"));
    let wallclock_index = plan
        .args
        .iter()
        .position(|arg| arg == "-use_wallclock_as_timestamps")
        .expect("live bridge should stabilize timestamps");
    assert_eq!(
        plan.args.get(wallclock_index + 1).map(String::as_str),
        Some("1")
    );
    assert!(plan.args.iter().any(|arg| arg == "+genpts"));
}

#[test]
fn build_multicast_bridge_plan_copy_or_transcode_copies_h264_aac_to_external_rtmp() {
    let temp_root =
        std::env::temp_dir().join(format!("streamserver-bridge-copy-rtmp-{}", Uuid::now_v7()));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin = create_mock_ffprobe_binary(&temp_root, "h264", Some("aac"));

    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamBridge,
        resolved_spec: json!({
            "type": "stream_bridge",
            "name": "bridge-copy-to-rtmp",
            "common": {"created_by": "tester"},
            "input": {
                "kind": "rtsp",
                "source_mode": "live",
                "url": "rtsp://camera.example/live"
            },
            "process": {"mode": "copy_or_transcode"},
            "publish": {
                "kind": "rtmp_push",
                "url": "rtmp://push.example.com/live/bridge-copy"
            },
            "record": {},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_multicast_bridge_plan(&settings, &request, &spec).expect("plan should build");

    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:v", "copy"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:a", "copy"])
    );
    assert!(
        !plan
            .args
            .windows(2)
            .any(|window| window == ["-c:v", "libx264"])
    );

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_multicast_bridge_plan_copy_or_transcode_transcodes_hevc_for_external_rtmp() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-bridge-copy-hevc-rtmp-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin = create_mock_ffprobe_binary(&temp_root, "hevc", Some("aac"));

    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamBridge,
        resolved_spec: json!({
            "type": "stream_bridge",
            "name": "bridge-copy-hevc-to-rtmp",
            "common": {"created_by": "tester"},
            "input": {
                "kind": "rtsp",
                "source_mode": "live",
                "url": "rtsp://camera.example/live"
            },
            "process": {"mode": "copy_or_transcode"},
            "publish": {
                "kind": "rtmp_push",
                "url": "rtmp://push.example.com/live/bridge-copy-hevc"
            },
            "record": {},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_multicast_bridge_plan(&settings, &request, &spec).expect("plan should build");

    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:v", "libx264"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:a", "copy"])
    );

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_multicast_bridge_plan_copy_or_transcode_copies_hls_aac_to_external_rtmp_with_bsf() {
    let temp_root =
        std::env::temp_dir().join(format!("streamserver-bridge-hls-aac-{}", Uuid::now_v7()));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin =
        create_mock_ffprobe_binary_with_format(&temp_root, "hls", "h264", Some("aac"));

    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamBridge,
        resolved_spec: json!({
            "type": "stream_bridge",
            "name": "bridge-hls-to-rtmp",
            "common": {"created_by": "tester"},
            "input": {
                "kind": "hls",
                "source_mode": "live",
                "url": "http://vod.example.com/archive.m3u8"
            },
            "process": {"mode": "copy_or_transcode"},
            "publish": {
                "kind": "rtmp_push",
                "url": "rtmp://push.example.com/live/bridge-hls"
            },
            "record": {},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_multicast_bridge_plan(&settings, &request, &spec).expect("plan should build");

    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:v", "copy"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:a", "copy"])
    );
    assert!(!plan.args.windows(2).any(|window| window == ["-c:a", "aac"]));
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-bsf:a", "aac_adtstoasc"])
    );

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_multicast_bridge_plan_passthrough_copies_hls_aac_to_external_rtmp_with_bsf() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-bridge-passthrough-hls-aac-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin =
        create_mock_ffprobe_binary_with_format(&temp_root, "hls", "h264", Some("aac"));

    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamBridge,
        resolved_spec: json!({
            "type": "stream_bridge",
            "name": "bridge-passthrough-hls-to-rtmp",
            "common": {"created_by": "tester"},
            "input": {
                "kind": "hls",
                "source_mode": "live",
                "url": "http://vod.example.com/archive.m3u8"
            },
            "process": {"mode": "passthrough"},
            "publish": {
                "kind": "rtmp_push",
                "url": "rtmp://push.example.com/live/bridge-hls-pass"
            },
            "record": {},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_multicast_bridge_plan(&settings, &request, &spec).expect("plan should build");

    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:v", "copy"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:a", "copy"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-bsf:a", "aac_adtstoasc"])
    );

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_multicast_bridge_plan_pushes_vod_input_to_external_rtmp_with_realtime_pacing() {
    let settings = test_settings("/tmp/work");
    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamBridge,
        resolved_spec: json!({
            "type": "stream_bridge",
            "name": "bridge-vod-to-rtmp",
            "common": {"created_by": "tester"},
            "input": {
                "kind": "http_mp4",
                "source_mode": "vod",
                "url": "http://vod.example.com/archive.mp4"
            },
            "process": {"mode": "passthrough"},
            "publish": {
                "kind": "rtmp_push",
                "url": "rtmps://push.example.com/live/bridge-default"
            },
            "record": {},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_multicast_bridge_plan(&settings, &request, &spec).expect("plan should build");

    assert_eq!(
        plan.output_target,
        "rtmps://push.example.com/live/bridge-default"
    );
    assert!(plan.args.iter().any(|arg| arg == "-re"));
    assert!(plan.args.windows(2).any(|window| window == ["-f", "flv"]));
    assert!(
        !plan
            .args
            .iter()
            .any(|arg| arg == "-use_wallclock_as_timestamps")
    );
    assert!(!plan.args.iter().any(|arg| arg == "+genpts"));
}

#[test]
fn resolve_interface_binding_ip_resolves_explicit_interface_name() {
    let Some(interface_name) = first_ipv4_interface_name_for_test() else {
        return;
    };

    let resolved = resolve_interface_binding_ip(
        Some(interface_name.as_str()),
        None,
        None,
        None,
        "input",
        true,
    )
    .expect("interface lookup should succeed");

    assert!(resolved.is_some());
}

fn first_ipv4_interface_name_for_test() -> Option<String> {
    unsafe {
        let mut addrs: *mut libc::ifaddrs = ptr::null_mut();
        if libc::getifaddrs(&mut addrs) != 0 || addrs.is_null() {
            return None;
        }

        let mut current = addrs;
        let mut resolved = None;
        while !current.is_null() {
            let ifa = &*current;
            if !ifa.ifa_name.is_null()
                && !ifa.ifa_addr.is_null()
                && (*ifa.ifa_addr).sa_family as i32 == libc::AF_INET
            {
                resolved = Some(CStr::from_ptr(ifa.ifa_name).to_string_lossy().to_string());
                break;
            }
            current = ifa.ifa_next;
        }
        libc::freeifaddrs(addrs);
        resolved
    }
}

#[test]
fn build_input_url_resolves_relative_file_input_under_work_root() {
    let settings = test_settings("/tmp/work");
    let input = InputSpec {
        kind: Some(InputKind::File),
        url: Some("vod/demo.ts".to_string()),
        ..InputSpec::default()
    };

    let input_url = build_input_url(&settings, &input).expect("input url should resolve");

    assert_eq!(input_url, "/tmp/work/vod/demo.ts");
}

#[test]
fn build_input_url_strips_leading_slash_for_file_input() {
    let settings = test_settings("/tmp/work");
    let input = InputSpec {
        kind: Some(InputKind::File),
        url: Some("/demo.mp4".to_string()),
        ..InputSpec::default()
    };

    let input_url = build_input_url(&settings, &input).expect("input url should resolve");

    assert_eq!(input_url, "/tmp/work/demo.mp4");
}

#[test]
fn build_input_url_rejects_parent_dir_in_file_input() {
    let settings = test_settings("/tmp/work");
    let input = InputSpec {
        kind: Some(InputKind::File),
        url: Some("../demo.mp4".to_string()),
        ..InputSpec::default()
    };

    let error = build_input_url(&settings, &input).expect_err("input url should fail");

    assert!(matches!(
        error,
        ExecutorError::InvalidRequest(message)
            if message.contains("must not contain '..' segments")
    ));
}

#[test]
fn build_input_url_keeps_ftp_url_unchanged() {
    let settings = test_settings("/tmp/work");
    let input = InputSpec {
        kind: Some(InputKind::Ftp),
        url: Some("ftp://vod.example.com/archive/demo.mp4".to_string()),
        ..InputSpec::default()
    };

    let input_url = build_input_url(&settings, &input).expect("input url should resolve");

    assert_eq!(input_url, "ftp://vod.example.com/archive/demo.mp4");
}

#[test]
fn build_file_to_live_plan_uses_zlm_recording_for_mp4_record() {
    let settings = test_settings("/tmp/work");
    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "file-live",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "input.mp4"},
            "stream": {"app": "live", "name": "stream"},
            "process": {"mode": "copy_or_transcode"},
            "record": {
                "enabled": true,
                "format": "mp4"
            },
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_file_to_live_plan(&settings, &request, &spec).expect("plan should build");

    assert!(!plan.args.iter().any(|arg| arg == "tee"));
    assert_eq!(plan.output_target, "rtmp://127.0.0.1:1935/live/stream");
    assert!(
        !plan
            .args
            .windows(2)
            .any(|window| window == ["-rtsp_transport", "tcp"])
    );
    assert!(plan.args.windows(2).any(|window| window == ["-f", "flv"]));
    assert_eq!(
        plan.outputs,
        vec![
            "rtmp://127.0.0.1:1935/live/stream".to_string(),
            managed_output_dir(&settings, request.task_id, "mp4")
                .to_string_lossy()
                .to_string(),
        ]
    );
    assert_eq!(plan.internal_ingress_protocol.as_deref(), Some("rtmp"));
    assert!(plan.companion_recording.is_none());
    let recording = plan.recording.expect("recording should use ZLM API");
    assert_eq!(recording.formats, vec![ZlmRecordKind::Mp4]);
    assert_eq!(
        recording.root_path_mp4.as_deref(),
        Some(
            managed_output_dir(&settings, request.task_id, "mp4")
                .to_string_lossy()
                .as_ref()
        )
    );
    assert_eq!(recording.root_path_hls, None);
}

#[test]
fn build_file_to_live_plan_loops_vod_input_when_enabled() {
    let settings = test_settings("/tmp/work");
    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "file-live-loop",
            "common": {"created_by": "tester"},
            "input": {
                "kind": "file",
                "source_mode": "vod",
                "loop_enabled": true,
                "url": "input.mp4"
            },
            "stream": {"app": "live", "name": "stream"},
            "process": {"mode": "copy_or_transcode"},
            "record": {
                "enabled": true,
                "format": "mp4",
                "duration_sec": 300
            },
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_file_to_live_plan(&settings, &request, &spec).expect("plan should build");

    assert_eq!(plan.output_target, "rtmp://127.0.0.1:1935/live/stream");
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-stream_loop", "-1"])
    );
    assert!(plan.args.iter().any(|arg| arg == "-re"));
    assert!(plan.args.windows(2).any(|window| window == ["-t", "300"]));
}

#[test]
fn build_stream_ingest_fast_record_plan_disables_realtime_pacing_and_stream_probe() {
    let settings = test_settings("/tmp/work");
    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "vod-fast-record",
            "common": {"created_by": "tester"},
            "input": {
                "kind": "http_mp4",
                "source_mode": "vod",
                "url": "http://vod.example.com/archive.mp4"
            },
            "stream": {"app": "live", "name": "archive-fast"},
            "expose": {
                "enable_rtsp": false,
                "enable_rtmp": false,
                "enable_http_ts": false,
                "enable_http_fmp4": false,
                "enable_hls": false
            },
            "process": {"mode": "copy_or_transcode"},
            "record": {
                "enabled": true,
                "format": "mp4",
                "duration_sec": 300
            },
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_stream_ingest_plan(&settings, &request, &spec).expect("plan should build");

    assert!(!plan.args.iter().any(|arg| arg == "-re"));
    assert!(plan.startup_probe.is_none());
    assert_eq!(plan.recording, None);
    assert_eq!(
        plan.managed_file_output_kind,
        Some(ManagedFileOutputKind::StreamIngestRecord)
    );
    assert!(
        plan.output_target.starts_with(
            managed_output_dir(&settings, request.task_id, "mp4")
                .to_string_lossy()
                .as_ref()
        )
    );
    assert!(plan.output_target.ends_with(".mp4"));
    assert!(plan.args.windows(2).any(|window| window == ["-t", "300"]));
}

#[test]
fn build_stream_ingest_fast_record_plan_copies_mpegts_aac_for_mp4_output_with_bsf() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-fast-record-mpegts-aac-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin =
        create_mock_ffprobe_binary_with_format(&temp_root, "mpegts", "h264", Some("aac"));

    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "vod-fast-record-ts",
            "common": {"created_by": "tester"},
            "input": {
                "kind": "file",
                "source_mode": "vod",
                "url": "archive.ts"
            },
            "stream": {"app": "live", "name": "archive-fast-ts"},
            "expose": {
                "enable_rtsp": false,
                "enable_rtmp": false,
                "enable_http_ts": false,
                "enable_http_fmp4": false,
                "enable_hls": false
            },
            "process": {"mode": "copy_or_transcode"},
            "record": {
                "enabled": true,
                "format": "mp4"
            },
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_stream_ingest_plan(&settings, &request, &spec).expect("plan should build");

    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:v", "copy"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:a", "copy"])
    );
    assert!(!plan.args.windows(2).any(|window| window == ["-c:a", "aac"]));
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-bsf:a", "aac_adtstoasc"])
    );

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_stream_ingest_fast_record_plan_maps_only_aac_for_multi_audio_mp4() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-fast-record-multi-audio-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin = create_mock_ffprobe_binary_with_audio_streams(
        &temp_root,
        "mpegts",
        "hevc",
        &[
            ("mp3", Some(22_050), Some(2), Some(2)),
            ("truehd", Some(48_000), Some(6), Some(2)),
            ("dts", Some(48_000), Some(6), Some(2)),
            ("eac3", Some(48_000), Some(6), Some(2)),
            ("aac", Some(48_000), Some(2), Some(2)),
        ],
    );

    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "vod-fast-record-multi-audio",
            "common": {"created_by": "tester"},
            "input": {
                "kind": "file",
                "source_mode": "vod",
                "url": "archive.ts"
            },
            "stream": {"app": "live", "name": "archive-fast-multi-audio"},
            "expose": {
                "enable_rtsp": false,
                "enable_rtmp": false,
                "enable_http_ts": false,
                "enable_http_fmp4": false,
                "enable_hls": false
            },
            "process": {"mode": "copy_or_transcode"},
            "record": {
                "enabled": true,
                "format": "mp4"
            },
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_stream_ingest_plan(&settings, &request, &spec).expect("plan should build");

    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-map", "0:v?"])
    );
    assert!(plan.args.windows(2).any(|window| window == ["-map", "0:5"]));
    assert!(
        !plan
            .args
            .windows(2)
            .any(|window| window == ["-map", "0:a?"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:a", "copy"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-bsf:a", "aac_adtstoasc"])
    );

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_stream_ingest_fast_record_plan_copies_mpegts_h264_aac_for_hls_output() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-fast-record-hls-copy-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin =
        create_mock_ffprobe_binary_with_format(&temp_root, "mpegts", "h264", Some("aac"));

    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "vod-fast-record-hls",
            "common": {"created_by": "tester"},
            "input": {
                "kind": "file",
                "source_mode": "vod",
                "url": "archive.ts"
            },
            "stream": {"app": "live", "name": "archive-fast-hls"},
            "expose": {
                "enable_rtsp": false,
                "enable_rtmp": false,
                "enable_http_ts": false,
                "enable_http_fmp4": false,
                "enable_hls": false
            },
            "process": {"mode": "copy_or_transcode"},
            "record": {
                "enabled": true,
                "format": "hls",
                "segment_sec": 6
            },
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_stream_ingest_plan(&settings, &request, &spec).expect("plan should build");

    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:v", "copy"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:a", "copy"])
    );
    assert!(!plan.args.windows(2).any(|window| window == ["-c:a", "aac"]));
    assert!(plan.args.windows(2).any(|window| window == ["-f", "hls"]));

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_stream_ingest_fast_record_plan_copies_hls_h264_aac_for_hls_output() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-fast-record-hls-source-copy-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin =
        create_mock_ffprobe_binary_with_format(&temp_root, "hls", "h264", Some("aac"));

    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "vod-fast-record-hls-source",
            "common": {"created_by": "tester"},
            "input": {
                "kind": "hls",
                "source_mode": "vod",
                "url": "http://vod.example.com/archive.m3u8"
            },
            "stream": {"app": "live", "name": "archive-fast-hls-source"},
            "expose": {
                "enable_rtsp": false,
                "enable_rtmp": false,
                "enable_http_ts": false,
                "enable_http_fmp4": false,
                "enable_hls": false
            },
            "process": {"mode": "copy_or_transcode"},
            "record": {
                "enabled": true,
                "format": "hls",
                "segment_sec": 6
            },
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_stream_ingest_plan(&settings, &request, &spec).expect("plan should build");

    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:v", "copy"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:a", "copy"])
    );
    assert!(!plan.args.windows(2).any(|window| window == ["-c:a", "aac"]));
    assert!(plan.args.windows(2).any(|window| window == ["-f", "hls"]));
    assert!(plan.internal_ingress_protocol.is_none());

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_stream_ingest_fast_record_plan_uses_configured_hls_segment_default() {
    let mut settings = test_settings("/tmp/work");
    settings.hls_record_segment_sec = 30;
    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "vod-fast-record-hls-default-segment",
            "common": {"created_by": "tester"},
            "input": {
                "kind": "file",
                "source_mode": "vod",
                "url": "archive.mp4"
            },
            "stream": {"app": "live", "name": "archive-hls-default-segment"},
            "expose": {
                "enable_rtsp": false,
                "enable_rtmp": false,
                "enable_http_ts": false,
                "enable_http_fmp4": false,
                "enable_hls": false
            },
            "process": {"mode": "copy_or_transcode"},
            "record": {
                "enabled": true,
                "format": "hls"
            },
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_stream_ingest_plan(&settings, &request, &spec).expect("plan should build");

    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-hls_time", "30"])
    );
}

#[test]
fn build_stream_ingest_fast_record_plan_generates_mp4_and_hls_outputs_for_both_format() {
    let temp_root =
        std::env::temp_dir().join(format!("streamserver-fast-record-both-{}", Uuid::now_v7()));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin = create_mock_ffprobe_binary(&temp_root, "h264", Some("aac"));
    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "vod-fast-record-both",
            "common": {"created_by": "tester"},
            "input": {
                "kind": "file",
                "source_mode": "vod",
                "url": "archive.mp4"
            },
            "stream": {"app": "live", "name": "archive-both"},
            "expose": {
                "enable_rtsp": false,
                "enable_rtmp": false,
                "enable_http_ts": false,
                "enable_http_fmp4": false,
                "enable_hls": false
            },
            "process": {"mode": "copy_or_transcode"},
            "record": {
                "enabled": true,
                "format": "both",
                "segment_sec": 8
            },
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_stream_ingest_plan(&settings, &request, &spec).expect("plan should build");

    assert_eq!(plan.outputs.len(), 2);
    assert!(plan.outputs.iter().any(|output| output.ends_with(".mp4")));
    assert!(plan.outputs.iter().any(|output| output.ends_with(".m3u8")));
    assert!(plan.args.windows(2).any(|window| window == ["-f", "mp4"]));
    assert!(plan.args.windows(2).any(|window| window == ["-f", "hls"]));
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-hls_time", "8"])
    );
    let mp4_output = plan
        .outputs
        .iter()
        .find(|output| output.ends_with(".mp4"))
        .expect("mp4 output should exist");
    let hls_output = plan
        .outputs
        .iter()
        .find(|output| output.ends_with(".m3u8"))
        .expect("hls output should exist");
    let mp4_target_position = plan
        .args
        .iter()
        .position(|arg| arg == mp4_output)
        .expect("mp4 target should be present");
    let hls_muxer_position = plan
        .args
        .windows(2)
        .position(|window| window == ["-f", "hls"])
        .expect("hls muxer should be present");
    let hls_target_position = plan
        .args
        .iter()
        .position(|arg| arg == hls_output)
        .expect("hls target should be present");
    let video_map_positions: Vec<_> = plan
        .args
        .windows(2)
        .enumerate()
        .filter_map(|(index, window)| (window == ["-map", "0:v?"]).then_some(index))
        .collect();
    let audio_map_positions: Vec<_> = plan
        .args
        .windows(2)
        .enumerate()
        .filter_map(|(index, window)| (window == ["-map", "0:1"]).then_some(index))
        .collect();
    let video_codec_positions: Vec<_> = plan
        .args
        .windows(2)
        .enumerate()
        .filter_map(|(index, window)| (window == ["-c:v", "copy"]).then_some(index))
        .collect();
    let audio_codec_positions: Vec<_> = plan
        .args
        .windows(2)
        .enumerate()
        .filter_map(|(index, window)| (window == ["-c:a", "copy"]).then_some(index))
        .collect();

    assert_eq!(video_map_positions.len(), 2);
    assert_eq!(audio_map_positions.len(), 2);
    assert_eq!(video_codec_positions.len(), 2);
    assert_eq!(audio_codec_positions.len(), 2);
    assert!(video_codec_positions[0] < video_map_positions[0]);
    assert!(audio_codec_positions[0] < audio_map_positions[0]);
    assert!(video_map_positions[0] < mp4_target_position);
    assert!(audio_map_positions[0] < mp4_target_position);
    assert!(mp4_target_position < video_codec_positions[1]);
    assert!(video_codec_positions[1] < video_map_positions[1]);
    assert!(mp4_target_position < audio_codec_positions[1]);
    assert!(audio_codec_positions[1] < audio_map_positions[1]);
    assert!(mp4_target_position < video_map_positions[1]);
    assert!(video_map_positions[1] < hls_muxer_position);
    assert!(mp4_target_position < audio_map_positions[1]);
    assert!(audio_map_positions[1] < hls_muxer_position);
    assert!(hls_muxer_position < hls_target_position);
    match &plan.success_check {
        SuccessCheck::FilesExist(paths) => {
            assert_eq!(paths.len(), 2);
            assert!(
                paths
                    .iter()
                    .any(|path| path.to_string_lossy().ends_with(".mp4"))
            );
            assert!(
                paths
                    .iter()
                    .any(|path| path.to_string_lossy().ends_with(".m3u8"))
            );
        }
        other => panic!("expected FilesExist success check, got {other:?}"),
    }

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn prepare_plan_paths_creates_all_dual_output_parent_dirs() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-prepare-dual-output-{}",
        Uuid::now_v7()
    ));
    let work_dir = temp_root.join("task").join("attempt-1");
    let mp4_path = temp_root.join("output/mp4/node-1-mp4/task/out.mp4");
    let hls_path = temp_root.join("output/hls/node-1-hls/task/out.m3u8");

    let plan = ProcessPlan {
        executable: "ffmpeg".to_string(),
        args: Vec::new(),
        work_dir: work_dir.clone(),
        output_target: mp4_path.to_string_lossy().to_string(),
        outputs: vec![
            mp4_path.to_string_lossy().to_string(),
            hls_path.to_string_lossy().to_string(),
        ],
        success_check: SuccessCheck::FilesExist(vec![mp4_path.clone(), hls_path.clone()]),
        startup_probe: None,
        recording: None,
        managed_file_output_kind: Some(ManagedFileOutputKind::StreamIngestRecord),
        companion_recording: None,
        internal_ingress_protocol: None,
    };

    prepare_plan_paths(&plan).expect("plan paths should prepare");

    assert!(work_dir.exists());
    assert!(mp4_path.parent().expect("mp4 parent").exists());
    assert!(hls_path.parent().expect("hls parent").exists());

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn prepare_plan_paths_creates_live_relay_recording_root_dirs() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-prepare-record-roots-{}",
        Uuid::now_v7()
    ));
    let work_dir = temp_root.join("task").join("attempt-1");
    let mp4_root = temp_root.join("output/mp4/node-1-mp4/task");
    let hls_root = temp_root.join("output/hls/node-1-hls/task");

    let plan = ProcessPlan {
        executable: "ffmpeg".to_string(),
        args: Vec::new(),
        work_dir: work_dir.clone(),
        output_target: "rtmp://127.0.0.1/live/test".to_string(),
        outputs: vec!["rtmp://127.0.0.1/live/test".to_string()],
        success_check: SuccessCheck::ProcessExit,
        startup_probe: None,
        recording: Some(LiveRelayRecording {
            formats: vec![ZlmRecordKind::Mp4, ZlmRecordKind::Hls],
            root_path_mp4: Some(mp4_root.to_string_lossy().to_string()),
            root_path_hls: Some(hls_root.to_string_lossy().to_string()),
            duration_sec: None,
            segment_sec: None,
            as_player: false,
            desired_enabled: true,
            manual_control: false,
            stop_task_on_duration: true,
            control_command_id: None,
            recording_started_at: None,
            auto_stop_requested: false,
            completion_reason: None,
            started: false,
            failed: false,
        }),
        managed_file_output_kind: None,
        companion_recording: None,
        internal_ingress_protocol: None,
    };

    prepare_plan_paths(&plan).expect("recording roots should prepare");

    assert!(work_dir.exists());
    assert!(mp4_root.exists());
    assert!(hls_root.exists());

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_stream_ingest_fast_record_plan_copies_mpegts_aac_for_both_output_with_mp4_bsf() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-fast-record-both-mpegts-aac-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin =
        create_mock_ffprobe_binary_with_format(&temp_root, "mpegts", "h264", Some("aac"));

    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "vod-fast-record-both-mpegts",
            "common": {"created_by": "tester"},
            "input": {
                "kind": "file",
                "source_mode": "vod",
                "url": "archive.ts"
            },
            "stream": {"app": "live", "name": "archive-both-mpegts"},
            "expose": {
                "enable_rtsp": false,
                "enable_rtmp": false,
                "enable_http_ts": false,
                "enable_http_fmp4": false,
                "enable_hls": false
            },
            "process": {"mode": "copy_or_transcode"},
            "record": {
                "enabled": true,
                "format": "both",
                "segment_sec": 8
            },
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_stream_ingest_plan(&settings, &request, &spec).expect("plan should build");

    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:v", "copy"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:a", "copy"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-bsf:a", "aac_adtstoasc"])
    );
    assert_eq!(
        plan.args
            .windows(2)
            .filter(|window| *window == ["-bsf:a", "aac_adtstoasc"])
            .count(),
        1
    );
    let input_index = plan
        .args
        .iter()
        .position(|arg| arg == "-i")
        .expect("ffmpeg input should exist");
    assert!(
        plan.args[..input_index]
            .windows(2)
            .any(|window| window == ["-probesize", "8000000"])
    );

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_file_to_live_plan_copy_or_transcode_routes_mpegts_aac_to_internal_rtmp() {
    let temp_root =
        std::env::temp_dir().join(format!("streamserver-file-live-mpegts-{}", Uuid::now_v7()));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin =
        create_mock_ffprobe_binary_with_format(&temp_root, "mpegts", "h264", Some("aac"));

    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "file-live-copy",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "input.ts"},
            "stream": {"app": "live", "name": "stream"},
            "process": {"mode": "copy_or_transcode"},
            "record": {},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_file_to_live_plan(&settings, &request, &spec).expect("plan should build");

    assert_eq!(plan.output_target, "rtmp://127.0.0.1:1935/live/stream");
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:v", "copy"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:a", "copy"])
    );
    assert!(
        !plan
            .args
            .windows(2)
            .any(|window| window == ["-c:v", "libx264"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-bsf:a", "aac_adtstoasc"])
    );
    assert!(plan.args.windows(2).any(|window| window == ["-f", "flv"]));
    assert_eq!(plan.internal_ingress_protocol.as_deref(), Some("rtmp"));
    assert!(
        !plan
            .args
            .windows(2)
            .any(|window| window == ["-threads", "0"])
    );
    let input_index = plan
        .args
        .iter()
        .position(|arg| arg == "-i")
        .expect("ffmpeg input should exist");
    assert!(
        plan.args[..input_index]
            .windows(2)
            .any(|window| window == ["-probesize", "8000000"])
    );

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_file_to_live_plan_copy_or_transcode_copies_mp4_h264_aac_into_internal_rtmp() {
    let temp_root =
        std::env::temp_dir().join(format!("streamserver-file-live-mp4-{}", Uuid::now_v7()));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin = create_mock_ffprobe_binary(&temp_root, "h264", Some("aac"));

    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "file-live-copy-safe",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "input.mp4"},
            "stream": {"app": "live", "name": "stream"},
            "process": {"mode": "copy_or_transcode"},
            "record": {},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_file_to_live_plan(&settings, &request, &spec).expect("plan should build");

    assert_eq!(plan.output_target, "rtmp://127.0.0.1:1935/live/stream");
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:v", "copy"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:a", "copy"])
    );
    assert!(!plan.args.windows(2).any(|window| window == ["-c:a", "aac"]));
    let input_index = plan
        .args
        .iter()
        .position(|arg| arg == "-i")
        .expect("ffmpeg input should exist");
    assert!(
        !plan.args[..input_index]
            .windows(2)
            .any(|window| window == ["-probesize", "8000000"])
    );

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_file_to_live_plan_copy_or_transcode_copies_hevc_aac_into_internal_enhanced_rtmp() {
    let temp_root =
        std::env::temp_dir().join(format!("streamserver-file-live-hevc-{}", Uuid::now_v7()));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin = create_mock_ffprobe_binary(&temp_root, "hevc", Some("aac"));

    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "file-live-copy-hevc",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "input.mp4"},
            "stream": {"app": "live", "name": "stream"},
            "process": {"mode": "copy_or_transcode"},
            "record": {},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_file_to_live_plan_with_capability_hints(
        &settings,
        &request,
        &spec,
        RuntimeCapabilityHints {
            zlm_rtmp_enhanced_enabled: Some(true),
        },
    )
    .expect("plan should build");

    assert_eq!(plan.output_target, "rtmp://127.0.0.1:1935/live/stream");
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:v", "copy"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:a", "copy"])
    );
    assert!(
        !plan
            .args
            .windows(2)
            .any(|window| window == ["-c:v", "libx264"])
    );
    assert!(!plan.args.windows(2).any(|window| window == ["-c:a", "aac"]));
    assert!(plan.args.windows(2).any(|window| window == ["-f", "flv"]));
    assert_eq!(
        plan.internal_ingress_protocol.as_deref(),
        Some("enhanced_rtmp")
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-threads", "0"])
    );

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_file_to_live_plan_maps_only_aac_for_multi_audio_internal_enhanced_rtmp() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-file-live-multi-audio-enhanced-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin = create_mock_ffprobe_binary_with_audio_streams(
        &temp_root,
        "mpegts",
        "hevc",
        &[
            ("mp3", Some(22_050), Some(2), Some(2)),
            ("truehd", Some(48_000), Some(6), Some(2)),
            ("dts", Some(48_000), Some(6), Some(2)),
            ("eac3", Some(48_000), Some(6), Some(2)),
            ("aac", Some(48_000), Some(2), Some(2)),
        ],
    );

    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "file-live-multi-audio-enhanced",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "input.ts"},
            "stream": {"app": "live", "name": "stream"},
            "expose": {
                "enable_rtmp": false,
                "enable_rtsp": false,
                "enable_http_ts": false,
                "enable_http_fmp4": true,
                "enable_hls": false
            },
            "process": {"mode": "copy_or_transcode"},
            "record": {},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_file_to_live_plan_with_capability_hints(
        &settings,
        &request,
        &spec,
        RuntimeCapabilityHints {
            zlm_rtmp_enhanced_enabled: Some(true),
        },
    )
    .expect("plan should build");

    assert_eq!(plan.output_target, "rtmp://127.0.0.1:1935/live/stream");
    assert_eq!(
        plan.internal_ingress_protocol.as_deref(),
        Some("enhanced_rtmp")
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-map", "0:v?"])
    );
    assert!(plan.args.windows(2).any(|window| window == ["-map", "0:5"]));
    assert!(
        !plan
            .args
            .windows(2)
            .any(|window| window == ["-map", "0:a?"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:a", "copy"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-bsf:a", "aac_adtstoasc"])
    );
    assert!(plan.args.windows(2).any(|window| window == ["-f", "flv"]));
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-threads", "0"])
    );

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_file_to_live_plan_falls_back_to_rtsp_and_transcodes_aac_when_enhanced_is_unavailable() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-file-live-hevc-aac-rtsp-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin = create_mock_ffprobe_binary_with_profile(
        &temp_root,
        "mpegts",
        "hevc",
        Some("aac"),
        Some(48_000),
        Some(2),
        Some(32),
        Some(0),
    );

    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "file-live-hevc-aac-rtsp-fallback",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "input.ts"},
            "stream": {"app": "live", "name": "stream"},
            "process": {"mode": "copy_or_transcode"},
            "record": {},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_file_to_live_plan(&settings, &request, &spec).expect("plan should build");

    assert_eq!(plan.output_target, "rtsp://127.0.0.1:554/live/stream");
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:v", "copy"])
    );
    assert!(plan.args.windows(2).any(|window| window == ["-c:a", "aac"]));
    assert!(plan.args.windows(2).any(|window| window == ["-f", "rtsp"]));
    assert_eq!(plan.internal_ingress_protocol.as_deref(), Some("rtsp"));
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-threads", "0"])
    );

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_file_to_live_plan_copy_or_transcode_copies_mp3_audio_when_flv_allows_it() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-file-live-mp3-copy-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin = create_mock_ffprobe_binary_with_profile(
        &temp_root,
        "mpegts",
        "h264",
        Some("mp3"),
        Some(44_100),
        Some(2),
        Some(32),
        Some(2),
    );

    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "file-live-mp3-copy",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "input.ts"},
            "stream": {"app": "live", "name": "stream"},
            "process": {"mode": "copy_or_transcode"},
            "record": {},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_file_to_live_plan(&settings, &request, &spec).expect("plan should build");

    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:v", "copy"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:a", "copy"])
    );
    assert!(!plan.args.windows(2).any(|window| window == ["-c:a", "aac"]));
    assert!(plan.args.windows(2).any(|window| window == ["-f", "flv"]));
    let input_index = plan
        .args
        .iter()
        .position(|arg| arg == "-i")
        .expect("ffmpeg input should exist");
    assert!(
        !plan.args[..input_index]
            .windows(2)
            .any(|window| window == ["-probesize", "8000000"])
    );

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_file_to_live_plan_uses_larger_probe_for_ts_ffprobe_preflight() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-file-live-probe-preflight-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");
    let recorded_args_path = temp_root.join("ffprobe-args.txt");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin = create_recording_mock_ffprobe_binary_with_profile(
        &temp_root,
        &recorded_args_path,
        "mpegts",
        "h264",
        Some("aac"),
        Some(48_000),
        Some(2),
        Some(32),
        Some(2),
    );

    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "file-live-probe-preflight",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "input.ts"},
            "stream": {"app": "live", "name": "stream"},
            "process": {"mode": "copy_or_transcode"},
            "record": {},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_file_to_live_plan(&settings, &request, &spec).expect("plan should build");
    match fs::read_to_string(&recorded_args_path) {
        Ok(recorded_args) => {
            assert!(recorded_args.lines().any(|line| line == "-probesize"));
            assert!(recorded_args.lines().any(|line| line == "8000000"));
        }
        Err(_) => {
            let input_index = plan
                .args
                .iter()
                .position(|arg| arg == "-i")
                .expect("ffmpeg input should exist");
            assert!(
                plan.args[..input_index]
                    .windows(2)
                    .any(|window| window == ["-probesize", "8000000"])
            );
        }
    }

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_file_to_live_plan_rejects_ts_aac_copy_when_audio_params_remain_missing() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-file-live-bad-ts-aac-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin = create_mock_ffprobe_binary_with_profile(
        &temp_root,
        "mpegts",
        "h264",
        Some("aac"),
        None,
        None,
        Some(32),
        Some(2),
    );

    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "file-live-bad-ts-aac",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "input.ts"},
            "stream": {"app": "live", "name": "stream"},
            "process": {"mode": "copy_or_transcode"},
            "record": {},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let error =
        build_file_to_live_plan(&settings, &request, &spec).expect_err("plan should reject");

    match error {
        ExecutorError::InvalidRequest(message) => {
            assert!(message.contains("sample_rate/channels remain unavailable"));
            assert!(message.contains("refusing audio copy"));
        }
        other => panic!("expected invalid request, got {other:?}"),
    }

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_file_to_live_plan_copy_or_transcode_transcodes_mp3_when_flv_sample_rate_is_unsupported() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-file-live-mp3-transcode-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin = create_mock_ffprobe_binary_with_profile(
        &temp_root,
        "mpegts",
        "h264",
        Some("mp3"),
        Some(48_000),
        Some(2),
        Some(32),
        Some(2),
    );

    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "file-live-mp3-transcode",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "input.ts"},
            "stream": {"app": "live", "name": "stream"},
            "process": {"mode": "copy_or_transcode"},
            "record": {},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_file_to_live_plan(&settings, &request, &spec).expect("plan should build");

    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:v", "copy"])
    );
    assert!(plan.args.windows(2).any(|window| window == ["-c:a", "aac"]));
    assert!(plan.args.windows(2).any(|window| window == ["-f", "flv"]));

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_file_to_live_plan_copy_or_transcode_copies_opus_audio_for_internal_rtsp() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-file-live-opus-transcode-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin =
        create_mock_ffprobe_binary_with_format(&temp_root, "matroska", "h264", Some("opus"));

    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "file-live-opus-transcode",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "input.mkv"},
            "stream": {"app": "live", "name": "stream"},
            "process": {"mode": "copy_or_transcode"},
            "record": {},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_file_to_live_plan(&settings, &request, &spec).expect("plan should build");

    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:v", "copy"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:a", "copy"])
    );
    assert!(!plan.args.windows(2).any(|window| window == ["-c:a", "aac"]));
    assert!(plan.args.windows(2).any(|window| window == ["-f", "rtsp"]));
    assert_eq!(plan.internal_ingress_protocol.as_deref(), Some("rtsp"));

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_file_transcode_plan_copy_or_transcode_copies_mp2_when_mpegts_allows_it() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-copy-transcode-mpegts-mp2-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin =
        create_mock_ffprobe_binary_with_format(&temp_root, "mpegts", "h264", Some("mp2"));

    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::FileTranscode,
        resolved_spec: json!({
            "type": "file_transcode",
            "name": "test-mpegts-mp2-to-mpegts",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "input.ts"},
            "process": {"mode": "copy_or_transcode"},
            "record": {},
            "publish": {
                "kind": "file",
                "format": "mpegts"
            },
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_file_transcode_plan(&settings, &request, &spec).expect("plan should build");

    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:v", "copy"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:a", "copy"])
    );
    assert!(!plan.args.windows(2).any(|window| window == ["-c:a", "aac"]));

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_file_transcode_plan_copy_or_transcode_copies_mp3_when_mp4_allows_it() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-copy-transcode-mp4-mp3-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin = create_mock_ffprobe_binary(&temp_root, "h264", Some("mp3"));

    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::FileTranscode,
        resolved_spec: json!({
            "type": "file_transcode",
            "name": "test-mp4-mp3-copy",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "input.mp4"},
            "process": {"mode": "copy_or_transcode"},
            "record": {},
            "publish": {
                "kind": "file",
                "format": "mp4"
            },
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_file_transcode_plan(&settings, &request, &spec).expect("plan should build");

    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:v", "copy"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:a", "copy"])
    );
    assert!(!plan.args.windows(2).any(|window| window == ["-c:a", "aac"]));

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_file_transcode_plan_copy_or_transcode_copies_opus_when_mkv_allows_it() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-copy-transcode-mkv-opus-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin =
        create_mock_ffprobe_binary_with_format(&temp_root, "matroska", "h264", Some("opus"));

    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::FileTranscode,
        resolved_spec: json!({
            "type": "file_transcode",
            "name": "test-mkv-opus-copy",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "input.mkv"},
            "process": {"mode": "copy_or_transcode"},
            "record": {},
            "publish": {
                "kind": "file",
                "format": "mkv"
            },
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_file_transcode_plan(&settings, &request, &spec).expect("plan should build");

    assert_eq!(plan.outputs.len(), 1);
    assert!(plan.outputs[0].ends_with(".mkv"));
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-f", "matroska"])
    );
    assert!(!plan.args.windows(2).any(|window| window == ["-f", "mkv"]));
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:v", "copy"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:a", "copy"])
    );
    assert!(!plan.args.windows(2).any(|window| window == ["-c:a", "aac"]));

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_file_transcode_plan_maps_matroska_format_to_mkv_extension_and_matroska_muxer() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-copy-transcode-matroska-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin =
        create_mock_ffprobe_binary_with_format(&temp_root, "matroska", "h264", Some("opus"));

    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::FileTranscode,
        resolved_spec: json!({
            "type": "file_transcode",
            "name": "test-matroska-opus-copy",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "input.mkv"},
            "process": {"mode": "copy_or_transcode"},
            "record": {},
            "publish": {
                "kind": "file",
                "format": "matroska"
            },
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_file_transcode_plan(&settings, &request, &spec).expect("plan should build");

    assert_eq!(plan.outputs.len(), 1);
    assert!(plan.outputs[0].ends_with(".mkv"));
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-f", "matroska"])
    );
    assert!(!plan.args.windows(2).any(|window| window == ["-f", "mkv"]));

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_file_transcode_plan_rejects_webm_output_format() {
    let settings = test_settings("/tmp/work");
    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::FileTranscode,
        resolved_spec: json!({
            "type": "file_transcode",
            "name": "test-webm-output-rejected",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "input.mp4"},
            "process": {"mode": "copy_or_transcode"},
            "record": {},
            "publish": {
                "kind": "file",
                "format": "webm"
            },
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let error =
        build_file_transcode_plan(&settings, &request, &spec).expect_err("plan should fail");

    assert!(matches!(
        error,
        ExecutorError::InvalidRequest(message) if message.contains("publish.format=webm")
    ));
}

#[test]
fn build_file_to_live_plan_uses_zlm_recording_when_recording_mp4_from_mpegts_aac() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-file-live-recording-mpegts-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin =
        create_mock_ffprobe_binary_with_format(&temp_root, "mpegts", "h264", Some("aac"));

    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "file-live-recording",
            "common": {"created_by": "tester"},
            "input": {"kind": "file", "url": "input.ts", "source_mode": "vod", "loop_enabled": true},
            "stream": {"app": "live", "name": "stream"},
            "process": {"mode": "copy_or_transcode", "bitrate": 8000},
            "record": {"enabled": true, "format": "mp4"},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_file_to_live_plan(&settings, &request, &spec).expect("plan should build");

    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:v", "libx264"])
    );
    assert!(
        plan.args
            .windows(2)
            .any(|window| window == ["-c:a", "copy"])
    );
    assert!(!plan.args.iter().any(|arg| arg == "tee"));
    assert!(plan.args.iter().any(|arg| arg == "aac_adtstoasc"));
    assert!(plan.companion_recording.is_none());
    let recording = plan.recording.expect("recording should use ZLM");
    assert_eq!(recording.formats, vec![ZlmRecordKind::Mp4]);

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn attach_file_artifact_metadata_uses_stream_ingest_outputs() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-stream-ingest-artifact-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");
    let artifact_path = temp_root.join("record.mp4");
    fs::write(&artifact_path, b"artifact").expect("artifact should be written");

    let mut handle = RuntimeHandle {
        runtime_id: Uuid::nil(),
        task_id: Uuid::nil(),
        attempt_no: 1,
        worker_kind: WorkerKind::ZlmProxy,
        pid: Some(1234),
        started_at: Utc::now(),
        last_progress_at: None,
        state: RuntimeState::Exited,
        command_line: None,
        outputs: vec![artifact_path.to_string_lossy().to_string()],
        metadata: json!({
            "managed_file_output_kind": "stream_ingest_record"
        }),
    };

    attach_file_artifact_metadata(&mut handle, &SuccessCheck::ProcessExit);

    let artifacts = handle.metadata["stream_ingest_record_artifacts"]
        .as_array()
        .expect("artifacts should be attached");
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0]["file_name"].as_str(), Some("record.mp4"));

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_file_to_live_plan_accepts_http_mp4_and_duration_limit() {
    let settings = test_settings("/tmp/work");
    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "file-live-http",
            "common": {"created_by": "tester"},
            "input": {"kind": "http_mp4", "source_mode": "vod", "url": "http://vod.example.com/archive.mp4"},
            "stream": {"app": "live", "name": "stream"},
            "process": {"mode": "copy_or_transcode"},
            "record": {
                "enabled": true,
                "format": "mp4",
                "duration_sec": 300
            },
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_file_to_live_plan(&settings, &request, &spec).expect("plan should build");

    assert!(plan.args.iter().any(|arg| arg == "-re"));
    assert!(plan.args.windows(2).any(|window| window == ["-t", "300"]));
    assert!(
        plan.args
            .iter()
            .any(|arg| arg == "http://vod.example.com/archive.mp4")
    );
}

#[test]
fn build_file_to_live_plan_uses_rtmp_for_internal_publish() {
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-file-live-internal-publish-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");

    let mut settings = test_settings("/tmp/work");
    settings.ffprobe_bin = create_mock_ffprobe_binary(&temp_root, "h264", Some("aac"));
    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "file-live-flv",
            "common": {"created_by": "tester"},
            "input": {
                "kind": "http_mp4",
                "source_mode": "vod",
                "url": "http://vod.example.com/archive.mp4"
            },
            "stream": {
                "app": "live",
                "name": "internal-flv-check"
            },
            "expose": {
                "enable_rtmp": true
            },
            "record": {},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_file_to_live_plan(&settings, &request, &spec).expect("plan should build");

    assert_eq!(
        plan.output_target,
        "rtmp://127.0.0.1:1935/live/internal-flv-check"
    );
    assert!(
        !plan
            .args
            .windows(2)
            .any(|window| window == ["-rtsp_transport", "tcp"])
    );
    assert!(plan.args.windows(2).any(|window| window == ["-f", "flv"]));
    assert_eq!(plan.internal_ingress_protocol.as_deref(), Some("rtmp"));

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_file_to_live_plan_uses_configured_zlm_rtsp_port() {
    let mut settings = test_settings("/tmp/work");
    settings.zlm_rtsp_port = 9554;
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-file-live-rtsp-port-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");
    settings.ffprobe_bin =
        create_mock_ffprobe_binary_with_format(&temp_root, "mpegts", "h264", Some("mp2"));
    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "configured-rtsp-port",
            "common": {"created_by": "tester"},
            "input": {"kind": "http_mp4", "source_mode": "vod", "url": "http://vod.example.com/archive.mp4"},
            "stream": {"app": "live", "name": "configured-port"},
            "process": {"mode": "copy_or_transcode"},
            "expose": {
                "enable_rtsp": true,
                "enable_rtmp": false,
                "enable_http_ts": false,
                "enable_http_fmp4": false,
                "enable_hls": false
            },
            "record": {},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_file_to_live_plan(&settings, &request, &spec).expect("plan should build");

    assert_eq!(
        plan.output_target,
        "rtsp://127.0.0.1:9554/live/configured-port"
    );
    assert!(plan.args.windows(2).any(|window| window == ["-f", "rtsp"]));

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_file_to_live_plan_uses_configured_zlm_rtmp_port() {
    let mut settings = test_settings("/tmp/work");
    settings.zlm_rtmp_port = 2935;
    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-file-live-rtmp-port-{}",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&temp_root).expect("temp root should exist");
    settings.ffprobe_bin = create_mock_ffprobe_binary(&temp_root, "h264", Some("aac"));

    let request = StartTaskRequest {
        task_id: Uuid::nil(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "configured-rtmp-port",
            "common": {"created_by": "tester"},
            "input": {"kind": "http_mp4", "source_mode": "vod", "url": "http://vod.example.com/archive.mp4"},
            "stream": {"app": "live", "name": "configured-port"},
            "process": {"mode": "copy_or_transcode"},
            "record": {},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_file_to_live_plan(&settings, &request, &spec).expect("plan should build");

    assert_eq!(
        plan.output_target,
        "rtmp://127.0.0.1:2935/live/configured-port"
    );
    assert!(plan.args.windows(2).any(|window| window == ["-f", "flv"]));

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn build_live_relay_plan_allocates_stable_stream_binding() {
    let settings = test_settings("/tmp/work");
    let task_id = Uuid::now_v7();
    let request = StartTaskRequest {
        task_id,
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "relay",
            "common": {"created_by": "tester"},
            "input": {"kind": "rtsp", "url": "rtsp://camera.example/live"},
            "expose": {
                "enable_rtsp": true,
                "enable_rtmp": false,
                "enable_http_ts": false,
                "enable_http_fmp4": false,
                "enable_hls": false
            },
            "record": {},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_live_relay_plan(&settings, &request, &spec).expect("plan should build");

    assert_eq!(plan.startup_probe.schema.as_deref(), Some("rtsp"));
    assert_eq!(plan.startup_probe.vhost, "__defaultVhost__");
    assert_eq!(plan.startup_probe.app, "live");
    assert_eq!(plan.startup_probe.stream, task_id.to_string());
    assert!(
        plan.command_line
            .contains("zlm addStreamProxy --url rtsp://camera.example/live")
    );
}

#[test]
fn build_live_relay_api_params_uses_expose_protocols_without_auto_recording() {
    let mut settings = test_settings("/tmp/work");
    settings.zlm_auto_close_on_no_reader_enabled = true;
    let spec = serde_json::from_value::<TaskSpec>(json!({
        "type": "stream_ingest",
        "name": "relay",
        "common": {"created_by": "tester"},
        "input": {"kind": "rtsp", "url": "rtsp://camera.example/live", "probe_timeout_ms": 7000},
        "expose": {
            "enable_rtsp": false,
            "enable_rtmp": true,
            "enable_http_ts": false,
            "enable_http_fmp4": true,
            "enable_hls": false,
            "stop_on_no_reader": true
        },
        "record": {"enabled": true, "format": "both"},
        "recovery": {},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    }))
    .expect("task spec should parse");
    let startup_probe = StartupProbe {
        schema: Some("rtmp".to_string()),
        vhost: "__defaultVhost__".to_string(),
        app: "relay".to_string(),
        stream: "stream-1".to_string(),
    };

    let params = build_live_relay_api_params(
        &settings,
        &spec,
        &startup_probe,
        "rtsp://camera.example/live",
    )
    .into_iter()
    .collect::<HashMap<_, _>>();

    assert_eq!(params.get("enable_rtsp").map(String::as_str), Some("0"));
    assert_eq!(params.get("enable_rtmp").map(String::as_str), Some("1"));
    assert_eq!(params.get("enable_ts").map(String::as_str), Some("0"));
    assert_eq!(params.get("enable_fmp4").map(String::as_str), Some("1"));
    assert_eq!(params.get("enable_hls").map(String::as_str), Some("0"));
    assert_eq!(params.get("add_mute_audio").map(String::as_str), Some("0"));
    assert_eq!(params.get("enable_mp4").map(String::as_str), Some("0"));
    assert_eq!(params.get("auto_close").map(String::as_str), Some("1"));
    assert_eq!(params.get("timeout_sec").map(String::as_str), Some("7"));
}

#[test]
fn build_live_relay_plan_uses_managed_recording_root_when_enabled() {
    let settings = test_settings("/tmp/work");
    let request = StartTaskRequest {
        task_id: Uuid::now_v7(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "relay-record",
            "common": {"created_by": "tester"},
            "input": {"kind": "rtsp", "url": "rtsp://camera.example/live"},
            "publish": {},
            "record": {
                "enabled": true,
                "format": "mp4",
                "segment_sec": 120
            },
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_live_relay_plan(&settings, &request, &spec).expect("plan should build");
    let recording = plan.recording.expect("recording should be present");

    assert_eq!(recording.formats, vec![ZlmRecordKind::Mp4]);
    assert_eq!(
        recording.root_path_mp4.as_deref(),
        Some(
            managed_output_dir(&settings, request.task_id, "mp4")
                .to_string_lossy()
                .as_ref()
        )
    );
    assert_eq!(recording.root_path_hls, None);
    assert_eq!(recording.duration_sec, None);
    assert_eq!(recording.segment_sec, Some(120));
    assert!(plan.outputs.iter().any(|output| {
        output
            == &managed_output_dir(&settings, request.task_id, "mp4")
                .to_string_lossy()
                .to_string()
    }));
}

#[test]
fn build_live_relay_plan_omits_playback_probe_schema_for_record_only_recording() {
    let settings = test_settings("/tmp/work");
    let request = StartTaskRequest {
        task_id: Uuid::now_v7(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "relay-record-only",
            "common": {"created_by": "tester"},
            "input": {"kind": "rtsp", "url": "rtsp://camera.example/live"},
            "expose": {
                "enable_rtsp": false,
                "enable_rtmp": false,
                "enable_http_ts": false,
                "enable_http_fmp4": false,
                "enable_hls": false
            },
            "record": {"enabled": true, "format": "mp4"},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_live_relay_plan(&settings, &request, &spec).expect("plan should build");

    assert_eq!(plan.startup_probe.schema, None);
    assert!(plan.recording.is_some());
}

#[test]
fn task_runtime_mode_uses_managed_process_for_record_only_live_http_ts() {
    let request = StartTaskRequest {
        task_id: Uuid::now_v7(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "relay-record-only-http-ts",
            "common": {"created_by": "tester"},
            "input": {
                "kind": "http_ts",
                "source_mode": "live",
                "url": "http://camera.example/live.ts"
            },
            "expose": {
                "enable_rtsp": false,
                "enable_rtmp": false,
                "enable_http_ts": false,
                "enable_http_fmp4": false,
                "enable_hls": false
            },
            "record": {"enabled": true, "format": "mp4"},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");

    assert_eq!(task_runtime_mode(&spec), TaskRuntimeMode::ManagedProcess);
}

#[test]
fn build_process_plan_uses_internal_startup_schema_for_record_only_live_http_ts() {
    let settings = test_settings("/tmp/work");
    let request = StartTaskRequest {
        task_id: Uuid::now_v7(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "relay-record-only-http-ts",
            "common": {"created_by": "tester"},
            "input": {
                "kind": "http_ts",
                "source_mode": "live",
                "url": "http://camera.example/live.ts"
            },
            "expose": {
                "enable_rtsp": false,
                "enable_rtmp": false,
                "enable_http_ts": false,
                "enable_http_fmp4": false,
                "enable_hls": false
            },
            "record": {"enabled": true, "format": "mp4"},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let plan = build_process_plan(&settings, &request, RuntimeCapabilityHints::default())
        .expect("plan should build");
    let startup_probe = plan
        .startup_probe
        .as_ref()
        .expect("managed process plan should include startup probe");

    assert_eq!(
        task_runtime_mode(&parse_task_spec(&request).expect("spec should parse")),
        TaskRuntimeMode::ManagedProcess
    );
    assert!(startup_probe.schema.is_some());
    assert!(plan.recording.is_some());
}

#[test]
fn recording_duration_reached_uses_recording_start_time() {
    let started_at = Utc::now();
    let recording = LiveRelayRecording {
        formats: vec![ZlmRecordKind::Mp4],
        root_path_mp4: Some("/var/media/archive".to_string()),
        root_path_hls: None,
        duration_sec: Some(300),
        segment_sec: None,
        as_player: false,
        desired_enabled: true,
        manual_control: false,
        stop_task_on_duration: true,
        control_command_id: None,
        recording_started_at: Some(started_at),
        auto_stop_requested: false,
        completion_reason: None,
        started: true,
        failed: false,
    };

    assert!(!recording_duration_reached(
        &recording,
        started_at + chrono::Duration::seconds(299)
    ));
    assert!(recording_duration_reached(
        &recording,
        started_at + chrono::Duration::seconds(300)
    ));
}

#[test]
fn should_auto_stop_live_relay_recording_requires_started_and_not_already_requested() {
    let started_at = Utc::now();
    let base = LiveRelayRecording {
        formats: vec![ZlmRecordKind::Mp4],
        root_path_mp4: Some("/var/media/archive".to_string()),
        root_path_hls: None,
        duration_sec: Some(60),
        segment_sec: None,
        as_player: false,
        desired_enabled: true,
        manual_control: false,
        stop_task_on_duration: true,
        control_command_id: None,
        recording_started_at: Some(started_at),
        auto_stop_requested: false,
        completion_reason: None,
        started: true,
        failed: false,
    };

    assert!(should_auto_stop_live_relay_recording(
        &base,
        started_at + chrono::Duration::seconds(60)
    ));

    let mut already_requested = base.clone();
    already_requested.auto_stop_requested = true;
    assert!(!should_auto_stop_live_relay_recording(
        &already_requested,
        started_at + chrono::Duration::seconds(60)
    ));

    let mut not_started = base;
    not_started.started = false;
    assert!(!should_auto_stop_live_relay_recording(
        &not_started,
        started_at + chrono::Duration::seconds(60)
    ));
}

#[test]
fn manual_recording_duration_does_not_stop_task() {
    let started_at = Utc::now();
    let mut recording = test_live_relay_recording();
    recording.duration_sec = Some(60);
    recording.recording_started_at = Some(started_at);
    recording.started = true;
    recording.manual_control = true;
    recording.stop_task_on_duration = false;

    assert!(recording_duration_reached(
        &recording,
        started_at + chrono::Duration::seconds(60)
    ));
    assert!(!should_auto_stop_live_relay_recording(
        &recording,
        started_at + chrono::Duration::seconds(60)
    ));
}

#[test]
fn classify_adopted_exit_treats_record_duration_reached_as_success() {
    let handle = RuntimeHandle {
        runtime_id: Uuid::now_v7(),
        task_id: Uuid::now_v7(),
        attempt_no: 1,
        worker_kind: WorkerKind::Ffmpeg,
        pid: Some(1234),
        started_at: Utc::now(),
        last_progress_at: None,
        state: RuntimeState::Exited,
        command_line: Some("ffmpeg -re -i input".to_string()),
        outputs: vec!["rtmp://127.0.0.1/live/stream".to_string()],
        metadata: json!({
            "task_type": "file_to_live",
            "completion_reason": "record_duration_reached",
        }),
    };

    let (event_type, _, _, payload) =
        classify_adopted_exit(&handle, &SuccessCheck::ProcessExit, true);
    assert_eq!(event_type, "succeeded");
    assert_eq!(payload["reason"], json!("record_duration_reached"));
}

#[test]
fn classify_adopted_exit_treats_disk_threshold_stop_as_failed() {
    let handle = RuntimeHandle {
        runtime_id: Uuid::now_v7(),
        task_id: Uuid::now_v7(),
        attempt_no: 1,
        worker_kind: WorkerKind::Ffmpeg,
        pid: Some(1234),
        started_at: Utc::now(),
        last_progress_at: None,
        state: RuntimeState::Exited,
        command_line: Some("ffmpeg -re -i input".to_string()),
        outputs: vec!["/data/zlm/www/output/mp4/node-test-mp4/task/out.mp4".to_string()],
        metadata: json!({
            "task_type": "file_transcode",
            "stop": {"reason": "disk_threshold_exceeded"},
        }),
    };

    let (event_type, _, _, payload) =
        classify_adopted_exit(&handle, &SuccessCheck::ProcessExit, true);
    assert_eq!(event_type, "failed");
    assert_eq!(payload["reason"], json!("disk_threshold_exceeded"));
}

#[test]
fn should_auto_restart_process_restarts_continuous_stream_on_zero_exit() {
    let handle = continuous_stream_ingest_handle();

    assert!(should_auto_restart_process(
        &handle,
        false,
        &Ok(success_exit_status()),
    ));
}

#[test]
fn should_auto_restart_process_restarts_sticky_live_ingest_before_first_online() {
    let handle = sticky_live_ingest_handle(false);

    assert!(should_auto_restart_process(
        &handle,
        false,
        &Ok(success_exit_status()),
    ));
}

#[test]
fn sticky_reconnect_allows_unbounded_live_recording() {
    let mut handle = sticky_live_recording_ingest_handle(false);

    assert!(sticky_reconnect_stream_ingest_from_handle(&handle));
    assert!(should_auto_restart_process(
        &handle,
        false,
        &Ok(success_exit_status()),
    ));

    handle.metadata["resolved_spec"]["record"]["duration_sec"] = json!(60);
    assert!(!sticky_reconnect_stream_ingest_from_handle(&handle));
    assert!(!should_auto_restart_process(
        &handle,
        false,
        &Ok(success_exit_status()),
    ));
}

#[test]
fn sticky_reconnect_allows_looped_vod_recording_without_duration() {
    let mut handle = continuous_stream_ingest_handle();
    handle.metadata["resolved_spec"]["record"] = json!({"enabled": true});

    assert!(sticky_reconnect_stream_ingest_from_handle(&handle));

    handle.metadata["resolved_spec"]["record"]["duration_sec"] = json!(60);
    assert!(!sticky_reconnect_stream_ingest_from_handle(&handle));
}

#[test]
fn sticky_reconnect_respects_recovery_never() {
    let mut handle = sticky_live_ingest_handle(false);
    handle.metadata["resolved_spec"]["recovery"]["policy"] = json!("never");

    assert!(!sticky_reconnect_stream_ingest_from_handle(&handle));
    assert!(!should_auto_restart_process(
        &handle,
        false,
        &Ok(success_exit_status()),
    ));
}

#[test]
fn managed_restart_cleanup_uses_stream_binding() {
    let mut handle = sticky_live_ingest_handle(true);
    handle.metadata["stream_binding"] = json!({
        "schema": "rtmp",
        "vhost": "__defaultVhost__",
        "app": "preview",
        "stream": "channel-1",
    });

    let binding = managed_stream_restart_cleanup_binding(&handle)
        .expect("managed stream_ingest should clean stale ZLM stream before restart");

    assert_eq!(binding.schema.as_deref(), Some("rtmp"));
    assert_eq!(binding.vhost, "__defaultVhost__");
    assert_eq!(binding.app, "preview");
    assert_eq!(binding.stream, "channel-1");
}

#[test]
fn managed_restart_cleanup_falls_back_to_startup_probe() {
    let mut handle = sticky_live_ingest_handle(true);
    handle.metadata["startup_probe"] = json!({
        "schema": "rtmp",
        "vhost": "__defaultVhost__",
        "app": "preview",
        "stream": "channel-2",
    });

    let binding = managed_stream_restart_cleanup_binding(&handle)
        .expect("startup probe should identify managed stream cleanup target");

    assert_eq!(binding.schema.as_deref(), Some("rtmp"));
    assert_eq!(binding.app, "preview");
    assert_eq!(binding.stream, "channel-2");
}

#[test]
fn managed_restart_cleanup_skips_zlm_proxy_ingest() {
    let mut handle = sticky_live_ingest_handle(true);
    handle.metadata["resolved_spec"]["input"] = json!({
        "kind": "rtsp",
        "source_mode": "live",
        "url": "rtsp://camera.example/live"
    });
    handle.metadata["stream_binding"] = json!({
        "schema": "rtmp",
        "vhost": "__defaultVhost__",
        "app": "preview",
        "stream": "channel-3",
    });

    assert!(managed_stream_restart_cleanup_binding(&handle).is_none());
}

#[test]
fn classify_adopted_exit_marks_unstopped_continuous_stream_exit_as_failed() {
    let mut handle = continuous_stream_ingest_handle();
    handle.state = RuntimeState::Exited;

    let (event_type, _, message, payload) =
        classify_adopted_exit(&handle, &SuccessCheck::ProcessExit, false);

    assert_eq!(event_type, "failed");
    assert_eq!(
        message,
        "adopted continuous stream_ingest process exited unexpectedly"
    );
    assert_eq!(payload["reason"], json!("unexpected_stream_exit"));
}

#[test]
fn live_relay_monitor_requires_consecutive_offline_before_failure() {
    let (polls, should_fail) = next_live_relay_offline_polls(0, true, Ok(false));
    assert_eq!(polls, 1);
    assert!(!should_fail);

    let (polls, should_fail) =
        next_live_relay_offline_polls(LIVE_STREAM_OFFLINE_GRACE_POLLS - 1, true, Ok(false));
    assert_eq!(polls, LIVE_STREAM_OFFLINE_GRACE_POLLS);
    assert!(should_fail);

    let (polls, should_fail) = next_live_relay_offline_polls(polls, true, Err(()));
    assert_eq!(polls, 0);
    assert!(!should_fail);
}

#[test]
fn source_reconnecting_event_dedupes_by_reason() {
    let mut handle = sticky_live_ingest_handle(true);
    assert!(should_emit_source_reconnecting(
        &handle,
        "source_disconnected"
    ));

    mark_source_reconnecting(&mut handle, "source_disconnected");
    assert!(!should_emit_source_reconnecting(
        &handle,
        "source_disconnected"
    ));
    assert!(should_emit_source_reconnecting(&handle, "startup_timeout"));

    clear_source_reconnecting(&mut handle);
    assert!(should_emit_source_reconnecting(
        &handle,
        "source_disconnected"
    ));
}

#[test]
fn recording_gap_tracks_recorded_sticky_reconnect_window() {
    let mut handle = sticky_live_recording_ingest_handle(true);
    assert!(should_emit_recording_gap_started(&handle));

    mark_source_reconnecting(&mut handle, "source_disconnected");
    assert!(recording_gap_active(&handle));
    assert!(!should_emit_recording_gap_started(&handle));
    assert_eq!(
        handle
            .metadata
            .get("recording_gap_reason")
            .and_then(Value::as_str),
        Some("source_disconnected")
    );

    clear_source_reconnecting(&mut handle);
    assert!(!recording_gap_active(&handle));
    assert!(
        handle
            .metadata
            .get("recording_gap_ended_at")
            .and_then(Value::as_str)
            .is_some()
    );
}

#[test]
fn open_rtp_server_params_can_be_rebuilt_from_runtime_metadata() {
    let params = build_open_rtp_server_params_from_metadata(&RtpServerMetadata {
        stream_id: "stream-1".to_string(),
        local_port: 15000,
        requested_port: 15000,
        tcp_mode: 1,
        reuse_port: Some(true),
        ssrc: Some(42),
    })
    .into_iter()
    .collect::<HashMap<_, _>>();

    assert_eq!(params.get("port").map(String::as_str), Some("15000"));
    assert_eq!(params.get("tcp_mode").map(String::as_str), Some("1"));
    assert_eq!(
        params.get("stream_id").map(String::as_str),
        Some("stream-1")
    );
    assert_eq!(params.get("re_use_port").map(String::as_str), Some("1"));
    assert_eq!(params.get("ssrc").map(String::as_str), Some("42"));
}

#[test]
fn rtp_receive_monitor_requires_consecutive_missing_before_failure() {
    let (polls, should_fail) = next_rtp_server_missing_polls(0, Ok(false));
    assert_eq!(polls, 1);
    assert!(!should_fail);

    let (polls, should_fail) =
        next_rtp_server_missing_polls(RTP_SERVER_MISSING_GRACE_POLLS - 1, Ok(false));
    assert_eq!(polls, RTP_SERVER_MISSING_GRACE_POLLS);
    assert!(should_fail);

    let (polls, should_fail) = next_rtp_server_missing_polls(polls, Err(()));
    assert_eq!(polls, 0);
    assert!(!should_fail);
}

#[test]
fn build_live_relay_plan_ignores_record_save_path_override() {
    let settings = test_settings("/tmp/work");
    let request = StartTaskRequest {
        task_id: Uuid::now_v7(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "relay-record-custom-path",
            "common": {"created_by": "tester"},
            "input": {"kind": "rtsp", "url": "rtsp://camera.example/live"},
            "publish": {},
            "record": {
                "enabled": true,
                "format": "hls",
                "save_path": "/var/media/archive/custom"
            },
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_live_relay_plan(&settings, &request, &spec).expect("plan should build");
    let recording = plan.recording.expect("recording should be present");

    assert_eq!(
        recording.root_path_hls.as_deref(),
        Some(
            managed_output_dir(&settings, request.task_id, "hls")
                .to_string_lossy()
                .as_ref()
        )
    );
    assert_eq!(recording.root_path_mp4, None);
}

#[test]
fn build_record_api_params_uses_expected_zlm_shape() {
    let binding = StreamBinding {
        schema: Some("rtmp".to_string()),
        vhost: "__defaultVhost__".to_string(),
        app: "relay".to_string(),
        stream: "stream-1".to_string(),
    };
    let recording = LiveRelayRecording {
        formats: vec![ZlmRecordKind::Mp4],
        root_path_mp4: Some("/var/media/archive".to_string()),
        root_path_hls: None,
        duration_sec: None,
        segment_sec: Some(90),
        as_player: false,
        desired_enabled: true,
        manual_control: false,
        stop_task_on_duration: true,
        control_command_id: None,
        recording_started_at: None,
        auto_stop_requested: false,
        completion_reason: None,
        started: false,
        failed: false,
    };

    let params = build_record_api_params(&binding, &recording, &ZlmRecordKind::Mp4)
        .into_iter()
        .collect::<HashMap<_, _>>();

    assert_eq!(params.get("type").map(String::as_str), Some("1"));
    assert_eq!(
        params.get("customized_path").map(String::as_str),
        Some("/var/media/archive")
    );
    assert_eq!(params.get("max_second").map(String::as_str), Some("90"));
    assert_eq!(params.get("schema").map(String::as_str), Some("rtmp"));
}

#[test]
fn build_record_api_params_defaults_mp4_to_task_duration() {
    let binding = StreamBinding {
        schema: Some("rtmp".to_string()),
        vhost: "__defaultVhost__".to_string(),
        app: "relay".to_string(),
        stream: "stream-1".to_string(),
    };
    let recording = LiveRelayRecording {
        formats: vec![ZlmRecordKind::Mp4],
        root_path_mp4: Some("/var/media/archive".to_string()),
        root_path_hls: None,
        duration_sec: Some(300),
        segment_sec: None,
        as_player: false,
        desired_enabled: true,
        manual_control: false,
        stop_task_on_duration: true,
        control_command_id: None,
        recording_started_at: None,
        auto_stop_requested: false,
        completion_reason: None,
        started: false,
        failed: false,
    };

    let params = build_record_api_params(&binding, &recording, &ZlmRecordKind::Mp4)
        .into_iter()
        .collect::<HashMap<_, _>>();

    assert_eq!(params.get("max_second").map(String::as_str), Some("300"));
}

#[test]
fn build_record_api_params_defaults_unbounded_mp4_to_two_hours() {
    let binding = StreamBinding {
        schema: None,
        vhost: "__defaultVhost__".to_string(),
        app: "relay".to_string(),
        stream: "stream-1".to_string(),
    };
    let recording = LiveRelayRecording {
        formats: vec![ZlmRecordKind::Mp4],
        root_path_mp4: Some("/var/media/archive".to_string()),
        root_path_hls: None,
        duration_sec: None,
        segment_sec: None,
        as_player: false,
        desired_enabled: true,
        manual_control: false,
        stop_task_on_duration: true,
        control_command_id: None,
        recording_started_at: None,
        auto_stop_requested: false,
        completion_reason: None,
        started: false,
        failed: false,
    };

    let params = build_record_api_params(&binding, &recording, &ZlmRecordKind::Mp4)
        .into_iter()
        .collect::<HashMap<_, _>>();

    assert_eq!(params.get("max_second").map(String::as_str), Some("7200"));
}

#[test]
fn build_rtp_receive_plan_uses_attempt_scoped_stream_id() {
    let settings = test_settings("/tmp/work");
    let task_id = Uuid::now_v7();
    let request = StartTaskRequest {
        task_id,
        attempt_no: 3,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "gb28181",
            "common": {"created_by": "tester"},
            "input": {"kind": "gb_rtp", "port": 0},
            "publish": {"enable_rtsp": true, "enable_rtmp": false},
            "record": {},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_rtp_receive_plan(&settings, &request, &spec).expect("plan should build");
    let params = build_open_rtp_server_params(&plan)
        .into_iter()
        .collect::<HashMap<_, _>>();
    let expected_stream_id = format!("{task_id}-3");

    assert_eq!(
        plan.command_line,
        format!(
            "zlm openRtpServer --port 0 --tcp_mode 0 --stream_id {}-3",
            task_id
        )
    );
    assert_eq!(params.get("port").map(String::as_str), Some("0"));
    assert_eq!(params.get("tcp_mode").map(String::as_str), Some("0"));
    assert_eq!(
        params.get("stream_id").map(String::as_str),
        Some(expected_stream_id.as_str())
    );
}

#[test]
fn build_rtp_receive_plan_maps_reuse_port_and_ssrc() {
    let settings = test_settings("/tmp/work");
    let request = StartTaskRequest {
        task_id: Uuid::now_v7(),
        attempt_no: 1,
        task_type: TaskType::StreamIngest,
        resolved_spec: json!({
            "type": "stream_ingest",
            "name": "gb28181",
            "common": {"created_by": "tester"},
            "input": {
                "kind": "gb_rtp",
                "port": 30000,
                "tcp_mode": 1,
                "reuse": true,
                "ssrc": 123456
            },
            "publish": {"enable_rtsp": true},
            "record": {},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        }),
        execution_mode: "managed".to_string(),
        lease_token: "lease".to_string(),
        trace_context: None,
        session_epoch: 1,
    };

    let spec = parse_task_spec(&request).expect("spec should parse");
    let plan = build_rtp_receive_plan(&settings, &request, &spec).expect("plan should build");
    let params = build_open_rtp_server_params(&plan)
        .into_iter()
        .collect::<HashMap<_, _>>();

    assert!(plan.command_line.contains("--re_use_port 1"));
    assert!(plan.command_line.contains("--ssrc 123456"));
    assert_eq!(params.get("re_use_port").map(String::as_str), Some("1"));
    assert_eq!(params.get("ssrc").map(String::as_str), Some("123456"));
}

#[test]
fn zlm_stream_online_in_body_matches_vhost_and_schema() {
    let body = json!({
        "code": 0,
        "data": [
            {
                "schema": "rtmp",
                "vhost": "__defaultVhost__",
                "app": "relay",
                "stream": "stream-1"
            }
        ]
    });
    let target = StartupProbe {
        schema: Some("rtmp".to_string()),
        vhost: "__defaultVhost__".to_string(),
        app: "relay".to_string(),
        stream: "stream-1".to_string(),
    };

    assert!(zlm_stream_status_in_body(&body, &target).is_some());
    assert!(
        zlm_stream_status_in_body(
            &body,
            &StartupProbe {
                schema: Some("rtsp".to_string()),
                ..target
            }
        )
        .is_none()
    );
}

#[test]
fn zlm_stream_online_in_body_allows_any_schema_when_probe_schema_is_absent() {
    let body = json!({
        "code": 0,
        "data": [
            {
                "schema": "rtmp",
                "vhost": "__defaultVhost__",
                "app": "relay",
                "stream": "stream-1"
            }
        ]
    });
    let target = StartupProbe {
        schema: None,
        vhost: "__defaultVhost__".to_string(),
        app: "relay".to_string(),
        stream: "stream-1".to_string(),
    };

    assert!(zlm_stream_status_in_body(&body, &target).is_some());
}

fn test_live_relay_recording() -> LiveRelayRecording {
    LiveRelayRecording {
        formats: vec![ZlmRecordKind::Mp4],
        root_path_mp4: Some("/var/media/archive".to_string()),
        root_path_hls: None,
        duration_sec: None,
        segment_sec: None,
        as_player: false,
        desired_enabled: true,
        manual_control: false,
        stop_task_on_duration: true,
        control_command_id: None,
        recording_started_at: None,
        auto_stop_requested: false,
        completion_reason: None,
        started: false,
        failed: false,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_probe_monitor_starts_managed_live_recording_without_keyframe_wait() {
    use axum::{
        Json, Router,
        extract::{Query, State},
        routing::get,
    };
    use std::{collections::HashMap, sync::Arc};
    use tokio::{
        net::TcpListener,
        sync::Mutex,
        time::{Duration, timeout},
    };

    #[derive(Clone)]
    struct StubState {
        calls: Arc<Mutex<Vec<(String, HashMap<String, String>)>>>,
    }

    async fn get_media_list(Query(_params): Query<HashMap<String, String>>) -> Json<Value> {
        Json(json!({
            "code": 0,
            "data": [{
                "schema": "rtmp",
                "vhost": "__defaultVhost__",
                "app": "objective",
                "stream": "objective-1",
                "tracks": [{
                    "codec_type": 0,
                    "ready": true,
                    "key_frames": 2,
                    "gop_interval_ms": 1500
                }]
            }]
        }))
    }

    async fn start_record(
        State(state): State<StubState>,
        Query(params): Query<HashMap<String, String>>,
    ) -> Json<Value> {
        state
            .calls
            .lock()
            .await
            .push(("startRecord".to_string(), params));
        Json(json!({"code": 0}))
    }

    let calls = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route("/index/api/getMediaList", get(get_media_list))
        .route("/index/api/startRecord", get(start_record))
        .with_state(StubState {
            calls: calls.clone(),
        });
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener addr should exist");
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("stub server should run");
    });

    let temp_root = std::env::temp_dir().join(format!(
        "streamserver-keyframe-startup-timeout-{}",
        Uuid::now_v7()
    ));
    let work_dir = temp_root.join("task").join("attempt-1");
    let registry = LocalRuntimeRegistry::new();
    let (priority_tx, mut priority_rx) = mpsc::unbounded_channel();
    let (log_tx, _log_rx) = mpsc::channel(8);
    let mut settings = test_settings(temp_root.to_string_lossy().as_ref());
    settings.zlm_api_base = format!("http://{addr}");
    settings.zlm_api_secret = "secret".to_string();

    let mut child = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .expect("sleep should spawn");
    let task_id = Uuid::now_v7();
    let runtime_id = Uuid::now_v7();
    let startup_probe = StartupProbe {
        schema: Some("rtmp".to_string()),
        vhost: "__defaultVhost__".to_string(),
        app: "objective".to_string(),
        stream: "objective-1".to_string(),
    };
    let resolved_spec = json!({
        "type": "stream_ingest",
        "name": "relay-record-only-http-ts",
        "common": {"created_by": "tester"},
        "input": {
            "kind": "http_ts",
            "source_mode": "live",
            "url": "http://camera.example/live.ts"
        },
        "stream": {"app": "objective", "name": "objective-1"},
        "expose": {
            "enable_rtsp": false,
            "enable_rtmp": false,
            "enable_http_ts": false,
            "enable_http_fmp4": false,
            "enable_hls": false
        },
        "record": {"enabled": true, "format": "mp4"},
        "recovery": {},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    });
    let handle = RuntimeHandle {
        runtime_id,
        task_id,
        attempt_no: 1,
        worker_kind: WorkerKind::Ffmpeg,
        pid: Some(child.id() as i32),
        started_at: Utc::now(),
        last_progress_at: None,
        state: RuntimeState::Starting,
        command_line: Some("ffmpeg -i input".to_string()),
        outputs: vec!["rtmp://127.0.0.1:1935/objective/objective-1".to_string()],
        metadata: json!({
            "task_type": "stream_ingest",
            "execution_mode": "managed",
            "lease_token": "lease",
            "resolved_spec": resolved_spec,
            "startup_probe": startup_probe,
            "recording": test_live_relay_recording(),
        }),
    };
    registry.track(handle);
    let runtimes = Arc::new(RwLock::new(HashMap::new()));
    runtimes.write().expect("runtime map lock poisoned").insert(
        runtime_id,
        ManagedRuntime {
            pid: Some(child.id() as i32),
            companion_pids: Vec::new(),
            _slot_permit: RuntimeSlotPermit::unbounded(),
            stop_requested: Arc::new(AtomicBool::new(false)),
            suppress_companion_events: Arc::new(AtomicBool::new(false)),
        },
    );

    spawn_startup_probe_monitor(
        runtime_id,
        work_dir.clone(),
        SuccessCheck::ProcessExit,
        StartupProbe {
            schema: Some("rtmp".to_string()),
            vhost: "__defaultVhost__".to_string(),
            app: "objective".to_string(),
            stream: "objective-1".to_string(),
        },
        settings,
        Client::new(),
        registry.clone(),
        runtimes,
        RuntimeEventSink::new(priority_tx, log_tx),
    );

    let mut seen_events = Vec::new();
    timeout(Duration::from_secs(6), async {
        loop {
            let notification = priority_rx
                .recv()
                .await
                .expect("event stream should stay open");
            if let RuntimeNotification::TaskEvent(event) = notification {
                if event.task_id != task_id {
                    continue;
                }
                seen_events.push(event.event_type.clone());
                if seen_events.contains(&"recording_started".to_string())
                    && seen_events.contains(&"running".to_string())
                {
                    break;
                }
            }
        }
    })
    .await
    .expect("startup probe should start recording immediately");

    let started_index = seen_events
        .iter()
        .position(|event| event == "recording_started")
        .expect("recording_started event should exist");
    let running_index = seen_events
        .iter()
        .position(|event| event == "running")
        .expect("running event should exist");
    assert!(started_index < running_index);

    let captured = calls.lock().await.clone();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].0, "startRecord");

    let updated_handle = registry
        .get(runtime_id)
        .expect("runtime handle should still exist");
    let recording = live_relay_recording_from_handle(&updated_handle)
        .expect("recording metadata should remain present");
    assert!(recording.started);
    assert!(recording.recording_started_at.is_some());
    assert_eq!(updated_handle.state, RuntimeState::Running);

    let _ = child.kill();
    let _ = child.wait();
    server.abort();
    let _ = fs::remove_dir_all(temp_root);
}

#[tokio::test]
async fn cleanup_live_relay_runtime_deletes_proxy_before_closing_stream() {
    use axum::{
        Json, Router,
        extract::{Query, State},
        routing::get,
    };
    use std::{collections::HashMap, sync::Arc};
    use tokio::{net::TcpListener, sync::Mutex};

    #[derive(Clone)]
    struct StubState {
        calls: Arc<Mutex<Vec<(String, HashMap<String, String>)>>>,
    }

    async fn del_stream_proxy(
        State(state): State<StubState>,
        Query(params): Query<HashMap<String, String>>,
    ) -> Json<Value> {
        state
            .calls
            .lock()
            .await
            .push(("delStreamProxy".to_string(), params));
        Json(json!({"code": 0}))
    }

    async fn close_streams(
        State(state): State<StubState>,
        Query(params): Query<HashMap<String, String>>,
    ) -> Json<Value> {
        state
            .calls
            .lock()
            .await
            .push(("close_streams".to_string(), params));
        Json(json!({"code": 0}))
    }

    let calls = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route("/index/api/delStreamProxy", get(del_stream_proxy))
        .route("/index/api/close_streams", get(close_streams))
        .with_state(StubState {
            calls: calls.clone(),
        });
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener addr should exist");
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("stub server should run");
    });

    let mut settings = test_settings("/tmp/work");
    settings.zlm_api_base = format!("http://{addr}");
    settings.zlm_api_secret = "secret".to_string();
    let handle = RuntimeHandle {
        runtime_id: Uuid::now_v7(),
        task_id: Uuid::now_v7(),
        attempt_no: 1,
        worker_kind: WorkerKind::ZlmProxy,
        pid: None,
        started_at: Utc::now(),
        last_progress_at: None,
        state: RuntimeState::Starting,
        command_line: Some("zlm addStreamProxy".to_string()),
        outputs: Vec::new(),
        metadata: json!({
            "zlm_proxy_key": "proxy-1"
        }),
    };
    let binding = StreamBinding {
        schema: Some("rtsp".to_string()),
        vhost: "__defaultVhost__".to_string(),
        app: "live".to_string(),
        stream: "camera01".to_string(),
    };

    cleanup_live_relay_runtime(&Client::new(), &settings, &handle, &binding).await;

    let captured = calls.lock().await.clone();
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[0].0, "delStreamProxy");
    assert_eq!(
        captured[0].1.get("key").map(String::as_str),
        Some("proxy-1")
    );
    assert_eq!(captured[1].0, "close_streams");
    assert_eq!(
        captured[1].1.get("stream").map(String::as_str),
        Some("camera01")
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_task_stops_managed_live_relay_recording_before_closing_stream() {
    use axum::{
        Json, Router,
        extract::{Query, State},
        routing::get,
    };
    use std::{collections::HashMap, sync::Arc};
    use tokio::{net::TcpListener, sync::Mutex};

    #[derive(Clone)]
    struct StubState {
        calls: Arc<Mutex<Vec<(String, HashMap<String, String>)>>>,
    }

    async fn stop_record(
        State(state): State<StubState>,
        Query(params): Query<HashMap<String, String>>,
    ) -> Json<Value> {
        state
            .calls
            .lock()
            .await
            .push(("stopRecord".to_string(), params));
        Json(json!({"code": 0}))
    }

    async fn close_streams(
        State(state): State<StubState>,
        Query(params): Query<HashMap<String, String>>,
    ) -> Json<Value> {
        state
            .calls
            .lock()
            .await
            .push(("close_streams".to_string(), params));
        Json(json!({"code": 0}))
    }

    let calls = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route("/index/api/stopRecord", get(stop_record))
        .route("/index/api/close_streams", get(close_streams))
        .with_state(StubState {
            calls: calls.clone(),
        });
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let addr = listener.local_addr().expect("listener addr should exist");
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("stub server should run");
    });

    let temp_root = std::env::temp_dir().join(format!("streamserver-stop-task-{}", Uuid::now_v7()));
    let registry = LocalRuntimeRegistry::new();
    let (priority_tx, _priority_rx) = mpsc::unbounded_channel();
    let (log_tx, _log_rx) = mpsc::channel(8);
    let mut settings = test_settings(temp_root.to_string_lossy().as_ref());
    settings.zlm_api_base = format!("http://{addr}");
    settings.zlm_api_secret = "secret".to_string();
    let executor = ManagedProcessExecutor::new(
        settings,
        registry.clone(),
        RuntimeEventSink::new(priority_tx, log_tx),
    );

    let mut child = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .expect("sleep should spawn");
    let task_id = Uuid::now_v7();
    let runtime_id = Uuid::now_v7();
    let started_at = Utc::now();
    let resolved_spec = json!({
        "type": "stream_ingest",
        "name": "relay-record-only-http-ts",
        "common": {"created_by": "tester"},
        "input": {
            "kind": "http_ts",
            "source_mode": "live",
            "url": "http://camera.example/live.ts"
        },
        "stream": {"app": "objective", "name": "objective-1"},
        "expose": {
            "enable_rtsp": false,
            "enable_rtmp": false,
            "enable_http_ts": false,
            "enable_http_fmp4": false,
            "enable_hls": false
        },
        "record": {"enabled": true, "format": "mp4"},
        "recovery": {},
        "schedule": {"start_mode": "immediate"},
        "resource": {}
    });
    let handle = RuntimeHandle {
        runtime_id,
        task_id,
        attempt_no: 1,
        worker_kind: WorkerKind::Ffmpeg,
        pid: Some(child.id() as i32),
        started_at,
        last_progress_at: None,
        state: RuntimeState::Running,
        command_line: Some("ffmpeg -i input".to_string()),
        outputs: vec!["rtmp://127.0.0.1/objective/objective-1".to_string()],
        metadata: json!({
            "task_type": "stream_ingest",
            "execution_mode": "managed",
            "lease_token": "lease",
            "resolved_spec": resolved_spec,
            "stream_binding": {
                "schema": "rtmp",
                "vhost": "__defaultVhost__",
                "app": "objective",
                "stream": "objective-1"
            },
            "recording": LiveRelayRecording {
                formats: vec![ZlmRecordKind::Mp4],
                root_path_mp4: Some("/data/zlm/www/output/mp4/task".to_string()),
                root_path_hls: None,
                duration_sec: None,
                segment_sec: None,
                as_player: false,
                desired_enabled: true,
                manual_control: false,
                stop_task_on_duration: true,
                control_command_id: None,
                recording_started_at: Some(started_at),
                auto_stop_requested: false,
                completion_reason: None,
                started: true,
                failed: false,
            }
        }),
    };
    registry.track(handle);
    executor
        .runtimes
        .write()
        .expect("runtime map lock poisoned")
        .insert(
            runtime_id,
            ManagedRuntime {
                pid: Some(child.id() as i32),
                companion_pids: Vec::new(),
                _slot_permit: RuntimeSlotPermit::unbounded(),
                stop_requested: Arc::new(AtomicBool::new(false)),
                suppress_companion_events: Arc::new(AtomicBool::new(false)),
            },
        );

    executor
        .stop_task(&StopTaskRequest {
            task_id,
            attempt_no: 1,
            lease_token: "lease".to_string(),
            reason: "user_requested".to_string(),
            grace_period_sec: 0,
            force_after_sec: 0,
        })
        .expect("stop should succeed");

    let status = child.wait().expect("sleep should exit after SIGTERM");
    assert!(!status.success());

    let captured = calls.lock().await.clone();
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[0].0, "stopRecord");
    assert_eq!(captured[0].1.get("type").map(String::as_str), Some("1"));
    assert_eq!(captured[1].0, "close_streams");
    assert_eq!(
        captured[1].1.get("stream").map(String::as_str),
        Some("objective-1")
    );

    server.abort();
    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn failed_live_relay_recording_is_not_retried() {
    assert!(!should_start_live_relay_recording(&LiveRelayRecording {
        formats: vec![ZlmRecordKind::Mp4],
        root_path_mp4: Some("/var/media/archive".to_string()),
        root_path_hls: None,
        duration_sec: None,
        segment_sec: None,
        as_player: false,
        desired_enabled: true,
        manual_control: false,
        stop_task_on_duration: true,
        control_command_id: None,
        recording_started_at: None,
        auto_stop_requested: false,
        completion_reason: None,
        started: false,
        failed: true,
    }));
}

#[test]
fn manually_disabled_recording_is_not_restarted() {
    let mut recording = test_live_relay_recording();
    recording.desired_enabled = false;
    recording.manual_control = true;

    assert!(!should_start_live_relay_recording(&recording));
}

#[test]
fn scan_persisted_runtimes_reads_runtime_state_files() {
    let temp_root = std::env::temp_dir().join(format!("streamserver-runtime-{}", Uuid::now_v7()));
    let work_dir = temp_root.join("task").join("attempt-1");
    let handle = RuntimeHandle {
        runtime_id: Uuid::now_v7(),
        task_id: Uuid::now_v7(),
        attempt_no: 1,
        worker_kind: WorkerKind::Ffmpeg,
        pid: Some(std::process::id() as i32),
        started_at: Utc::now(),
        last_progress_at: None,
        state: RuntimeState::Running,
        command_line: Some("ffmpeg -re -i input".to_string()),
        outputs: vec!["rtmp://127.0.0.1/live/stream".to_string()],
        metadata: json!({"task_type": "file_to_live", "lease_token": "lease"}),
    };

    persist_runtime_state(&work_dir, &handle, &SuccessCheck::ProcessExit)
        .expect("runtime state should persist");
    let scanned = scan_persisted_runtimes(temp_root.to_string_lossy().as_ref());

    assert_eq!(scanned.len(), 1);
    assert_eq!(scanned[0].handle.task_id, handle.task_id);
    assert_eq!(scanned[0].success_check, SuccessCheck::ProcessExit);

    let _ = fs::remove_dir_all(temp_root);
}

#[tokio::test]
async fn adopt_orphans_tracks_persisted_runtime() {
    let temp_root =
        std::env::temp_dir().join(format!("streamserver-adopt-runtime-{}", Uuid::now_v7()));
    let work_dir = temp_root.join("task").join("attempt-1");
    let handle = RuntimeHandle {
        runtime_id: Uuid::now_v7(),
        task_id: Uuid::now_v7(),
        attempt_no: 1,
        worker_kind: WorkerKind::Ffmpeg,
        pid: Some(std::process::id() as i32),
        started_at: Utc::now(),
        last_progress_at: None,
        state: RuntimeState::Running,
        command_line: Some("ffmpeg -re -i input".to_string()),
        outputs: vec!["rtmp://127.0.0.1/live/stream".to_string()],
        metadata: json!({"task_type": "file_to_live", "lease_token": "lease"}),
    };

    persist_runtime_state(&work_dir, &handle, &SuccessCheck::ProcessExit)
        .expect("runtime state should persist");

    let registry = LocalRuntimeRegistry::new();
    let (priority_tx, _priority_rx) = mpsc::unbounded_channel();
    let (log_tx, _log_rx) = mpsc::channel(8);
    let mut settings = test_settings(temp_root.to_string_lossy().as_ref());
    settings.max_runtime_slots = 1;
    let executor = ManagedProcessExecutor::new(
        settings,
        registry.clone(),
        RuntimeEventSink::new(priority_tx, log_tx),
    );

    let adopted = executor.adopt_orphans(&AdoptFilter {
        session_epoch: 1,
        runtimes: vec![AdoptRuntimeFilter {
            task_id: handle.task_id,
            attempt_no: handle.attempt_no,
            lease_token: "lease".to_string(),
            worker_kind: WorkerKind::Ffmpeg,
        }],
    });

    assert_eq!(adopted.len(), 1);
    assert_eq!(adopted[0].state, RuntimeState::Orphaned);
    assert!(
        registry
            .find_by_task_attempt(handle.task_id, handle.attempt_no)
            .is_some()
    );
    let follow_up = executor
        .start_task(&StartTaskRequest {
            task_id: Uuid::now_v7(),
            attempt_no: 1,
            task_type: TaskType::FileTranscode,
            resolved_spec: json!({
                "type": "file_transcode",
                "name": "adopted-capacity",
                "common": {"created_by": "tester"},
                "input": {"kind": "file", "url": "input.mp4"},
                "process": {"mode": "copy_or_transcode"},
                "record": {},
                "publish": {
                    "kind": "file",
                    "url": "/data/zlm/www/artifacts/transcode/output.mp4"
                },
                "recovery": {},
                "schedule": {"start_mode": "immediate"},
                "resource": {}
            }),
            execution_mode: "managed".to_string(),
            lease_token: "lease-2".to_string(),
            trace_context: None,
            session_epoch: 1,
        })
        .expect_err("adopted runtime should consume a slot");
    assert!(matches!(
        follow_up,
        ExecutorError::InvalidRequest(message) if message.contains("max_runtime_slots")
    ));

    let _ = fs::remove_dir_all(temp_root);
}

#[tokio::test]
async fn adopt_orphans_emits_adopted_for_active_registry_runtime() {
    let temp_root =
        std::env::temp_dir().join(format!("streamserver-adopt-active-{}", Uuid::now_v7()));
    let registry = LocalRuntimeRegistry::new();
    let (priority_tx, mut priority_rx) = mpsc::unbounded_channel();
    let (log_tx, _log_rx) = mpsc::channel(8);
    let settings = test_settings(temp_root.to_string_lossy().as_ref());
    let executor = ManagedProcessExecutor::new(
        settings,
        registry.clone(),
        RuntimeEventSink::new(priority_tx, log_tx),
    );
    let handle = RuntimeHandle {
        runtime_id: Uuid::now_v7(),
        task_id: Uuid::now_v7(),
        attempt_no: 1,
        worker_kind: WorkerKind::Ffmpeg,
        pid: Some(std::process::id() as i32),
        started_at: Utc::now(),
        last_progress_at: None,
        state: RuntimeState::Starting,
        command_line: Some("ffmpeg -re -i input".to_string()),
        outputs: vec!["rtmp://127.0.0.1/live/stream".to_string()],
        metadata: json!({
            "task_type": "file_to_live",
            "lease_token": "lease",
        }),
    };
    registry.track(handle.clone());
    executor
        .runtimes
        .write()
        .expect("runtime map lock poisoned")
        .insert(
            handle.runtime_id,
            ManagedRuntime {
                pid: handle.pid,
                companion_pids: Vec::new(),
                _slot_permit: RuntimeSlotPermit::unbounded(),
                stop_requested: Arc::new(AtomicBool::new(false)),
                suppress_companion_events: Arc::new(AtomicBool::new(false)),
            },
        );

    let adopted = executor.adopt_orphans(&AdoptFilter {
        session_epoch: 7,
        runtimes: vec![AdoptRuntimeFilter {
            task_id: handle.task_id,
            attempt_no: handle.attempt_no,
            lease_token: "lease".to_string(),
            worker_kind: WorkerKind::Ffmpeg,
        }],
    });

    assert_eq!(adopted.len(), 1);
    assert_eq!(runtime_session_epoch(&adopted[0]), 7);
    let event = priority_rx
        .try_recv()
        .expect("active registry adoption should emit an adopted event");
    let RuntimeNotification::TaskEvent(event) = event else {
        panic!("expected adopted task event");
    };
    assert_eq!(event.task_id, handle.task_id);
    assert_eq!(event.attempt_no, handle.attempt_no);
    assert_eq!(event.event_type, "adopted");
    assert_eq!(event.session_epoch, 7);

    let _ = fs::remove_dir_all(temp_root);
}

#[tokio::test]
async fn stale_attempt_cleanup_signals_lower_registry_attempt() {
    let temp_root =
        std::env::temp_dir().join(format!("streamserver-stale-registry-{}", Uuid::now_v7()));
    let registry = LocalRuntimeRegistry::new();
    let (priority_tx, _priority_rx) = mpsc::unbounded_channel();
    let (log_tx, _log_rx) = mpsc::channel(8);
    let executor = ManagedProcessExecutor::new(
        test_settings(temp_root.to_string_lossy().as_ref()),
        registry.clone(),
        RuntimeEventSink::new(priority_tx, log_tx),
    );
    let mut child = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .expect("sleep should spawn");
    let task_id = Uuid::now_v7();
    let runtime_id = Uuid::now_v7();
    let handle = RuntimeHandle {
        runtime_id,
        task_id,
        attempt_no: 1,
        worker_kind: WorkerKind::Ffmpeg,
        pid: Some(child.id() as i32),
        started_at: Utc::now(),
        last_progress_at: None,
        state: RuntimeState::Starting,
        command_line: Some("ffmpeg -re -i input".to_string()),
        outputs: vec!["rtmp://127.0.0.1/live/stream".to_string()],
        metadata: json!({"task_type": "file_to_live", "lease_token": "old-lease"}),
    };
    registry.track(handle);
    executor
        .runtimes
        .write()
        .expect("runtime map lock poisoned")
        .insert(
            runtime_id,
            ManagedRuntime {
                pid: Some(child.id() as i32),
                companion_pids: Vec::new(),
                _slot_permit: RuntimeSlotPermit::unbounded(),
                stop_requested: Arc::new(AtomicBool::new(false)),
                suppress_companion_events: Arc::new(AtomicBool::new(false)),
            },
        );

    cleanup_stale_attempt_runtimes(
        StaleAttemptCleanupContext {
            settings: &executor.settings,
            registry: &executor.registry,
            runtimes: &executor.runtimes,
        },
        &StartTaskRequest {
            task_id,
            attempt_no: 2,
            task_type: TaskType::FileTranscode,
            resolved_spec: Value::Null,
            execution_mode: "managed".to_string(),
            lease_token: "new-lease".to_string(),
            trace_context: None,
            session_epoch: 1,
        },
    );

    let status = timeout(Duration::from_secs(2), async {
        loop {
            if let Some(status) = child.try_wait().expect("sleep status should be readable") {
                break status;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("stale registry process should exit after SIGTERM");
    assert!(!status.success());
    let updated = registry
        .get(runtime_id)
        .expect("stale runtime should remain tracked until monitor removes it");
    assert_eq!(updated.state, RuntimeState::Stopping);

    let _ = fs::remove_dir_all(temp_root);
}

#[tokio::test]
async fn stale_attempt_cleanup_signals_lower_persisted_attempt() {
    let temp_root =
        std::env::temp_dir().join(format!("streamserver-stale-persisted-{}", Uuid::now_v7()));
    let work_dir = temp_root.join("task").join("attempt-1");
    let registry = LocalRuntimeRegistry::new();
    let (priority_tx, _priority_rx) = mpsc::unbounded_channel();
    let (log_tx, _log_rx) = mpsc::channel(8);
    let executor = ManagedProcessExecutor::new(
        test_settings(temp_root.to_string_lossy().as_ref()),
        registry,
        RuntimeEventSink::new(priority_tx, log_tx),
    );
    let mut child = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .expect("sleep should spawn");
    let task_id = Uuid::now_v7();
    let handle = RuntimeHandle {
        runtime_id: Uuid::now_v7(),
        task_id,
        attempt_no: 1,
        worker_kind: WorkerKind::Ffmpeg,
        pid: Some(child.id() as i32),
        started_at: Utc::now(),
        last_progress_at: None,
        state: RuntimeState::Starting,
        command_line: Some("ffmpeg -re -i input".to_string()),
        outputs: vec!["rtmp://127.0.0.1/live/stream".to_string()],
        metadata: json!({"task_type": "file_to_live", "lease_token": "old-lease"}),
    };
    persist_runtime_state(&work_dir, &handle, &SuccessCheck::ProcessExit)
        .expect("runtime state should persist");

    cleanup_stale_attempt_runtimes(
        StaleAttemptCleanupContext {
            settings: &executor.settings,
            registry: &executor.registry,
            runtimes: &executor.runtimes,
        },
        &StartTaskRequest {
            task_id,
            attempt_no: 2,
            task_type: TaskType::FileTranscode,
            resolved_spec: Value::Null,
            execution_mode: "managed".to_string(),
            lease_token: "new-lease".to_string(),
            trace_context: None,
            session_epoch: 1,
        },
    );

    let status = timeout(Duration::from_secs(2), async {
        loop {
            if let Some(status) = child.try_wait().expect("sleep status should be readable") {
                break status;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("stale persisted process should exit after SIGTERM");
    assert!(!status.success());

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn bounded_log_batches_splits_and_truncates_large_payloads() {
    let batch = RuntimeTaskLogBatch {
        task_id: Uuid::now_v7(),
        attempt_no: 1,
        lease_token: "lease".to_string(),
        session_epoch: 1,
        stream: "stderr".to_string(),
        lines: vec![
            "x".repeat(MAX_LOG_BATCH_BYTES + 1024),
            "y".repeat(MAX_LOG_BATCH_BYTES / 2),
        ],
        source_line_count: 2,
    };

    let batches = bounded_log_batches(batch);

    assert!(batches.len() >= 2);
    assert_eq!(
        batches
            .iter()
            .map(|batch| batch.source_line_count)
            .sum::<usize>(),
        2
    );
    assert!(batches.iter().all(
        |batch| batch.lines.iter().map(|line| line.len() + 1).sum::<usize>() <= MAX_LOG_BATCH_BYTES
    ));
    assert!(batches[0].lines[0].contains("[truncated]"));
}

#[tokio::test]
async fn runtime_event_sink_summarizes_dropped_log_lines() {
    let (priority_tx, _priority_rx) = mpsc::unbounded_channel();
    let (log_tx, mut log_rx) = mpsc::channel(1);
    let sink = RuntimeEventSink::new(priority_tx, log_tx);
    let task_id = Uuid::now_v7();

    assert!(
        sink.send(RuntimeNotification::TaskLogBatch(RuntimeTaskLogBatch {
            task_id,
            attempt_no: 1,
            lease_token: "lease".to_string(),
            session_epoch: 1,
            stream: "stderr".to_string(),
            lines: vec!["first".to_string()],
            source_line_count: 1,
        }))
        .is_ok()
    );
    assert!(
        sink.send(RuntimeNotification::TaskLogBatch(RuntimeTaskLogBatch {
            task_id,
            attempt_no: 1,
            lease_token: "lease".to_string(),
            session_epoch: 1,
            stream: "stderr".to_string(),
            lines: vec!["dropped".to_string()],
            source_line_count: 3,
        }))
        .is_ok()
    );

    let first = log_rx.recv().await.expect("first batch should be queued");
    assert_eq!(first.lines, vec!["first".to_string()]);

    assert!(
        sink.send(RuntimeNotification::TaskLogBatch(RuntimeTaskLogBatch {
            task_id,
            attempt_no: 1,
            lease_token: "lease".to_string(),
            session_epoch: 1,
            stream: "stderr".to_string(),
            lines: vec!["after".to_string()],
            source_line_count: 1,
        }))
        .is_ok()
    );

    let second = log_rx.recv().await.expect("second batch should be queued");
    assert_eq!(
        second.lines,
        vec![
            "suppressed 3 stderr log lines".to_string(),
            "after".to_string()
        ]
    );
}

#[tokio::test]
async fn runtime_event_sink_bounds_large_log_batches() {
    let (priority_tx, _priority_rx) = mpsc::unbounded_channel();
    let (log_tx, mut log_rx) = mpsc::channel(8);
    let sink = RuntimeEventSink::new(priority_tx, log_tx);

    assert!(
        sink.send(RuntimeNotification::TaskLogBatch(RuntimeTaskLogBatch {
            task_id: Uuid::now_v7(),
            attempt_no: 1,
            lease_token: "lease".to_string(),
            session_epoch: 1,
            stream: "stderr".to_string(),
            lines: vec![
                "x".repeat(MAX_LOG_BATCH_BYTES + 1024),
                "y".repeat(MAX_LOG_BATCH_BYTES / 2),
            ],
            source_line_count: 2,
        }))
        .is_ok()
    );

    let first = log_rx.recv().await.expect("first bounded batch");
    let second = log_rx.recv().await.expect("second bounded batch");
    for batch in [first, second] {
        let bytes = batch.lines.iter().map(|line| line.len() + 1).sum::<usize>();
        assert!(bytes <= MAX_LOG_BATCH_BYTES);
    }
}

#[test]
fn collect_terminal_runtime_replays_only_replays_stopped_exited_runtimes() {
    let temp_root =
        std::env::temp_dir().join(format!("streamserver-terminal-replay-{}", Uuid::now_v7()));
    let stopped_dir = temp_root.join("stopped").join("attempt-1");
    let completed_dir = temp_root.join("completed").join("attempt-1");

    let stopped_handle = RuntimeHandle {
        runtime_id: Uuid::now_v7(),
        task_id: Uuid::now_v7(),
        attempt_no: 1,
        worker_kind: WorkerKind::ZlmProxy,
        pid: Some(1234),
        started_at: Utc::now(),
        last_progress_at: Some(Utc::now()),
        state: RuntimeState::Exited,
        command_line: Some("ffmpeg -i input".to_string()),
        outputs: vec!["rtmp://127.0.0.1/live/test".to_string()],
        metadata: json!({
            "task_type": "stream_ingest",
            "stop": {
                "reason": "user_requested"
            }
        }),
    };
    let completed_handle = RuntimeHandle {
        runtime_id: Uuid::now_v7(),
        task_id: Uuid::now_v7(),
        attempt_no: 1,
        worker_kind: WorkerKind::ZlmProxy,
        pid: Some(5678),
        started_at: Utc::now(),
        last_progress_at: Some(Utc::now()),
        state: RuntimeState::Exited,
        command_line: Some("ffmpeg -i input".to_string()),
        outputs: vec!["rtmp://127.0.0.1/live/test".to_string()],
        metadata: json!({
            "task_type": "stream_ingest"
        }),
    };

    persist_runtime_state(&stopped_dir, &stopped_handle, &SuccessCheck::ProcessExit)
        .expect("stopped runtime should persist");
    persist_runtime_state(
        &completed_dir,
        &completed_handle,
        &SuccessCheck::ProcessExit,
    )
    .expect("completed runtime should persist");

    let replays = collect_terminal_runtime_replays(
        temp_root.to_string_lossy().as_ref(),
        &LocalRuntimeRegistry::new(),
    );

    assert_eq!(replays.len(), 1);
    assert_eq!(replays[0].handle.task_id, stopped_handle.task_id);
    assert_eq!(replays[0].event.event_type, "canceled");

    let _ = fs::remove_dir_all(temp_root);
}

#[test]
fn cleanup_persisted_runtime_state_removes_runtime_files() {
    let temp_root =
        std::env::temp_dir().join(format!("streamserver-runtime-cleanup-{}", Uuid::now_v7()));
    let task_id = Uuid::parse_str("019d8631-7061-71b3-a9ca-95874bddeb55").unwrap();
    let work_dir = temp_root.join(task_id.to_string()).join("attempt-2");
    let handle = RuntimeHandle {
        runtime_id: Uuid::now_v7(),
        task_id,
        attempt_no: 2,
        worker_kind: WorkerKind::ZlmProxy,
        pid: Some(4321),
        started_at: Utc::now(),
        last_progress_at: None,
        state: RuntimeState::Exited,
        command_line: Some("ffmpeg -i input".to_string()),
        outputs: vec!["rtmp://127.0.0.1/live/test".to_string()],
        metadata: json!({"task_type": "stream_ingest"}),
    };

    persist_runtime_state(&work_dir, &handle, &SuccessCheck::ProcessExit)
        .expect("runtime should persist");
    cleanup_persisted_runtime_state(
        temp_root.to_string_lossy().as_ref(),
        handle.task_id,
        handle.attempt_no,
    );

    assert!(!work_dir.join(RUNTIME_STATE_FILE).exists());
    assert!(!work_dir.join(RUNTIME_PID_FILE).exists());
    assert!(!work_dir.join(RUNTIME_COMMAND_FILE).exists());

    let _ = fs::remove_dir_all(temp_root);
}
