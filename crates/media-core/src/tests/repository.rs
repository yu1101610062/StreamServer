use super::*;

#[test]
fn build_resolved_task_json_applies_request_defaults() {
    let merged = build_resolved_task_json(
        TaskType::StreamIngest,
        &json!({
            "name": "relay-camera-01",
            "common": {
                "created_by": "alice"
            },
            "input": {
                "kind": "rtsp",
                "source_mode": "live",
                "url": "rtsp://camera.example/live"
            },
            "expose": {
                "enable_rtsp": true
            }
        }),
    )
    .expect("merged json should build");

    let spec: TaskSpec = serde_json::from_value(merged).expect("task spec should parse");
    let resolved = spec.resolved();

    assert_eq!(resolved.process.mode, None);
    assert_eq!(resolved.expose.enable_rtsp, Some(true));
    assert_eq!(resolved.expose.enable_hls, Some(false));
    assert_eq!(resolved.record.enabled, Some(false));
}

#[test]
fn task_summary_transcode_mode_marks_live_rtsp_ingest_as_non_transcode() {
    let spec: TaskSpec = serde_json::from_value(json!({
        "type": "stream_ingest",
        "name": "relay-camera-01",
        "input": {
            "kind": "rtsp",
            "source_mode": "live",
            "url": "rtsp://camera.example/live"
        }
    }))
    .expect("spec should parse");

    assert_eq!(
        task_summary_transcode_mode(&spec),
        Some(TASK_TRANSCODE_NONE)
    );
}

#[test]
fn task_summary_transcode_mode_defaults_file_transcode_to_adaptive() {
    let spec: TaskSpec = serde_json::from_value(json!({
        "type": "file_transcode",
        "name": "transcode-archive",
        "input": {
            "kind": "file",
            "source_mode": "vod",
            "url": "archive/demo.mp4"
        },
        "publish": {
            "kind": "file"
        }
    }))
    .expect("spec should parse");

    assert_eq!(
        task_summary_transcode_mode(&spec),
        Some(TASK_TRANSCODE_ADAPTIVE)
    );
}

#[test]
fn task_summary_transcode_mode_marks_mpegts_bridge_stabilization_as_forced() {
    let spec: TaskSpec = serde_json::from_value(json!({
        "type": "stream_bridge",
        "name": "bridge-live-to-mcast",
        "input": {
            "kind": "rtsp",
            "source_mode": "live",
            "url": "rtsp://camera.example/live"
        },
        "publish": {
            "kind": "udp_mpegts_multicast",
            "group": "239.0.0.10",
            "port": 1234
        },
        "process": {
            "mode": "passthrough"
        }
    }))
    .expect("spec should parse");

    assert_eq!(
        task_summary_transcode_mode(&spec),
        Some(TASK_TRANSCODE_FORCED)
    );
}

#[test]
fn task_spec_overlay_skips_empty_option_fields() {
    let spec = TaskSpec {
        task_type: TaskType::StreamIngest,
        name: "relay-camera-01".to_string(),
        priority: 50,
        common: media_domain::CommonSpec {
            created_by: Some("alice".to_string()),
            callback_url: None,
            labels: Vec::new(),
        },
        input: media_domain::InputSpec {
            kind: Some(media_domain::InputKind::Rtsp),
            source_mode: Some(media_domain::SourceMode::Live),
            url: Some("rtsp://camera.example/live".to_string()),
            ..Default::default()
        },
        stream: Default::default(),
        expose: Default::default(),
        process: Default::default(),
        publish: Default::default(),
        record: Default::default(),
        recovery: Default::default(),
        schedule: Default::default(),
        resource: Default::default(),
    };

    let overlay = task_spec_overlay(&spec);

    assert_eq!(overlay["common"]["created_by"], json!("alice"));
    assert!(overlay["publish"].is_null());
}

#[test]
fn task_spec_overlay_preserves_record_duration_sec() {
    let mut spec = TaskSpec {
        task_type: TaskType::StreamIngest,
        name: "duration-check".to_string(),
        priority: 50,
        common: media_domain::CommonSpec {
            created_by: Some("alice".to_string()),
            callback_url: None,
            labels: Vec::new(),
        },
        input: media_domain::InputSpec {
            kind: Some(media_domain::InputKind::HttpMp4),
            source_mode: Some(media_domain::SourceMode::Vod),
            url: Some("http://127.0.0.1/test.mp4".to_string()),
            ..Default::default()
        },
        stream: Default::default(),
        expose: Default::default(),
        process: Default::default(),
        publish: Default::default(),
        record: Default::default(),
        recovery: Default::default(),
        schedule: Default::default(),
        resource: Default::default(),
    };
    spec.record.enabled = Some(true);
    spec.record.duration_sec = Some(300);

    let overlay = task_spec_overlay(&spec);

    assert_eq!(overlay["record"]["duration_sec"], json!(300));
}

#[test]
fn task_spec_overlay_preserves_input_loop_enabled() {
    let spec = TaskSpec {
        task_type: TaskType::StreamIngest,
        name: "loop-check".to_string(),
        priority: 50,
        common: media_domain::CommonSpec {
            created_by: Some("alice".to_string()),
            callback_url: None,
            labels: Vec::new(),
        },
        input: media_domain::InputSpec {
            kind: Some(media_domain::InputKind::HttpMp4),
            source_mode: Some(media_domain::SourceMode::Vod),
            loop_enabled: Some(true),
            url: Some("http://127.0.0.1/test.mp4".to_string()),
            ..Default::default()
        },
        stream: Default::default(),
        expose: Default::default(),
        process: Default::default(),
        publish: Default::default(),
        record: Default::default(),
        recovery: Default::default(),
        schedule: Default::default(),
        resource: Default::default(),
    };

    let overlay = task_spec_overlay(&spec);

    assert_eq!(overlay["input"]["loop_enabled"], json!(true));
}

#[test]
fn artifact_http_url_from_path_uses_node_stream_base() {
    let url = artifact_http_url_from_path(
        "http://192.168.1.10:8081",
        "/data/zlm/www/output/mp4/node-192_168_1_10-mp4/task-1/clip.mp4",
    )
    .expect("artifact url should build");

    assert_eq!(
        url,
        "http://192.168.1.10:8081/output/mp4/node-192_168_1_10-mp4/task-1/clip.mp4"
    );
}

#[test]
fn record_http_url_from_path_uses_web_root_directly() {
    let url = record_http_url_from_path(
        "http://192.168.1.10:8081",
        "/data/zlm/www/output/mp4/node-192_168_1_10-mp4/task-1/clip.mp4",
    )
    .expect("record url should build");

    assert_eq!(
        url,
        "http://192.168.1.10:8081/output/mp4/node-192_168_1_10-mp4/task-1/clip.mp4"
    );
}

#[test]
fn externalize_managed_path_strips_mount_roots() {
    let prefixes = OutputMountPrefixes {
        mp4: "output/mp4".to_string(),
        hls: "output/hls".to_string(),
    };
    assert_eq!(
        externalize_managed_path(
            "/data/zlm/www/output/mp4/node-192_168_1_10-mp4/task-1/clip.mp4",
            "file_path",
            &prefixes,
        )
        .expect("mp4 path should externalize"),
        "/node-192_168_1_10-mp4/task-1/clip.mp4"
    );
    assert_eq!(
        externalize_managed_path(
            "/data/zlm/www/output/hls/node-192_168_1_10-hls/task-1/index.m3u8",
            "file_path",
            &prefixes,
        )
        .expect("hls path should externalize"),
        "/node-192_168_1_10-hls/task-1/index.m3u8"
    );
}

#[test]
fn resolve_absolute_http_url_accepts_relative_paths() {
    let url = resolve_absolute_http_url(
        "http://worker.example:8081",
        "/output/hls/node-192_168_1_10-hls/task-1/index.m3u8",
    )
    .expect("relative hook url should resolve");
    assert_eq!(
        url,
        "http://worker.example:8081/output/hls/node-192_168_1_10-hls/task-1/index.m3u8"
    );
}

#[test]
fn is_hls_playlist_record_path_accepts_record_root_m3u8_only() {
    assert!(is_hls_playlist_record_path(
        "/data/zlm/www/output/hls/node-192_168_1_10-hls/task-1/index.m3u8"
    ));
    assert!(!is_hls_playlist_record_path(
        "/data/zlm/www/output/mp4/node-192_168_1_10-mp4/task-1/clip.mp4"
    ));
    assert!(!is_hls_playlist_record_path(
        "/data/zlm/www/output/hls/node-192_168_1_10-hls/task-1/index-00001.ts"
    ));
}

#[test]
fn externalize_path_fields_in_payload_rewrites_file_path_and_folder() {
    let prefixes = OutputMountPrefixes {
        mp4: "output/mp4".to_string(),
        hls: "output/hls".to_string(),
    };
    let payload = externalize_path_fields_in_payload(
        json!({
            "file_path": "/data/zlm/www/output/hls/node-192_168_1_10-hls/task-1/index.m3u8",
            "folder": "/data/zlm/www/output/hls/node-192_168_1_10-hls/task-1",
            "records": [
                {
                    "file_path": "/data/zlm/www/output/mp4/node-192_168_1_10-mp4/task-1/out.mp4",
                    "folder": "/data/zlm/www/output/mp4/node-192_168_1_10-mp4/task-1"
                }
            ]
        }),
        Some(&prefixes),
    )
    .expect("payload should externalize");

    assert_eq!(
        payload["file_path"],
        json!("/node-192_168_1_10-hls/task-1/index.m3u8")
    );
    assert_eq!(payload["folder"], json!("/node-192_168_1_10-hls/task-1"));
    assert_eq!(
        payload["records"][0]["file_path"],
        json!("/node-192_168_1_10-mp4/task-1/out.mp4")
    );
    assert_eq!(
        payload["records"][0]["folder"],
        json!("/node-192_168_1_10-mp4/task-1")
    );
}

#[test]
fn should_persist_record_file_hook_only_keeps_hls_record_playlists() {
    let binding = HookStreamBinding {
        task_id: Uuid::now_v7(),
        attempt_id: Uuid::now_v7(),
        attempt_no: 1,
        resolved_spec: Some(json!({
            "type": "stream_ingest",
            "name": "record-hls",
            "common": {"created_by": "tester"},
            "input": {"kind": "rtsp", "url": "rtsp://camera/live"},
            "stream": {"app": "live", "name": "camera01"},
            "expose": {
                "enable_rtsp": false,
                "enable_rtmp": false,
                "enable_http_ts": false,
                "enable_http_fmp4": false,
                "enable_hls": false
            },
            "process": {"mode": "copy_or_transcode"},
            "record": {"enabled": true, "format": "hls"},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        })),
        started_at: None,
        ended_at: None,
    };
    let playlist = ZlmRecordFileRecord {
        record_format: Some("hls".to_string()),
        schema: None,
        vhost: "__defaultVhost__".to_string(),
        app: "live".to_string(),
        stream: "camera01".to_string(),
        file_path: "/data/zlm/www/record/live/camera01/index.m3u8".to_string(),
        file_size: 1024,
        time_len_sec: Some(30),
        start_time: None,
        file_name: Some("index.m3u8".to_string()),
        folder: Some("/data/zlm/www/record/live/camera01".to_string()),
        url: Some("http://stream.example/record/live/camera01/index.m3u8".to_string()),
    };
    let segment = ZlmRecordFileRecord {
        file_path: "/data/zlm/www/record/live/camera01/index-00001.ts".to_string(),
        file_name: Some("index-00001.ts".to_string()),
        ..playlist.clone()
    };

    assert!(
        should_persist_record_file_hook("on_record_hls", &binding, &playlist)
            .expect("playlist should evaluate")
    );
    assert!(
        !should_persist_record_file_hook("on_record_ts", &binding, &segment)
            .expect("segment should evaluate")
    );

    let exposed_only_binding = HookStreamBinding {
        resolved_spec: Some(json!({
            "type": "stream_ingest",
            "name": "expose-hls",
            "common": {"created_by": "tester"},
            "input": {"kind": "rtsp", "url": "rtsp://camera/live"},
            "stream": {"app": "live", "name": "camera01"},
            "expose": {
                "enable_rtsp": false,
                "enable_rtmp": false,
                "enable_http_ts": false,
                "enable_http_fmp4": false,
                "enable_hls": true
            },
            "process": {"mode": "copy_or_transcode"},
            "record": {"enabled": false},
            "recovery": {},
            "schedule": {"start_mode": "immediate"},
            "resource": {}
        })),
        ..binding
    };
    let exposed_playlist = ZlmRecordFileRecord {
        file_path: "/data/zlm/www/live/camera01/hls.m3u8".to_string(),
        file_name: Some("hls.m3u8".to_string()),
        folder: Some("/data/zlm/www/live/camera01".to_string()),
        url: Some("http://stream.example/live/camera01/hls.m3u8".to_string()),
        ..playlist
    };

    assert!(
        !should_persist_record_file_hook("on_record_hls", &exposed_only_binding, &exposed_playlist)
            .expect("exposed playlist should evaluate")
    );
}

#[test]
fn validate_managed_file_publish_target_rejects_file_path_override() {
    let spec: TaskSpec = serde_json::from_value(json!({
        "type": "file_transcode",
        "name": "artifact-test",
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
    }))
    .expect("task spec should parse");

    let error =
        validate_managed_file_publish_target(&spec).expect_err("invalid output should reject");
    assert!(matches!(error, RepoError::Validation(_)));
}
