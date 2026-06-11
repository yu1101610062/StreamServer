use super::*;

fn sample_task(task_type: TaskType) -> TaskSpec {
    TaskSpec {
        task_type,
        name: "relay-camera-01".to_string(),
        priority: 50,
        common: CommonSpec {
            created_by: Some("alice".to_string()),
            callback_url: None,
            labels: Vec::new(),
        },
        input: InputSpec {
            kind: Some(InputKind::Rtsp),
            url: Some("rtsp://camera.example/live".to_string()),
            source_mode: Some(SourceMode::Live),
            ..InputSpec::default()
        },
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

fn disable_all_playback_expose(task: &mut TaskSpec) {
    task.expose.enable_rtsp = Some(false);
    task.expose.enable_rtmp = Some(false);
    task.expose.enable_http_ts = Some(false);
    task.expose.enable_http_fmp4 = Some(false);
    task.expose.enable_hls = Some(false);
}

#[test]
fn resolve_applies_documented_defaults() {
    let resolved = sample_task(TaskType::StreamIngest).resolved();

    assert_eq!(resolved.stream.app.as_deref(), Some("live"));
    assert_eq!(resolved.stream.vhost.as_deref(), Some("__defaultVhost__"));
    assert_eq!(resolved.expose.enable_rtsp, Some(true));
    assert_eq!(resolved.expose.enable_rtmp, Some(true));
    assert_eq!(resolved.expose.enable_http_ts, Some(true));
    assert_eq!(resolved.expose.enable_http_fmp4, Some(true));
    assert_eq!(resolved.expose.enable_hls, Some(false));
    assert_eq!(resolved.expose.stop_on_no_reader, Some(false));
    assert_eq!(resolved.record.enabled, Some(false));
    assert_eq!(resolved.record.save_path, None);
    assert_eq!(resolved.input.loop_enabled, Some(false));
    assert_eq!(resolved.recovery.policy, Some(RecoveryPolicy::Auto));
    assert_eq!(resolved.schedule.start_mode, Some(StartMode::Immediate));
}

#[test]
fn resolve_ignores_stream_ingest_record_save_path_override() {
    let mut task = sample_task(TaskType::StreamIngest);
    task.record.enabled = Some(true);
    task.record.save_path = Some("/data/zlm/www/record/custom".to_string());

    let resolved = task.resolved();

    assert_eq!(resolved.record.enabled, Some(true));
    assert_eq!(resolved.record.save_path, None);
}

#[test]
fn resolved_stream_ingest_live_falls_back_to_http_fmp4_without_recording() {
    let mut task = sample_task(TaskType::StreamIngest);
    task.record.enabled = Some(false);
    disable_all_playback_expose(&mut task);

    let resolved = task.resolved();

    assert_eq!(resolved.expose.enable_rtsp, Some(false));
    assert_eq!(resolved.expose.enable_rtmp, Some(false));
    assert_eq!(resolved.expose.enable_http_ts, Some(false));
    assert_eq!(resolved.expose.enable_http_fmp4, Some(true));
    assert_eq!(resolved.expose.enable_hls, Some(false));
}

#[test]
fn resolved_stream_ingest_live_falls_back_to_http_fmp4_with_recording() {
    let mut task = sample_task(TaskType::StreamIngest);
    task.record.enabled = Some(true);
    disable_all_playback_expose(&mut task);

    let resolved = task.resolved();

    assert_eq!(resolved.expose.enable_rtsp, Some(false));
    assert_eq!(resolved.expose.enable_rtmp, Some(false));
    assert_eq!(resolved.expose.enable_http_ts, Some(false));
    assert_eq!(resolved.expose.enable_http_fmp4, Some(true));
    assert_eq!(resolved.expose.enable_hls, Some(false));
}

#[test]
fn resolved_stream_ingest_vod_record_mode_defaults_to_realtime_when_playback_is_exposed() {
    let mut task = sample_task(TaskType::StreamIngest);
    task.input.kind = Some(InputKind::HttpMp4);
    task.input.source_mode = Some(SourceMode::Vod);
    task.input.url = Some("http://vod.example.com/archive.mp4".to_string());
    task.record.enabled = Some(true);

    let resolved = task.resolved();

    assert_eq!(
        resolved.stream_ingest_record_mode(),
        Some(StreamIngestRecordMode::Realtime)
    );
}

#[test]
fn resolved_stream_ingest_vod_record_mode_becomes_fast_when_all_playback_is_disabled() {
    let mut task = sample_task(TaskType::StreamIngest);
    task.input.kind = Some(InputKind::HttpMp4);
    task.input.source_mode = Some(SourceMode::Vod);
    task.input.url = Some("http://vod.example.com/archive.mp4".to_string());
    task.record.enabled = Some(true);
    disable_all_playback_expose(&mut task);

    let resolved = task.resolved();

    assert!(!resolved.expose.any_playback_enabled());
    assert_eq!(
        resolved.stream_ingest_record_mode(),
        Some(StreamIngestRecordMode::Fast)
    );
}

#[test]
fn resolve_defaults_ftp_input_to_vod() {
    let mut task = sample_task(TaskType::FileTranscode);
    task.input.kind = Some(InputKind::Ftp);
    task.input.source_mode = None;
    task.input.url = Some("ftp://vod.example.com/archive/demo.mp4".to_string());
    task.publish.kind = Some(PublishTargetKind::File);

    let resolved = task.resolved();

    assert_eq!(resolved.input.source_mode, Some(SourceMode::Vod));
}

#[test]
fn resolve_normalizes_file_input_relative_path() {
    let mut task = sample_task(TaskType::FileTranscode);
    task.input.kind = Some(InputKind::File);
    task.input.source_mode = Some(SourceMode::Vod);
    task.input.url = Some("///vod/./demo.ts".to_string());
    task.publish.kind = Some(PublishTargetKind::File);

    let resolved = task.resolved();

    assert_eq!(resolved.input.url.as_deref(), Some("vod/demo.ts"));
}

#[test]
fn file_transcode_defaults_to_auto_recovery() {
    let resolved = sample_task(TaskType::FileTranscode).resolved();
    assert_eq!(resolved.recovery.policy, Some(RecoveryPolicy::Auto));
}

#[test]
fn live_stream_ingest_without_recording_uses_sticky_reconnect_by_default() {
    let resolved = sample_task(TaskType::StreamIngest).resolved();

    assert!(resolved.stream_ingest_uses_sticky_reconnect());
}

#[test]
fn sticky_reconnect_allows_unbounded_live_recording() {
    let mut recording_task = sample_task(TaskType::StreamIngest);
    recording_task.record.enabled = Some(true);
    assert!(
        recording_task
            .resolved()
            .stream_ingest_uses_sticky_reconnect()
    );

    recording_task.record.duration_sec = Some(60);
    assert!(
        !recording_task
            .resolved()
            .stream_ingest_uses_sticky_reconnect()
    );
}

#[test]
fn sticky_reconnect_respects_recovery_opt_out() {
    let mut opt_out_task = sample_task(TaskType::StreamIngest);
    opt_out_task.recovery.policy = Some(RecoveryPolicy::Never);
    assert!(
        !opt_out_task
            .resolved()
            .stream_ingest_uses_sticky_reconnect()
    );
}

#[test]
fn vod_loop_playback_ingest_uses_sticky_reconnect_until_duration() {
    let mut task = sample_task(TaskType::StreamIngest);
    task.input.kind = Some(InputKind::HttpMp4);
    task.input.source_mode = Some(SourceMode::Vod);
    task.input.loop_enabled = Some(true);
    task.record.enabled = Some(true);
    task.expose.enable_rtsp = Some(true);

    assert!(task.resolved().stream_ingest_uses_sticky_reconnect());

    task.record.duration_sec = Some(60);
    assert!(!task.resolved().stream_ingest_uses_sticky_reconnect());

    task.record.duration_sec = None;
    disable_all_playback_expose(&mut task);
    assert!(!task.resolved().stream_ingest_uses_sticky_reconnect());
}

#[test]
fn recovery_policy_deserializes_legacy_aliases() {
    let on_failure: RecoveryPolicy = serde_json::from_str("\"on_failure\"").unwrap();
    let always: RecoveryPolicy = serde_json::from_str("\"always\"").unwrap();
    assert_eq!(on_failure, RecoveryPolicy::Auto);
    assert_eq!(always, RecoveryPolicy::Auto);
}

#[test]
fn validate_rejects_missing_input_and_creator() {
    let task = TaskSpec {
        task_type: TaskType::StreamIngest,
        name: " ".to_string(),
        priority: 101,
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

    let error = task.validate().expect_err("validation should fail");
    assert!(error.issues.iter().any(|issue| issue.field == "name"));
    assert!(
        error
            .issues
            .iter()
            .any(|issue| issue.field == "common.created_by")
    );
    assert!(error.issues.iter().any(|issue| issue.field == "input.kind"));
}

#[test]
fn validate_rejects_task_name_with_whitespace() {
    let mut task = sample_task(TaskType::StreamIngest);
    task.name = "black new".to_string();

    let error = task.validate().expect_err("validation should fail");

    assert!(
        error.issues.iter().any(|issue| {
            issue.field == "name" && issue.message == "must not contain whitespace"
        })
    );
}

#[test]
fn validate_rejects_stream_routing_fields_with_whitespace() {
    let mut task = sample_task(TaskType::StreamIngest);
    task.stream.app = Some("live app".to_string());
    task.stream.name = Some("black new".to_string());
    task.stream.vhost = Some("__default Vhost__".to_string());

    let error = task.validate().expect_err("validation should fail");

    for field in ["stream.app", "stream.name", "stream.vhost"] {
        assert!(error.issues.iter().any(|issue| {
            issue.field == field && issue.message == "must not contain whitespace when provided"
        }));
    }
}

#[test]
fn validate_allows_stream_bridge_multicast_input_without_explicit_interface_binding() {
    let task = TaskSpec {
        task_type: TaskType::StreamBridge,
        name: "bridge".to_string(),
        priority: 50,
        common: CommonSpec {
            created_by: Some("alice".to_string()),
            callback_url: None,
            labels: Vec::new(),
        },
        input: InputSpec {
            kind: Some(InputKind::UdpMpegtsMulticast),
            group: Some("239.0.0.1".to_string()),
            port: Some(1234),
            source_mode: Some(SourceMode::Live),
            ..InputSpec::default()
        },
        stream: StreamSpec::default(),
        expose: ExposeSpec::default(),
        process: ProcessSpec::default(),
        publish: PublishSpec {
            kind: Some(PublishTargetKind::File),
            ..PublishSpec::default()
        },
        record: RecordSpec::default(),
        recovery: RecoverySpec::default(),
        schedule: ScheduleSpec::default(),
        resource: ResourceSpec::default(),
    };

    task.validate()
        .expect("validation should allow agent-level multicast defaults");
}

#[test]
fn validate_rejects_stream_ingest_with_publish_settings() {
    let mut task = sample_task(TaskType::StreamIngest);
    task.publish.kind = Some(PublishTargetKind::File);
    task.publish.url = Some("/tmp/out.ts".to_string());

    let error = task.validate().expect_err("validation should fail");
    assert!(
        error
            .issues
            .iter()
            .any(|issue| issue.field == "publish.kind")
    );
}

#[test]
fn validate_allows_stream_ingest_vod_input_looping() {
    let task = TaskSpec {
        task_type: TaskType::StreamIngest,
        name: "vod-loop".to_string(),
        priority: 50,
        common: CommonSpec {
            created_by: Some("alice".to_string()),
            callback_url: None,
            labels: Vec::new(),
        },
        input: InputSpec {
            kind: Some(InputKind::HttpTs),
            source_mode: Some(SourceMode::Vod),
            loop_enabled: Some(true),
            url: Some("http://vod.example.com/archive.ts".to_string()),
            ..InputSpec::default()
        },
        stream: StreamSpec::default(),
        expose: ExposeSpec::default(),
        process: ProcessSpec::default(),
        publish: PublishSpec::default(),
        record: RecordSpec::default(),
        recovery: RecoverySpec::default(),
        schedule: ScheduleSpec::default(),
        resource: ResourceSpec::default(),
    };

    task.validate()
        .expect("validation should allow looping vod ingest input");
}

#[test]
fn validate_rejects_loop_enabled_for_live_input() {
    let mut task = sample_task(TaskType::StreamIngest);
    task.input.loop_enabled = Some(true);

    let error = task.validate().expect_err("validation should fail");
    assert!(
        error
            .issues
            .iter()
            .any(|issue| issue.field == "input.loop_enabled")
    );
}

#[test]
fn validate_rejects_fast_record_loop_without_duration() {
    let mut task = sample_task(TaskType::StreamIngest);
    task.input.kind = Some(InputKind::HttpMp4);
    task.input.source_mode = Some(SourceMode::Vod);
    task.input.loop_enabled = Some(true);
    task.input.url = Some("http://vod.example.com/archive.mp4".to_string());
    task.record.enabled = Some(true);
    disable_all_playback_expose(&mut task);

    let error = task.validate().expect_err("validation should fail");
    assert!(
        error
            .issues
            .iter()
            .any(|issue| issue.field == "record.duration_sec")
    );
}

#[test]
fn validate_rejects_stream_bridge_without_publish_target() {
    let task = TaskSpec {
        task_type: TaskType::StreamBridge,
        name: "bridge".to_string(),
        priority: 50,
        common: CommonSpec {
            created_by: Some("alice".to_string()),
            callback_url: None,
            labels: Vec::new(),
        },
        input: InputSpec {
            kind: Some(InputKind::UdpMpegtsMulticast),
            group: Some("239.0.0.1".to_string()),
            port: Some(1234),
            source_mode: Some(SourceMode::Live),
            ..InputSpec::default()
        },
        stream: StreamSpec::default(),
        expose: ExposeSpec::default(),
        process: ProcessSpec::default(),
        publish: PublishSpec::default(),
        record: RecordSpec::default(),
        recovery: RecoverySpec::default(),
        schedule: ScheduleSpec::default(),
        resource: ResourceSpec::default(),
    };

    let error = task.validate().expect_err("validation should fail");
    assert!(
        error
            .issues
            .iter()
            .any(|issue| issue.field == "publish.kind")
    );
}

#[test]
fn validate_rejects_stream_bridge_gb_rtp_input() {
    let task = TaskSpec {
        task_type: TaskType::StreamBridge,
        name: "bridge".to_string(),
        priority: 50,
        common: CommonSpec {
            created_by: Some("alice".to_string()),
            callback_url: None,
            labels: Vec::new(),
        },
        input: InputSpec {
            kind: Some(InputKind::GbRtp),
            source_mode: Some(SourceMode::Live),
            port: Some(30000),
            ..InputSpec::default()
        },
        stream: StreamSpec::default(),
        expose: ExposeSpec::default(),
        process: ProcessSpec::default(),
        publish: PublishSpec {
            kind: Some(PublishTargetKind::UdpMpegtsMulticast),
            group: Some("239.0.0.1".to_string()),
            port: Some(1234),
            ..PublishSpec::default()
        },
        record: RecordSpec::default(),
        recovery: RecoverySpec::default(),
        schedule: ScheduleSpec::default(),
        resource: ResourceSpec::default(),
    };

    let error = task.validate().expect_err("validation should fail");
    assert!(error.issues.iter().any(|issue| issue.field == "input.kind"));
}

#[test]
fn validate_rejects_stream_bridge_vod_file_output() {
    let task = TaskSpec {
        task_type: TaskType::StreamBridge,
        name: "vod-bridge".to_string(),
        priority: 50,
        common: CommonSpec {
            created_by: Some("alice".to_string()),
            callback_url: None,
            labels: Vec::new(),
        },
        input: InputSpec {
            kind: Some(InputKind::HttpMp4),
            source_mode: Some(SourceMode::Vod),
            url: Some("http://vod.example.com/archive.mp4".to_string()),
            ..InputSpec::default()
        },
        stream: StreamSpec::default(),
        expose: ExposeSpec::default(),
        process: ProcessSpec::default(),
        publish: PublishSpec {
            kind: Some(PublishTargetKind::File),
            ..PublishSpec::default()
        },
        record: RecordSpec::default(),
        recovery: RecoverySpec::default(),
        schedule: ScheduleSpec::default(),
        resource: ResourceSpec::default(),
    };

    let error = task.validate().expect_err("validation should fail");
    assert!(
        error
            .issues
            .iter()
            .any(|issue| issue.field == "publish.kind")
    );
}

#[test]
fn validate_rejects_loop_enabled_for_non_ingest_tasks() {
    let task = TaskSpec {
        task_type: TaskType::StreamBridge,
        name: "bridge-loop".to_string(),
        priority: 50,
        common: CommonSpec {
            created_by: Some("alice".to_string()),
            callback_url: None,
            labels: Vec::new(),
        },
        input: InputSpec {
            kind: Some(InputKind::HttpMp4),
            source_mode: Some(SourceMode::Vod),
            loop_enabled: Some(true),
            url: Some("http://vod.example.com/archive.mp4".to_string()),
            ..InputSpec::default()
        },
        stream: StreamSpec::default(),
        expose: ExposeSpec::default(),
        process: ProcessSpec::default(),
        publish: PublishSpec {
            kind: Some(PublishTargetKind::RtmpPush),
            url: Some("rtmp://push.example.com/live/stream01".to_string()),
            ..PublishSpec::default()
        },
        record: RecordSpec::default(),
        recovery: RecoverySpec::default(),
        schedule: ScheduleSpec::default(),
        resource: ResourceSpec::default(),
    };

    let error = task.validate().expect_err("validation should fail");
    assert!(
        error
            .issues
            .iter()
            .any(|issue| issue.field == "input.loop_enabled")
    );
}

#[test]
fn validate_allows_file_transcode_with_http_mp4_vod_input() {
    let task = TaskSpec {
        task_type: TaskType::FileTranscode,
        name: "file-transcode".to_string(),
        priority: 50,
        common: CommonSpec {
            created_by: Some("alice".to_string()),
            callback_url: None,
            labels: Vec::new(),
        },
        input: InputSpec {
            kind: Some(InputKind::HttpMp4),
            source_mode: Some(SourceMode::Vod),
            url: Some("http://vod.example.com/archive.mp4".to_string()),
            ..InputSpec::default()
        },
        stream: StreamSpec::default(),
        expose: ExposeSpec::default(),
        process: ProcessSpec::default(),
        publish: PublishSpec {
            kind: Some(PublishTargetKind::File),
            ..PublishSpec::default()
        },
        record: RecordSpec::default(),
        recovery: RecoverySpec::default(),
        schedule: ScheduleSpec::default(),
        resource: ResourceSpec::default(),
    };

    task.validate()
        .expect("validation should allow http_mp4 file_transcode input");
}

#[test]
fn validate_rejects_webm_publish_output_format() {
    let mut task = sample_task(TaskType::FileTranscode);
    task.input.kind = Some(InputKind::File);
    task.input.source_mode = Some(SourceMode::Vod);
    task.input.url = Some("input.mp4".to_string());
    task.publish.kind = Some(PublishTargetKind::File);
    task.publish.format = Some("webm".to_string());

    let error = task.validate().expect_err("validation should fail");

    assert!(
        error.issues.iter().any(|issue| {
            issue.field == "publish.format" && issue.message.contains("webm output")
        })
    );
}

#[test]
fn validate_allows_file_transcode_with_ftp_vod_input() {
    let task = TaskSpec {
        task_type: TaskType::FileTranscode,
        name: "file-transcode".to_string(),
        priority: 50,
        common: CommonSpec {
            created_by: Some("alice".to_string()),
            callback_url: None,
            labels: Vec::new(),
        },
        input: InputSpec {
            kind: Some(InputKind::Ftp),
            source_mode: Some(SourceMode::Vod),
            url: Some("ftp://vod.example.com/archive.mp4".to_string()),
            ..InputSpec::default()
        },
        stream: StreamSpec::default(),
        expose: ExposeSpec::default(),
        process: ProcessSpec::default(),
        publish: PublishSpec {
            kind: Some(PublishTargetKind::File),
            ..PublishSpec::default()
        },
        record: RecordSpec::default(),
        recovery: RecoverySpec::default(),
        schedule: ScheduleSpec::default(),
        resource: ResourceSpec::default(),
    };

    task.validate()
        .expect("validation should allow ftp file_transcode input");
}

#[test]
fn validate_allows_stream_ingest_with_ftp_vod_input() {
    let mut task = sample_task(TaskType::StreamIngest);
    task.input.kind = Some(InputKind::Ftp);
    task.input.source_mode = Some(SourceMode::Vod);
    task.input.url = Some("ftp://vod.example.com/archive.ts".to_string());

    task.validate()
        .expect("validation should allow ftp ingest input");
}

#[test]
fn validate_rejects_ftp_with_live_source_mode() {
    let mut task = sample_task(TaskType::StreamIngest);
    task.input.kind = Some(InputKind::Ftp);
    task.input.source_mode = Some(SourceMode::Live);
    task.input.url = Some("ftp://vod.example.com/archive.ts".to_string());

    let error = task.validate().expect_err("validation should fail");

    assert!(error.issues.iter().any(|issue| {
        issue.field == "input.source_mode" && issue.message == "ftp input requires source_mode=vod"
    }));
}

#[test]
fn validate_rejects_ftps_input_url() {
    let mut task = sample_task(TaskType::StreamIngest);
    task.input.kind = Some(InputKind::Ftp);
    task.input.source_mode = Some(SourceMode::Vod);
    task.input.url = Some("ftps://vod.example.com/archive.ts".to_string());

    let error = task.validate().expect_err("validation should fail");

    assert!(error.issues.iter().any(|issue| {
        issue.field == "input.url" && issue.message == "ftps:// is not supported; use ftp://"
    }));
}

#[test]
fn validate_rejects_hls_without_source_mode() {
    let task = TaskSpec {
        task_type: TaskType::StreamIngest,
        name: "hls-ingest".to_string(),
        priority: 50,
        common: CommonSpec {
            created_by: Some("alice".to_string()),
            callback_url: None,
            labels: Vec::new(),
        },
        input: InputSpec {
            kind: Some(InputKind::Hls),
            url: Some("http://vod.example.com/index.m3u8".to_string()),
            ..InputSpec::default()
        },
        stream: StreamSpec::default(),
        expose: ExposeSpec::default(),
        process: ProcessSpec::default(),
        publish: PublishSpec::default(),
        record: RecordSpec::default(),
        recovery: RecoverySpec::default(),
        schedule: ScheduleSpec::default(),
        resource: ResourceSpec::default(),
    };

    let error = task.validate().expect_err("validation should fail");
    assert!(
        error
            .issues
            .iter()
            .any(|issue| issue.field == "input.source_mode")
    );
}

#[test]
fn validate_rejects_record_duration_for_unsupported_task_types() {
    let mut task = sample_task(TaskType::StreamBridge);
    task.publish.kind = Some(PublishTargetKind::File);
    task.record.enabled = Some(true);
    task.record.duration_sec = Some(300);

    let error = task.validate().expect_err("validation should fail");
    assert!(
        error
            .issues
            .iter()
            .any(|issue| issue.field == "record.duration_sec")
    );
}

#[test]
fn validate_rejects_stream_bridge_file_publish_url_override() {
    let mut task = sample_task(TaskType::StreamBridge);
    task.publish.kind = Some(PublishTargetKind::File);
    task.publish.url = Some("/data/zlm/www/artifacts/bridge/out.mp4".to_string());

    let error = task.validate().expect_err("validation should fail");
    assert!(
        error
            .issues
            .iter()
            .any(|issue| issue.field == "publish.url")
    );
}

#[test]
fn validate_allows_stream_bridge_rtmp_push_output() {
    let mut task = sample_task(TaskType::StreamBridge);
    task.publish.kind = Some(PublishTargetKind::RtmpPush);
    task.publish.url = Some("rtmp://push.example.com/live/stream01".to_string());

    task.validate()
        .expect("validation should allow rtmp_push output");
}

#[test]
fn validate_allows_stream_bridge_rtmps_push_output() {
    let mut task = sample_task(TaskType::StreamBridge);
    task.publish.kind = Some(PublishTargetKind::RtmpPush);
    task.publish.url = Some("rtmps://push.example.com/live/stream01".to_string());

    task.validate()
        .expect("validation should allow rtmps push output");
}

#[test]
fn validate_rejects_stream_bridge_rtmp_push_without_url() {
    let mut task = sample_task(TaskType::StreamBridge);
    task.publish.kind = Some(PublishTargetKind::RtmpPush);

    let error = task.validate().expect_err("validation should fail");
    assert!(
        error
            .issues
            .iter()
            .any(|issue| issue.field == "publish.url")
    );
}

#[test]
fn validate_rejects_stream_bridge_rtmp_push_with_non_rtmp_scheme() {
    let mut task = sample_task(TaskType::StreamBridge);
    task.publish.kind = Some(PublishTargetKind::RtmpPush);
    task.publish.url = Some("http://push.example.com/live/stream01".to_string());

    let error = task.validate().expect_err("validation should fail");
    assert!(
        error
            .issues
            .iter()
            .any(|issue| issue.field == "publish.url")
    );
}

#[test]
fn validate_rejects_stream_bridge_rtmp_push_with_non_flv_format() {
    let mut task = sample_task(TaskType::StreamBridge);
    task.publish.kind = Some(PublishTargetKind::RtmpPush);
    task.publish.url = Some("rtmp://push.example.com/live/stream01".to_string());
    task.publish.format = Some("mp4".to_string());

    let error = task.validate().expect_err("validation should fail");
    assert!(
        error
            .issues
            .iter()
            .any(|issue| issue.field == "publish.format")
    );
}

#[test]
fn validate_rejects_stream_bridge_rtmp_push_with_multicast_fields() {
    let mut task = sample_task(TaskType::StreamBridge);
    task.publish.kind = Some(PublishTargetKind::RtmpPush);
    task.publish.url = Some("rtmp://push.example.com/live/stream01".to_string());
    task.publish.group = Some("239.0.0.1".to_string());
    task.publish.port = Some(1234);

    let error = task.validate().expect_err("validation should fail");
    assert!(
        error
            .issues
            .iter()
            .any(|issue| issue.field == "publish.group")
    );
    assert!(
        error
            .issues
            .iter()
            .any(|issue| issue.field == "publish.port")
    );
}

#[test]
fn validate_rejects_file_transcode_publish_url_override() {
    let mut task = sample_task(TaskType::FileTranscode);
    task.input.kind = Some(InputKind::File);
    task.input.source_mode = Some(SourceMode::Vod);
    task.publish.kind = Some(PublishTargetKind::File);
    task.publish.url = Some("/data/zlm/www/artifacts/transcode/out.mp4".to_string());

    let error = task.validate().expect_err("validation should fail");
    assert!(
        error
            .issues
            .iter()
            .any(|issue| issue.field == "publish.url")
    );
}

#[test]
fn validate_rejects_non_positive_record_duration() {
    let mut task = sample_task(TaskType::StreamIngest);
    task.record.enabled = Some(true);
    task.record.duration_sec = Some(0);

    let error = task.validate().expect_err("validation should fail");
    assert!(
        error
            .issues
            .iter()
            .any(|issue| issue.field == "record.duration_sec")
    );
}

#[test]
fn resolve_defaults_gb_rtp_tcp_mode_to_udp() {
    let task = TaskSpec {
        task_type: TaskType::StreamIngest,
        name: "rtp-recv".to_string(),
        priority: 50,
        common: CommonSpec {
            created_by: Some("alice".to_string()),
            callback_url: None,
            labels: Vec::new(),
        },
        input: InputSpec {
            kind: Some(InputKind::GbRtp),
            port: Some(0),
            ..InputSpec::default()
        },
        stream: StreamSpec::default(),
        expose: ExposeSpec::default(),
        process: ProcessSpec::default(),
        publish: PublishSpec::default(),
        record: RecordSpec::default(),
        recovery: RecoverySpec::default(),
        schedule: ScheduleSpec::default(),
        resource: ResourceSpec::default(),
    };

    let resolved = task.resolved();
    assert_eq!(resolved.input.tcp_mode, Some(0));
}

#[test]
fn resolve_does_not_inject_ingest_only_defaults_into_stream_bridge() {
    let mut task = sample_task(TaskType::StreamBridge);
    task.publish.kind = Some(PublishTargetKind::RtmpPush);
    task.publish.url = Some("rtmp://push.example.com/live/stream01".to_string());

    let resolved = task.resolved();

    assert_eq!(resolved.stream, StreamSpec::default());
    assert_eq!(resolved.expose, ExposeSpec::default());
    assert_eq!(resolved.record, RecordSpec::default());
}

#[test]
fn validate_rejects_file_transcode_with_stream_settings() {
    let task = TaskSpec {
        task_type: TaskType::FileTranscode,
        name: "file-transcode".to_string(),
        priority: 50,
        common: CommonSpec {
            created_by: Some("alice".to_string()),
            callback_url: None,
            labels: Vec::new(),
        },
        input: InputSpec {
            kind: Some(InputKind::File),
            source_mode: Some(SourceMode::Vod),
            url: Some("input.mp4".to_string()),
            ..InputSpec::default()
        },
        stream: StreamSpec {
            name: Some("should-not-be-here".to_string()),
            ..StreamSpec::default()
        },
        expose: ExposeSpec::default(),
        process: ProcessSpec::default(),
        publish: PublishSpec {
            kind: Some(PublishTargetKind::File),
            url: Some("/tmp/out.mp4".to_string()),
            ..PublishSpec::default()
        },
        record: RecordSpec::default(),
        recovery: RecoverySpec::default(),
        schedule: ScheduleSpec::default(),
        resource: ResourceSpec::default(),
    };

    let error = task.validate().expect_err("validation should fail");
    assert!(error.issues.iter().any(|issue| issue.field == "stream"));
}

#[test]
fn validate_allows_file_input_with_leading_slash() {
    let mut task = sample_task(TaskType::StreamIngest);
    task.input.kind = Some(InputKind::File);
    task.input.source_mode = Some(SourceMode::Vod);
    task.input.url = Some("/demo.mp4".to_string());

    task.validate()
        .expect("validation should accept a file input path with a leading slash");
}

#[test]
fn validate_rejects_file_input_with_parent_dir() {
    let mut task = sample_task(TaskType::StreamIngest);
    task.input.kind = Some(InputKind::File);
    task.input.source_mode = Some(SourceMode::Vod);
    task.input.url = Some("../demo.mp4".to_string());

    let error = task.validate().expect_err("validation should fail");
    assert!(error.issues.iter().any(|issue| {
        issue.field == "input.url" && issue.message.contains("must not contain '..' segments")
    }));
}

#[test]
fn validate_rejects_file_input_with_url_value() {
    let mut task = sample_task(TaskType::StreamIngest);
    task.input.kind = Some(InputKind::File);
    task.input.source_mode = Some(SourceMode::Vod);
    task.input.url = Some("http://example.com/demo.mp4".to_string());

    let error = task.validate().expect_err("validation should fail");
    assert!(
        error
            .issues
            .iter()
            .any(|issue| { issue.field == "input.url" && issue.message.contains("not a URL") })
    );
}
