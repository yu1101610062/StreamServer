use std::path::PathBuf;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::get,
};
use media_gateway::{GatewayConfig, GatewayState, build_app, safe_target_path};
use serde_json::{Value, json};
use tower::util::ServiceExt;

#[tokio::test]
async fn relay_requires_registered_task_and_token() -> anyhow::Result<()> {
    async fn upstream() -> &'static str {
        "relay-bytes"
    }

    let upstream = spawn_server(Router::new().route("/live.flv", get(upstream))).await?;
    let temp = test_temp_dir()?;
    let state = GatewayState::new(GatewayConfig {
        public_base_url: "http://media:18080".to_string(),
        work_root: temp.clone(),
        ffmpeg_bin: PathBuf::from("ffmpeg"),
    });
    let app = build_app(state);
    let task_id = uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000111")?;

    let create = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/relays")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "task_id": task_id,
                        "source_url": format!("{upstream}/live.flv")
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(create.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&to_bytes(create.into_body(), usize::MAX).await?)?;
    let relay_url = body["relay_url"].as_str().expect("relay_url");
    assert!(relay_url.starts_with("http://media:18080/relay/"));
    let token = relay_url.split("token=").nth(1).expect("token");

    let rejected = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/relay/{task_id}?url={upstream}/live.flv"))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);

    let accepted = app
        .oneshot(
            Request::builder()
                .uri(format!("/relay/{task_id}?token={token}"))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(accepted.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(accepted.into_body(), usize::MAX).await?.as_ref(),
        b"relay-bytes"
    );
    Ok(())
}

#[test]
fn prefetch_target_path_stays_under_work_root_and_not_uploads() -> anyhow::Result<()> {
    let temp = test_temp_dir()?;

    assert_eq!(
        safe_target_path(&temp, "imports/task/source.mp4")?,
        temp.join("imports/task/source.mp4")
    );
    assert!(safe_target_path(&temp, "../source.mp4").is_err());
    assert!(safe_target_path(&temp, "/tmp/source.mp4").is_err());
    assert!(safe_target_path(&temp, "uploads/node/source.mp4").is_err());
    Ok(())
}

#[tokio::test]
async fn prefetch_downloads_http_source_to_shared_storage_path() -> anyhow::Result<()> {
    async fn source() -> &'static str {
        "prefetch-bytes"
    }

    let upstream = spawn_server(Router::new().route("/archive.mp4", get(source))).await?;
    let temp = test_temp_dir()?;
    let state = GatewayState::new(GatewayConfig {
        public_base_url: "http://media:18080".to_string(),
        work_root: temp.clone(),
        ffmpeg_bin: PathBuf::from("ffmpeg"),
    });
    let app = build_app(state);
    let task_id = uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000222")?;

    let created = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/prefetch")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "task_id": task_id,
                        "source_url": format!("{upstream}/archive.mp4"),
                        "target_path": "imports/00000000-0000-0000-0000-000000000222/source.mp4"
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(created.status(), StatusCode::ACCEPTED);

    let status = wait_prefetch_ready(app, task_id).await?;
    assert_eq!(status["status"], "ready");
    assert_eq!(
        status["source_url"],
        "imports/00000000-0000-0000-0000-000000000222/source.mp4"
    );
    assert_eq!(
        std::fs::read(temp.join("imports/00000000-0000-0000-0000-000000000222/source.mp4"))?,
        b"prefetch-bytes"
    );
    Ok(())
}

fn test_temp_dir() -> anyhow::Result<std::path::PathBuf> {
    let path = std::env::temp_dir().join(format!("streamserver-gateway-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

async fn spawn_server(app: Router) -> anyhow::Result<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("stub server should run");
    });
    Ok(format!("http://{addr}"))
}

async fn wait_prefetch_ready(app: Router, task_id: uuid::Uuid) -> anyhow::Result<Value> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/prefetch/{task_id}"))
                    .body(Body::empty())?,
            )
            .await?;
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await?)?;
        if body["status"] == "ready" {
            return Ok(body);
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for prefetch: {body}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[cfg(unix)]
fn install_fake_ffmpeg(root: &std::path::Path, exit_code: i32) -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let script = root.join(format!("fake-ffmpeg-{exit_code}"));
    let body = if exit_code == 0 {
        r#"#!/bin/sh
printf '%s\n' "$@" > "${0}.args"
last=
for arg in "$@"; do last="$arg"; done
mkdir -p "$(dirname "$last")"
case "$last" in
  *.m3u8)
    base="$(basename "$last" .m3u8)"
    printf '#EXTM3U\n#EXT-X-ENDLIST\n%s-00000.ts\n' "$base" > "$last"
    printf 'segment-bytes' > "$(dirname "$last")/${base}-00000.ts"
    ;;
  *)
    printf 'slice-bytes' > "$last"
    ;;
esac
exit 0
"#
        .to_string()
    } else {
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"${{0}}.args\"\nprintf 'synthetic ffmpeg failure' >&2\nexit {exit_code}\n"
        )
    };
    std::fs::write(&script, body)?;
    let mut permissions = std::fs::metadata(&script)?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions)?;
    Ok(script)
}

fn fake_ffmpeg_args_path(script: &std::path::Path) -> PathBuf {
    PathBuf::from(format!("{}.args", script.display()))
}

#[cfg(unix)]
fn install_fake_ffmpeg_without_output(root: &std::path::Path) -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let script = root.join("fake-ffmpeg-no-output");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"${0}.args\"\nexit 0\n",
    )?;
    let mut permissions = std::fs::metadata(&script)?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions)?;
    Ok(script)
}

#[cfg(unix)]
fn install_fake_ffmpeg_playlist_only(root: &std::path::Path) -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let script = root.join("fake-ffmpeg-playlist-only");
    std::fs::write(
        &script,
        "#!/bin/sh\nlast=\nfor arg in \"$@\"; do last=\"$arg\"; done\nmkdir -p \"$(dirname \"$last\")\"\nprintf '#EXTM3U\\n#EXT-X-ENDLIST\\n' > \"$last\"\nexit 0\n",
    )?;
    let mut permissions = std::fs::metadata(&script)?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions)?;
    Ok(script)
}

async fn wait_prefetch_terminal(app: Router, task_id: uuid::Uuid) -> anyhow::Result<Value> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/prefetch/{task_id}"))
                    .body(Body::empty())?,
            )
            .await?;
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await?)?;
        if matches!(body["status"].as_str(), Some("ready" | "failed")) {
            return Ok(body);
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for prefetch: {body}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn prefetch_time_slice_uses_input_seek_duration_and_stream_copy() -> anyhow::Result<()> {
    let temp = test_temp_dir()?;
    let ffmpeg = install_fake_ffmpeg(&temp, 0)?;
    let state = GatewayState::new(GatewayConfig {
        public_base_url: "http://media:18080".to_string(),
        work_root: temp.clone(),
        ffmpeg_bin: ffmpeg.clone(),
    });
    let app = build_app(state);
    let task_id = uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000333")?;

    let created = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/prefetch")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "task_id": task_id,
                        "source_url": "http://customer.example/archive.mp4",
                        "target_path": "imports/00000000-0000-0000-0000-000000000333/source.mp4",
                        "source_kind": "http_mp4",
                        "start_offset_sec": 600,
                        "duration_sec": 180
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(created.status(), StatusCode::ACCEPTED);

    let status = wait_prefetch_terminal(app, task_id).await?;
    assert_eq!(status["status"], "ready");
    assert_eq!(
        std::fs::read(temp.join("imports/00000000-0000-0000-0000-000000000333/source.mp4"))?,
        b"slice-bytes"
    );

    let args_text = std::fs::read_to_string(fake_ffmpeg_args_path(&ffmpeg))?;
    let args: Vec<&str> = args_text.lines().collect();
    let seek = args.iter().position(|value| *value == "-ss").expect("-ss");
    let input = args.iter().position(|value| *value == "-i").expect("-i");
    let duration = args.iter().position(|value| *value == "-t").expect("-t");
    let codec = args.iter().position(|value| *value == "-c").expect("-c");
    assert!(seek < input);
    assert_eq!(args[seek + 1], "600");
    assert_eq!(args[duration + 1], "180");
    assert_eq!(args[codec + 1], "copy");
    assert!(
        !args
            .iter()
            .any(|value| matches!(*value, "-vf" | "-af" | "-r" | "-b:v"))
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn failed_ffmpeg_marks_prefetch_failed_without_publishing_target() -> anyhow::Result<()> {
    let temp = test_temp_dir()?;
    let ffmpeg = install_fake_ffmpeg(&temp, 7)?;
    let state = GatewayState::new(GatewayConfig {
        public_base_url: "http://media:18080".to_string(),
        work_root: temp.clone(),
        ffmpeg_bin: ffmpeg,
    });
    let app = build_app(state);
    let task_id = uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000555")?;

    let created = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/prefetch")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "task_id": task_id,
                        "source_url": "http://customer.example/archive.ts",
                        "target_path": "imports/00000000-0000-0000-0000-000000000555/source.ts",
                        "source_kind": "http_ts",
                        "start_offset_sec": 10,
                        "duration_sec": 5
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(created.status(), StatusCode::ACCEPTED);
    let status = wait_prefetch_terminal(app, task_id).await?;
    assert_eq!(status["status"], "failed");
    assert!(
        status["failure_reason"]
            .as_str()
            .is_some_and(|value| value.contains("synthetic ffmpeg failure"))
    );
    assert!(
        !temp
            .join("imports/00000000-0000-0000-0000-000000000555/source.ts")
            .exists()
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn missing_ffmpeg_output_marks_prefetch_failed() -> anyhow::Result<()> {
    let temp = test_temp_dir()?;
    let ffmpeg = install_fake_ffmpeg_without_output(&temp)?;
    let state = GatewayState::new(GatewayConfig {
        public_base_url: "http://media:18080".to_string(),
        work_root: temp.clone(),
        ffmpeg_bin: ffmpeg,
    });
    let app = build_app(state);
    let task_id = uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000558")?;
    let created = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/prefetch")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "task_id": task_id,
                        "source_url": "http://customer.example/archive.mp4",
                        "target_path": "imports/00000000-0000-0000-0000-000000000558/source.mp4",
                        "source_kind": "http_mp4",
                        "start_offset_sec": 10,
                        "duration_sec": 5
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(created.status(), StatusCode::ACCEPTED);
    let status = wait_prefetch_terminal(app, task_id).await?;
    assert_eq!(status["status"], "failed");
    assert!(
        !temp
            .join("imports/00000000-0000-0000-0000-000000000558/source.mp4")
            .exists()
    );
    Ok(())
}

#[tokio::test]
async fn time_slice_requires_source_kind_and_positive_duration() -> anyhow::Result<()> {
    let temp = test_temp_dir()?;
    let state = GatewayState::new(GatewayConfig {
        public_base_url: "http://media:18080".to_string(),
        work_root: temp,
        ffmpeg_bin: PathBuf::from("ffmpeg"),
    });
    let app = build_app(state);

    let missing_kind = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/prefetch")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "task_id": "00000000-0000-0000-0000-000000000556",
                        "source_url": "http://customer.example/archive.mp4",
                        "target_path": "imports/00000000-0000-0000-0000-000000000556/source.mp4",
                        "duration_sec": 5
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(missing_kind.status(), StatusCode::BAD_REQUEST);

    let zero_duration = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/prefetch")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "task_id": "00000000-0000-0000-0000-000000000557",
                        "source_url": "http://customer.example/archive.mp4",
                        "target_path": "imports/00000000-0000-0000-0000-000000000557/source.mp4",
                        "source_kind": "http_mp4",
                        "duration_sec": 0
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(zero_duration.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn prefetch_hls_time_slice_publishes_playlist_and_segments_together() -> anyhow::Result<()> {
    let temp = test_temp_dir()?;
    let ffmpeg = install_fake_ffmpeg(&temp, 0)?;
    let state = GatewayState::new(GatewayConfig {
        public_base_url: "http://media:18080".to_string(),
        work_root: temp.clone(),
        ffmpeg_bin: ffmpeg,
    });
    let app = build_app(state);
    let task_id = uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000444")?;

    let created = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/prefetch")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "task_id": task_id,
                        "source_url": "http://customer.example/archive.m3u8",
                        "target_path": "imports/00000000-0000-0000-0000-000000000444/source.m3u8",
                        "source_kind": "hls",
                        "start_offset_sec": 60,
                        "duration_sec": 30
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(created.status(), StatusCode::ACCEPTED);
    let status = wait_prefetch_terminal(app, task_id).await?;
    assert_eq!(status["status"], "ready");
    let final_dir = temp.join("imports/00000000-0000-0000-0000-000000000444");
    assert!(final_dir.join("source.m3u8").is_file());
    assert!(final_dir.join("source-00000.ts").is_file());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn hls_without_media_segment_fails_without_publishing_directory() -> anyhow::Result<()> {
    let temp = test_temp_dir()?;
    let ffmpeg = install_fake_ffmpeg_playlist_only(&temp)?;
    let state = GatewayState::new(GatewayConfig {
        public_base_url: "http://media:18080".to_string(),
        work_root: temp.clone(),
        ffmpeg_bin: ffmpeg,
    });
    let app = build_app(state);
    let task_id = uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000559")?;
    let created = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/prefetch")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "task_id": task_id,
                        "source_url": "http://customer.example/archive.m3u8",
                        "target_path": "imports/00000000-0000-0000-0000-000000000559/source.m3u8",
                        "source_kind": "hls",
                        "duration_sec": 5
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(created.status(), StatusCode::ACCEPTED);
    let status = wait_prefetch_terminal(app, task_id).await?;
    assert_eq!(status["status"], "failed");
    assert!(
        status["failure_reason"]
            .as_str()
            .is_some_and(|value| value.contains("no HLS media segment"))
    );
    assert!(
        !temp
            .join("imports/00000000-0000-0000-0000-000000000559")
            .exists()
    );
    Ok(())
}
