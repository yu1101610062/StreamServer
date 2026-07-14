# Media Gateway VOD Time Slice Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `media-gateway` 使用现有 `input.start_offset_sec` 和 `record.duration_sec` 直接预取 HTTP 点播时间片，并在不转码的前提下把共享存储时间片交给 Agent。

**Architecture:** `media-core` 在内部 Prefetch 请求中传递源类型、偏移量和长度；`media-gateway` 在无时间参数时保留现有普通 HTTP 下载，在有时间参数时通过 FFmpeg 输入侧 seek、`-t` 和 `-c copy` 生成 MP4、TS 或 HLS 时间片。Gateway 成功重写后，Core 只清除实际调度用 `resolved_spec.input.start_offset_sec`，保留原始 `requested_spec` 和 `record.duration_sec`。

**Tech Stack:** Rust 2024 workspace、Axum、Reqwest、Tokio process/fs、FFmpeg/FFprobe、Bash native-bundle verification。

## Global Constraints

- 不新增北向任务字段；偏移量必须沿用 `input.start_offset_sec`，长度必须沿用 `record.duration_sec`。
- Gateway 只做时间截取；必须使用 `-c copy`，不得加入编码器、滤镜、缩放、改帧率或改码率参数。
- 经 Gateway 成功重写后，发送给 Agent 的 `resolved_spec.input.start_offset_sec` 必须为 `None`；`record.duration_sec` 必须保留。
- `requested_spec` 必须保留调用方的原始偏移量。
- 无时间参数时必须保留当前普通 HTTP 完整响应下载行为。
- FFmpeg 缺失、非零退出或输出无效时 Prefetch 必须失败；不得回退为完整下载、转码或 Agent 直连源站。
- `http_mp4`、`http_ts`、`hls` 必须分别保持 MP4、MPEG-TS、HLS 封装族；容器索引、时间戳和 HLS 分片边界允许重建。
- 码流复制只承诺关键帧级起点；网络读取量取决于源站 seek/Range/HLS 分片能力。
- FFmpeg 必须由 `tokio::process::Command` 直接执行，不得经过 shell。
- 设计真相来源：`docs/superpowers/specs/2026-07-14-media-gateway-vod-time-slice-design.md`。

---

### Task 1: 扩展 Core Prefetch 内部契约并清除 Agent 偏移量

**Files:**
- Modify: `crates/media-core/src/source_gateway.rs:12-315`
- Modify: `crates/media-core/src/tests/control_plane.rs:265-322`

**Interfaces:**
- Consumes: `TaskSpec.input.kind`, `TaskSpec.input.start_offset_sec`, `TaskSpec.record.duration_sec`。
- Produces: `GatewayAction::Prefetch { source_kind, start_offset_sec, duration_sec }` 和同形 JSON 请求；Gateway 成功后统一清除 `resolved_spec.input.start_offset_sec`。

- [ ] **Step 1: 写出 Prefetch 时间参数和统一清除偏移量的失败测试**

在 `crates/media-core/src/tests/control_plane.rs` 中增强现有两个 Source Gateway 测试，并新增 `start_offset_sec=0` 归一化测试：

```rust
#[test]
fn source_gateway_rewrites_live_http_input_to_relay_url() -> anyhow::Result<()> {
    let task_id = Uuid::parse_str("00000000-0000-0000-0000-000000000111")?;
    let mut spec = sample_spec(
        InputKind::HttpFlv,
        Some("http://customer.example/live.flv"),
        None,
    )
    .resolved();
    spec.input.start_offset_sec = Some(0);

    let action = crate::source_gateway::plan_gateway_action(&spec, task_id)
        .expect("live http input should use media relay");
    crate::source_gateway::apply_gateway_result(
        &mut spec,
        action,
        crate::source_gateway::GatewayActionResult::Relay {
            relay_url: "http://media:18080/relay/00000000-0000-0000-0000-000000000111?token=t"
                .to_string(),
        },
    )?;

    assert_eq!(spec.input.kind, Some(InputKind::HttpFlv));
    assert_eq!(spec.input.source_mode, Some(SourceMode::Live));
    assert_eq!(spec.input.start_offset_sec, None);
    assert_eq!(
        spec.input.url.as_deref(),
        Some("http://media:18080/relay/00000000-0000-0000-0000-000000000111?token=t")
    );
    Ok(())
}

#[test]
fn source_gateway_rewrites_vod_http_time_window_to_shared_file_path() -> anyhow::Result<()> {
    let task_id = Uuid::parse_str("00000000-0000-0000-0000-000000000222")?;
    let mut requested_spec = sample_spec(
        InputKind::HttpMp4,
        Some("http://customer.example/archive.mp4"),
        None,
    )
    .resolved();
    requested_spec.input.start_offset_sec = Some(600);
    requested_spec.record.enabled = Some(true);
    requested_spec.record.duration_sec = Some(180);
    let mut spec = requested_spec.clone();

    let action = crate::source_gateway::plan_gateway_action(&spec, task_id)
        .expect("vod http input should use media prefetch");
    assert_eq!(
        action,
        crate::source_gateway::GatewayAction::Prefetch {
            task_id,
            source_url: "http://customer.example/archive.mp4".to_string(),
            target_path: "imports/00000000-0000-0000-0000-000000000222/source.mp4".to_string(),
            source_kind: InputKind::HttpMp4,
            start_offset_sec: Some(600),
            duration_sec: Some(180),
        }
    );

    crate::source_gateway::apply_gateway_result(
        &mut spec,
        action,
        crate::source_gateway::GatewayActionResult::Prefetch {
            source_url: "imports/00000000-0000-0000-0000-000000000222/source.mp4".to_string(),
        },
    )?;

    assert_eq!(spec.input.kind, Some(InputKind::File));
    assert_eq!(spec.input.source_mode, Some(SourceMode::Vod));
    assert_eq!(spec.input.start_offset_sec, None);
    assert_eq!(spec.record.duration_sec, Some(180));
    assert_eq!(requested_spec.input.start_offset_sec, Some(600));
    assert_eq!(
        spec.input.url.as_deref(),
        Some("imports/00000000-0000-0000-0000-000000000222/source.mp4")
    );
    Ok(())
}

#[test]
fn source_gateway_normalizes_zero_vod_offset_before_prefetch() {
    let task_id = Uuid::from_u128(0x333);
    let mut spec = sample_spec(
        InputKind::HttpTs,
        Some("http://customer.example/archive"),
        None,
    )
    .resolved();
    spec.input.start_offset_sec = Some(0);

    let action = crate::source_gateway::plan_gateway_action(&spec, task_id)
        .expect("vod http input should use media prefetch");
    assert!(matches!(
        action,
        crate::source_gateway::GatewayAction::Prefetch {
            source_kind: InputKind::HttpTs,
            start_offset_sec: None,
            duration_sec: None,
            ref target_path,
            ..
        } if target_path == &format!("imports/{task_id}/source.ts")
    ));
}
```

- [ ] **Step 2: 运行测试并确认 RED**

Run:

```bash
cargo test -p media-core source_gateway_rewrites_ -- --nocapture
cargo test -p media-core source_gateway_normalizes_zero_vod_offset_before_prefetch -- --nocapture
```

Expected: 编译失败，指出 `GatewayAction::Prefetch` 尚无 `source_kind`、`start_offset_sec`、`duration_sec` 字段；旧实现也不会清除 Relay/Prefetch 的偏移量。

- [ ] **Step 3: 实现 Core action、JSON 请求和固定封装扩展名**

在 `crates/media-core/src/source_gateway.rs` 中把 Prefetch action/request 更新为：

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GatewayAction {
    Relay {
        task_id: Uuid,
        source_url: String,
    },
    Prefetch {
        task_id: Uuid,
        source_url: String,
        target_path: String,
        source_kind: InputKind,
        start_offset_sec: Option<u32>,
        duration_sec: Option<u32>,
    },
}

#[derive(Debug, Serialize)]
struct PrefetchRequest {
    task_id: Uuid,
    source_url: String,
    target_path: String,
    source_kind: InputKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_offset_sec: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_sec: Option<u32>,
}
```

更新 `execute_action` 的 Prefetch 分支，使所有字段原样进入 JSON：

```rust
GatewayAction::Prefetch {
    task_id,
    source_url,
    target_path,
    source_kind,
    start_offset_sec,
    duration_sec,
} => {
    let response: PrefetchResponse = self
        .http
        .post(self.endpoint("/api/prefetch")?)
        .json(&PrefetchRequest {
            task_id: *task_id,
            source_url: source_url.clone(),
            target_path: target_path.clone(),
            source_kind: *source_kind,
            start_offset_sec: *start_offset_sec,
            duration_sec: *duration_sec,
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    self.wait_for_prefetch(*task_id, response).await
}
```

点播 action 必须从现有字段取值并固定目标扩展名：

```rust
(InputKind::HttpMp4, Some(SourceMode::Vod))
| (InputKind::HttpTs | InputKind::Hls, Some(SourceMode::Vod)) => {
    Some(GatewayAction::Prefetch {
        task_id,
        source_url: source_url.to_string(),
        target_path: default_prefetch_target_path(task_id, kind),
        source_kind: kind,
        start_offset_sec: spec.input.start_offset_sec.filter(|value| *value > 0),
        duration_sec: spec.record.duration_sec,
    })
}

fn default_prefetch_target_path(task_id: Uuid, kind: InputKind) -> String {
    let ext = match kind {
        InputKind::Hls => "m3u8",
        InputKind::HttpTs => "ts",
        InputKind::HttpMp4 => "mp4",
        _ => unreachable!("only HTTP VOD inputs use prefetch targets"),
    };
    format!("imports/{task_id}/source.{ext}")
}
```

最后把 `apply_gateway_result` 改为成功分支完成后统一清除偏移量：

```rust
pub(crate) fn apply_gateway_result(
    spec: &mut TaskSpec,
    action: GatewayAction,
    result: GatewayActionResult,
) -> Result<(), SourceGatewayError> {
    match (action, result) {
        (GatewayAction::Relay { .. }, GatewayActionResult::Relay { relay_url }) => {
            if relay_url.trim().is_empty() {
                return Err(SourceGatewayError::InvalidSpec(
                    "relay_url must not be empty".to_string(),
                ));
            }
            spec.input.url = Some(relay_url);
        }
        (GatewayAction::Prefetch { .. }, GatewayActionResult::Prefetch { source_url }) => {
            if source_url.trim().is_empty() || source_url.starts_with("uploads/") {
                return Err(SourceGatewayError::InvalidSpec(
                    "prefetch source_url must be a non-upload relative path".to_string(),
                ));
            }
            spec.input.kind = Some(InputKind::File);
            spec.input.source_mode = Some(SourceMode::Vod);
            spec.input.url = Some(source_url);
        }
        _ => return Err(SourceGatewayError::ActionMismatch),
    }
    spec.input.start_offset_sec = None;
    Ok(())
}
```

- [ ] **Step 4: 运行 Core 测试并确认 GREEN**

Run:

```bash
cargo test -p media-core source_gateway_ -- --nocapture
```

Expected: 新增和既有 Source Gateway 测试全部 PASS；`record.duration_sec` 保持 `Some(180)`，Agent 侧偏移量为 `None`。

- [ ] **Step 5: 提交 Core 内部契约改造**

```bash
git add crates/media-core/src/source_gateway.rs crates/media-core/src/tests/control_plane.rs
git commit -m "feat: 传递网关点播时间参数"
```

---

### Task 2: 实现 Gateway 无转码时间片引擎和安全发布

**Files:**
- Create: `crates/media-gateway/src/prefetch.rs`
- Modify: `crates/media-gateway/src/lib.rs:1-295`
- Modify: `crates/media-gateway/src/main.rs:1-32`
- Modify: `crates/media-gateway/tests/gateway.rs:1-172`

**Interfaces:**
- Consumes: 内部 JSON `source_kind`, `start_offset_sec`, `duration_sec` 和 `GatewayConfig.ffmpeg_bin`。
- Produces: `prefetch::execute_prefetch(http, ffmpeg_bin, PrefetchJob)`；无时间参数走 Reqwest，有时间参数执行 `-ss/-t/-c copy`，成功后只发布最终共享存储路径。

- [ ] **Step 1: 给现有测试配置补上 FFmpeg 路径，并写 MP4 时间片失败测试**

在测试文件顶部增加：

```rust
use std::path::PathBuf;
```

所有现有 `GatewayConfig` 初始化增加：

```rust
ffmpeg_bin: PathBuf::from("ffmpeg"),
```

在 `crates/media-gateway/tests/gateway.rs` 增加 Unix fake FFmpeg 和终态轮询辅助函数：

```rust
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
    std::fs::write(&script, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"${0}.args\"\nexit 0\n")?;
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
```

新增 MP4 时间片测试：

```rust
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
    assert!(!args.iter().any(|value| matches!(*value, "-vf" | "-af" | "-r" | "-b:v")));
    Ok(())
}
```

同时在生产失败处理存在之前新增 FFmpeg 非零退出测试：

```rust
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
    assert!(status["failure_reason"].as_str().is_some_and(|value| value.contains("synthetic ffmpeg failure")));
    assert!(!temp.join("imports/00000000-0000-0000-0000-000000000555/source.ts").exists());
    Ok(())
}
```

新增“FFmpeg 成功退出但没有输出”的失败测试，保证输出验证代码也先看到 RED：

```rust
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
    assert!(!temp.join("imports/00000000-0000-0000-0000-000000000558/source.mp4").exists());
    Ok(())
}
```

在同一 RED 阶段增加 Gateway 边界校验测试：

```rust
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
```

在 `crates/media-gateway/src/main.rs` 增加纯函数优先级测试，避免测试期间修改进程环境：

```rust
#[cfg(test)]
mod tests {
    use super::resolve_ffmpeg_bin;
    use std::path::PathBuf;

    #[test]
    fn gateway_ffmpeg_path_prefers_gateway_then_agent_then_path_default() {
        assert_eq!(
            resolve_ffmpeg_bin(Some(" /gateway/ffmpeg "), Some("/agent/ffmpeg")),
            PathBuf::from("/gateway/ffmpeg")
        );
        assert_eq!(
            resolve_ffmpeg_bin(Some(" "), Some(" /agent/ffmpeg ")),
            PathBuf::from("/agent/ffmpeg")
        );
        assert_eq!(resolve_ffmpeg_bin(None, None), PathBuf::from("ffmpeg"));
    }
}
```

- [ ] **Step 2: 运行 MP4 测试并确认 RED**

Run:

```bash
cargo test -p media-gateway prefetch_time_slice_uses_input_seek_duration_and_stream_copy -- --nocapture
cargo test -p media-gateway failed_ffmpeg_marks_prefetch_failed_without_publishing_target -- --nocapture
cargo test -p media-gateway missing_ffmpeg_output_marks_prefetch_failed -- --nocapture
cargo test -p media-gateway time_slice_requires_source_kind_and_positive_duration -- --nocapture
cargo test -p media-gateway gateway_ffmpeg_path_prefers_gateway_then_agent_then_path_default -- --nocapture
```

Expected: 编译失败，因为 `GatewayConfig` 还没有 `ffmpeg_bin`、请求模型不接受时间片字段，并且 `resolve_ffmpeg_bin` 尚不存在。

- [ ] **Step 3: 新建 Prefetch 引擎并接入 MP4/TS**

创建 `crates/media-gateway/src/prefetch.rs`。实现以下公开边界，函数体必须遵循后面的具体算法：

```rust
use std::{ffi::OsString, path::{Path, PathBuf}};

use anyhow::{Context, ensure};
use serde::Deserialize;
use tokio::{fs, io::AsyncWriteExt, process::Command};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PrefetchSourceKind {
    HttpMp4,
    HttpTs,
    Hls,
}

#[derive(Debug, Clone)]
pub(crate) struct PrefetchJob {
    pub(crate) source_url: String,
    pub(crate) final_path: PathBuf,
    pub(crate) source_kind: Option<PrefetchSourceKind>,
    pub(crate) start_offset_sec: Option<u32>,
    pub(crate) duration_sec: Option<u32>,
}

pub(crate) async fn execute_prefetch(
    http: reqwest::Client,
    ffmpeg_bin: &Path,
    job: PrefetchJob,
) -> anyhow::Result<()> {
    if job.start_offset_sec.is_none() && job.duration_sec.is_none() {
        return download_to_file(http, &job.source_url, &job.final_path).await;
    }
    let source_kind = job
        .source_kind
        .context("source_kind is required for time-slice prefetch")?;
    match source_kind {
        PrefetchSourceKind::HttpMp4 | PrefetchSourceKind::HttpTs => {
            clip_single_file(ffmpeg_bin, &job, source_kind).await
        }
        PrefetchSourceKind::Hls => clip_hls(ffmpeg_bin, &job).await,
    }
}

async fn clip_hls(_ffmpeg_bin: &Path, _job: &PrefetchJob) -> anyhow::Result<()> {
    anyhow::bail!("HLS time-slice prefetch is not implemented")
}
```

普通下载必须保留当前流式 GET 和成功后 rename 语义：

```rust
async fn download_to_file(
    http: reqwest::Client,
    source_url: &str,
    final_path: &Path,
) -> anyhow::Result<()> {
    let part_path = temporary_file_path(final_path, "download");
    if let Some(parent) = final_path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let result = async {
        let mut response = http
            .get(source_url)
            .send()
            .await?
            .error_for_status()
            .context("source download failed")?;
        let mut file = fs::File::create(&part_path).await?;
        while let Some(chunk) = response.chunk().await? {
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        drop(file);
        fs::rename(&part_path, final_path).await?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&part_path).await;
    }
    result
}
```

MP4/TS 使用显式 muxer、任务级暂存文件和统一参数生成器：

```rust
async fn clip_single_file(
    ffmpeg_bin: &Path,
    job: &PrefetchJob,
    source_kind: PrefetchSourceKind,
) -> anyhow::Result<()> {
    let parent = job.final_path.parent().context("prefetch target has no parent")?;
    fs::create_dir_all(parent).await?;
    let stage_path = temporary_file_path(&job.final_path, "clip");
    let muxer = match source_kind {
        PrefetchSourceKind::HttpMp4 => "mp4",
        PrefetchSourceKind::HttpTs => "mpegts",
        PrefetchSourceKind::Hls => unreachable!("HLS uses directory publishing"),
    };
    let mut args = base_clip_args(job);
    args.extend([
        OsString::from("-f"),
        OsString::from(muxer),
        stage_path.as_os_str().to_os_string(),
    ]);
    let result = async {
        run_ffmpeg(ffmpeg_bin, &args).await?;
        let metadata = fs::metadata(&stage_path).await?;
        ensure!(metadata.is_file() && metadata.len() > 0, "ffmpeg produced an empty time slice");
        fs::rename(&stage_path, &job.final_path).await?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&stage_path).await;
    }
    result
}

fn base_clip_args(job: &PrefetchJob) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "-hide_banner",
        "-nostdin",
        "-y",
        "-loglevel",
        "error",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    if let Some(start_offset_sec) = job.start_offset_sec.filter(|value| *value > 0) {
        args.extend([OsString::from("-ss"), OsString::from(start_offset_sec.to_string())]);
    }
    args.extend([
        OsString::from("-i"),
        OsString::from(job.source_url.as_str()),
    ]);
    if let Some(duration_sec) = job.duration_sec {
        args.extend([OsString::from("-t"), OsString::from(duration_sec.to_string())]);
    }
    args.extend(
        [
            "-map",
            "0:v?",
            "-map",
            "0:a?",
            "-map",
            "0:s?",
            "-map_metadata",
            "0",
            "-c",
            "copy",
        ]
        .into_iter()
        .map(OsString::from),
    );
    args
}

async fn run_ffmpeg(ffmpeg_bin: &Path, args: &[OsString]) -> anyhow::Result<()> {
    let output = Command::new(ffmpeg_bin)
        .args(args)
        .output()
        .await
        .with_context(|| format!("failed to start ffmpeg at {}", ffmpeg_bin.display()))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr: String = String::from_utf8_lossy(&output.stderr).chars().take(4096).collect();
    anyhow::bail!("ffmpeg exited with {}: {}", output.status, stderr.trim());
}

fn temporary_file_path(final_path: &Path, label: &str) -> PathBuf {
    let file_name = final_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("source");
    final_path.with_file_name(format!(".{file_name}.{label}.{}.part", Uuid::now_v7()))
}
```

在 `crates/media-gateway/src/lib.rs` 中增加 `mod prefetch;`，给 `GatewayConfig` 增加 `ffmpeg_bin: PathBuf`，给 `PrefetchRequest` 增加带默认值的内部字段：

```rust
#[derive(Debug, Deserialize)]
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
```

`create_prefetch` 在 spawn 前拒绝 `duration_sec=0`，并在存在任意时间参数时要求 `source_kind`：

```rust
let start_offset_sec = request.start_offset_sec.filter(|value| *value > 0);
if request.duration_sec == Some(0) {
    return (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": "duration_sec must be greater than 0"})),
    )
        .into_response();
}
if (start_offset_sec.is_some() || request.duration_sec.is_some())
    && request.source_kind.is_none()
{
    return (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": "source_kind is required for time-slice prefetch"})),
    )
        .into_response();
}
```

spawn 中调用新引擎：

```rust
let ffmpeg_bin = state.config.ffmpeg_bin.clone();
let source_kind = request.source_kind;
let duration_sec = request.duration_sec;
tokio::spawn(async move {
    let result = prefetch::execute_prefetch(
        http,
        &ffmpeg_bin,
        prefetch::PrefetchJob {
            source_url,
            final_path,
            source_kind,
            start_offset_sec,
            duration_sec,
        },
    )
    .await;
    let mut prefetches = prefetches.lock().await;
    prefetches.insert(
        task_id,
        match result {
            Ok(()) => PrefetchState {
                status: "ready".to_string(),
                source_url: Some(target_path),
                failure_reason: None,
            },
            Err(error) => PrefetchState {
                status: "failed".to_string(),
                source_url: None,
                failure_reason: Some(error.to_string()),
            },
        },
    );
});
```

删除 `lib.rs` 中旧的 `download_to_file`，避免保留两套下载实现。

在 `crates/media-gateway/src/main.rs` 实现经过测试的路径选择，并在创建 `GatewayConfig` 时传入：

```rust
fn resolve_ffmpeg_bin(gateway_value: Option<&str>, agent_value: Option<&str>) -> PathBuf {
    gateway_value
        .and_then(nonempty_trimmed)
        .or_else(|| agent_value.and_then(nonempty_trimmed))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("ffmpeg"))
}

fn nonempty_trimmed(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}
```

`main` 中使用：

```rust
let gateway_ffmpeg_bin = std::env::var("MEDIA_GATEWAY_FFMPEG_BIN").ok();
let agent_ffmpeg_bin = std::env::var("FFMPEG_BIN").ok();
let ffmpeg_bin = resolve_ffmpeg_bin(
    gateway_ffmpeg_bin.as_deref(),
    agent_ffmpeg_bin.as_deref(),
);
let state = GatewayState::new(GatewayConfig {
    public_base_url,
    work_root,
    ffmpeg_bin,
});
```

- [ ] **Step 4: 运行 MP4 和普通下载测试并确认 GREEN**

Run:

```bash
cargo test -p media-gateway prefetch_time_slice_uses_input_seek_duration_and_stream_copy -- --nocapture
cargo test -p media-gateway failed_ffmpeg_marks_prefetch_failed_without_publishing_target -- --nocapture
cargo test -p media-gateway missing_ffmpeg_output_marks_prefetch_failed -- --nocapture
cargo test -p media-gateway time_slice_requires_source_kind_and_positive_duration -- --nocapture
cargo test -p media-gateway gateway_ffmpeg_path_prefers_gateway_then_agent_then_path_default -- --nocapture
cargo test -p media-gateway prefetch_downloads_http_source_to_shared_storage_path -- --nocapture
```

Expected: 六个测试 PASS；无时间参数请求仍可省略 `source_kind` 并得到原始字节。

- [ ] **Step 5: 写出 HLS 原子目录发布的 RED 测试**

新增 HLS 测试；FFmpeg 非零退出测试已经在 Step 1 先于失败处理实现：

```rust
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
```

同一 RED 阶段增加“只有播放列表、没有媒体分片”的 HLS 验证测试：

```rust
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
    assert!(status["failure_reason"].as_str().is_some_and(|value| value.contains("no HLS media segment")));
    assert!(!temp.join("imports/00000000-0000-0000-0000-000000000559").exists());
    Ok(())
}

```

- [ ] **Step 6: 运行 HLS 测试并确认 RED**

Run:

```bash
cargo test -p media-gateway prefetch_hls_time_slice_publishes_playlist_and_segments_together -- --nocapture
cargo test -p media-gateway hls_without_media_segment_fails_without_publishing_directory -- --nocapture
```

Expected: 两个 HLS 测试都得到 `failed`；第一个失败原因是 `HLS time-slice prefetch is not implemented`，第二个尚未得到期望的 `no HLS media segment`。

- [ ] **Step 7: 实现 HLS 暂存目录和原子发布**

在 `prefetch.rs` 中实现 `clip_hls`：

```rust
async fn clip_hls(ffmpeg_bin: &Path, job: &PrefetchJob) -> anyhow::Result<()> {
    let final_dir = job.final_path.parent().context("HLS target has no parent")?;
    let publish_parent = final_dir.parent().context("HLS target directory has no parent")?;
    fs::create_dir_all(publish_parent).await?;
    ensure!(!fs::try_exists(final_dir).await?, "HLS target directory already exists");

    let final_dir_name = final_dir
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("hls");
    let stage_dir = publish_parent.join(format!(
        ".{final_dir_name}.clip.{}.part",
        Uuid::now_v7()
    ));
    fs::create_dir_all(&stage_dir).await?;
    let playlist_name = job
        .final_path
        .file_name()
        .context("HLS target has no playlist name")?;
    let playlist_path = stage_dir.join(playlist_name);
    let playlist_stem = job
        .final_path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("source");
    let segment_template = stage_dir.join(format!("{playlist_stem}-%05d.ts"));

    let mut args = base_clip_args(job);
    args.extend([
        OsString::from("-f"),
        OsString::from("hls"),
        OsString::from("-hls_playlist_type"),
        OsString::from("vod"),
        OsString::from("-hls_list_size"),
        OsString::from("0"),
        OsString::from("-hls_segment_filename"),
        segment_template.as_os_str().to_os_string(),
        playlist_path.as_os_str().to_os_string(),
    ]);

    let result = async {
        run_ffmpeg(ffmpeg_bin, &args).await?;
        ensure!(
            fs::metadata(&playlist_path).await?.len() > 0,
            "ffmpeg produced an empty HLS playlist"
        );
        let mut entries = fs::read_dir(&stage_dir).await?;
        let mut has_segment = false;
        while let Some(entry) = entries.next_entry().await? {
            if entry.path().extension().and_then(|value| value.to_str()) == Some("ts")
                && entry.metadata().await?.len() > 0
            {
                has_segment = true;
                break;
            }
        }
        ensure!(has_segment, "ffmpeg produced no HLS media segment");
        fs::rename(&stage_dir, final_dir).await?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_dir_all(&stage_dir).await;
    }
    result
}
```

- [ ] **Step 8: 运行全部 Gateway 测试**

Run:

```bash
cargo fmt --check
cargo test -p media-gateway -- --nocapture
```

Expected: Gateway 全部测试 PASS，且编译输出没有新增 warning。

- [ ] **Step 9: 提交 Gateway 时间片引擎**

```bash
git add crates/media-gateway/src/prefetch.rs crates/media-gateway/src/lib.rs crates/media-gateway/src/main.rs crates/media-gateway/tests/gateway.rs
git commit -m "feat: 网关按时间片预取点播源"
```

---

### Task 3: 增加真实媒体、Native smoke 和部署文档验收

**Files:**
- Create: `scripts/smoke-media-gateway-timeslice.sh`
- Modify: `scripts/verify-native-bundle-on-target.sh:2656-2706`
- Modify: `docs/zh/08-native-deployment.md:41-74`

**Interfaces:**
- Consumes: 已构建的 `media-gateway`、FFmpeg、FFprobe、curl、Python 3。
- Produces: 可重复运行的真实 MP4 时间片 smoke，验证 codec/宽高/帧率/音频参数不变；Native 启动 smoke 显式传入 Gateway FFmpeg 路径。

- [ ] **Step 1: 先写真实媒体 smoke 并确认它在旧 Gateway 上失败**

新增 `scripts/smoke-media-gateway-timeslice.sh`，脚本必须完成以下实际流程：

```bash
#!/usr/bin/env bash
set -euo pipefail

FFMPEG_BIN="${FFMPEG_BIN:-ffmpeg}"
FFPROBE_BIN="${FFPROBE_BIN:-ffprobe}"
MEDIA_GATEWAY_BIN="${MEDIA_GATEWAY_BIN:-target/debug/media-gateway}"
ROOT="$(mktemp -d)"
gateway_pid=
source_pid=

cleanup() {
  [ -z "${gateway_pid:-}" ] || kill "${gateway_pid}" >/dev/null 2>&1 || true
  [ -z "${source_pid:-}" ] || kill "${source_pid}" >/dev/null 2>&1 || true
  rm -rf -- "${ROOT}"
}
trap cleanup EXIT

pick_port() {
  python3 - <<'PY'
import socket
s = socket.socket()
s.bind(('127.0.0.1', 0))
print(s.getsockname()[1])
s.close()
PY
}

source_port="$(pick_port)"
gateway_port="$(pick_port)"
mkdir -p "${ROOT}/source" "${ROOT}/work"
"${FFMPEG_BIN}" -v error -y \
  -f lavfi -i testsrc2=size=320x180:rate=25 \
  -f lavfi -i sine=frequency=1000:sample_rate=48000 \
  -t 12 -c:v libx264 -g 50 -pix_fmt yuv420p -c:a aac \
  "${ROOT}/source/input.mp4"

python3 -m http.server "${source_port}" --bind 127.0.0.1 \
  --directory "${ROOT}/source" >"${ROOT}/source.log" 2>&1 &
source_pid=$!

MEDIA_GATEWAY_BIND_ADDR="127.0.0.1:${gateway_port}" \
MEDIA_GATEWAY_PUBLIC_BASE_URL="http://127.0.0.1:${gateway_port}" \
MEDIA_GATEWAY_WORK_ROOT="${ROOT}/work" \
MEDIA_GATEWAY_FFMPEG_BIN="${FFMPEG_BIN}" \
  "${MEDIA_GATEWAY_BIN}" >"${ROOT}/gateway.log" 2>&1 &
gateway_pid=$!

for _ in $(seq 1 100); do
  curl -fsS "http://127.0.0.1:${gateway_port}/api/healthz" >/dev/null && break
  sleep 0.05
done

task_id=00000000-0000-0000-0000-000000000666
curl -fsS -X POST "http://127.0.0.1:${gateway_port}/api/prefetch" \
  -H 'content-type: application/json' \
  -d "{\"task_id\":\"${task_id}\",\"source_url\":\"http://127.0.0.1:${source_port}/input.mp4\",\"target_path\":\"imports/${task_id}/source.mp4\",\"source_kind\":\"http_mp4\",\"start_offset_sec\":4,\"duration_sec\":4}" \
  >/dev/null

status=
for _ in $(seq 1 200); do
  status="$(curl -fsS "http://127.0.0.1:${gateway_port}/api/prefetch/${task_id}")"
  python3 -c 'import json,sys; raise SystemExit(0 if json.load(sys.stdin).get("status") == "ready" else 1)' \
    <<<"${status}" && break
  python3 -c 'import json,sys; raise SystemExit(0 if json.load(sys.stdin).get("status") != "failed" else 1)' \
    <<<"${status}" || { printf '%s\n' "${status}" >&2; exit 1; }
  sleep 0.05
done

input_json="$(${FFPROBE_BIN} -v error -show_entries stream=codec_type,codec_name,width,height,r_frame_rate,sample_rate,channels -of json "${ROOT}/source/input.mp4")"
output_json="$(${FFPROBE_BIN} -v error -show_entries stream=codec_type,codec_name,width,height,r_frame_rate,sample_rate,channels -of json "${ROOT}/work/imports/${task_id}/source.mp4")"
python3 - "${input_json}" "${output_json}" <<'PY'
import json, sys
source = json.loads(sys.argv[1])["streams"]
output = json.loads(sys.argv[2])["streams"]
keys = ("codec_type", "codec_name", "width", "height", "r_frame_rate", "sample_rate", "channels")
normalize = lambda streams: [{k: stream.get(k) for k in keys if k in stream} for stream in streams]
assert normalize(source) == normalize(output), (normalize(source), normalize(output))
PY

duration="$(${FFPROBE_BIN} -v error -show_entries format=duration -of default=nk=1:nw=1 "${ROOT}/work/imports/${task_id}/source.mp4")"
python3 - "${duration}" <<'PY'
import sys
duration = float(sys.argv[1])
assert 3.0 <= duration <= 5.5, duration
PY

echo "media-gateway time-slice smoke passed"
```

Run:

```bash
cargo build -p media-gateway
bash scripts/smoke-media-gateway-timeslice.sh
```

Expected before Task 2 implementation: FAIL，因为旧 `/api/prefetch` 忽略时间参数并输出完整 12 秒源文件。Expected after Task 2: 输出 `media-gateway time-slice smoke passed`。

- [ ] **Step 2: 把真实 smoke 纳入脚本语法检查并设置可执行权限**

```bash
chmod +x scripts/smoke-media-gateway-timeslice.sh
bash -n scripts/smoke-media-gateway-timeslice.sh
bash scripts/smoke-media-gateway-timeslice.sh
```

Expected: Bash 语法检查和真实媒体 smoke 均 PASS。

- [ ] **Step 3: 更新 Native Gateway 启动 smoke**

在 `scripts/verify-native-bundle-on-target.sh` 的 Gateway 启动环境中增加：

```bash
MEDIA_GATEWAY_FFMPEG_BIN=\"\${VERIFY_ROOT}/runtime/ffmpeg/cpu/bin/ffmpeg\" \
```

Run:

```bash
bash -n scripts/verify-native-bundle-on-target.sh
```

Expected: PASS。

- [ ] **Step 4: 补充 Native 部署文档**

在 `docs/zh/08-native-deployment.md` 的 FFmpeg runtime 说明后增加以下内容：

```markdown
`media-gateway` 对没有时间参数的点播源继续执行普通 HTTP 下载；当 Core 传入 `input.start_offset_sec` 或 `record.duration_sec` 时，Gateway 使用 FFmpeg 输入侧 seek、`-t` 和 `-c copy` 生成共享存储时间片。该过程不转码，编码、分辨率、帧率和音频参数保持不变，但容器索引、时间戳和 HLS 分片边界会重新生成，起点精度受关键帧约束。

Gateway 主机通过 `MEDIA_GATEWAY_FFMPEG_BIN` 指定 FFmpeg；未设置时依次回退到 `FFMPEG_BIN` 和 PATH 中的 `ffmpeg`。worker/all-in-one 可以复用 Native 安装器生成的 `FFMPEG_BIN`，独立 Gateway 或 core-only 主机必须显式提供可执行文件。源站不支持 Range、HLS 分片定位或容器快速 seek 时，Gateway 不会在共享存储落完整源文件，但网络侧仍可能读取偏移量之前的数据。
```

- [ ] **Step 5: 提交 smoke 和文档**

```bash
git add scripts/smoke-media-gateway-timeslice.sh scripts/verify-native-bundle-on-target.sh docs/zh/08-native-deployment.md
git commit -m "test: 验证网关点播时间片不转码"
```

---

### Task 4: 完成工作区回归和交付核验

**Files:**
- Verify: `crates/media-core/src/source_gateway.rs`
- Verify: `crates/media-gateway/src/prefetch.rs`
- Verify: `crates/media-gateway/src/lib.rs`
- Verify: `crates/media-gateway/tests/gateway.rs`
- Verify: `scripts/smoke-media-gateway-timeslice.sh`
- Verify: `docs/zh/08-native-deployment.md`

**Interfaces:**
- Consumes: Tasks 1-3 的三个独立提交。
- Produces: 格式、单包测试、真实媒体 smoke、全工作区测试和静态检查的最终证据。

- [ ] **Step 1: 格式化并做差异卫生检查**

```bash
cargo fmt --all
cargo fmt --all --check
git diff --check
```

Expected: 全部返回 0；`cargo fmt --all` 若产生变更，应将变更包含进对应实现提交或单独提交 `style: 格式化网关时间片改造`。

- [ ] **Step 2: 运行聚焦测试**

```bash
cargo test -p media-gateway -- --nocapture
cargo test -p media-core source_gateway_ -- --nocapture
bash scripts/smoke-media-gateway-timeslice.sh
```

Expected: 全部 PASS；真实 smoke 明确输出 `media-gateway time-slice smoke passed`。

- [ ] **Step 3: 运行全工作区验证**

```bash
cargo test --workspace
cargo check --workspace
```

Expected: 两条命令返回 0；不得新增 error 或 warning。若仓库已有可复现 warning，只能记录既有 warning，不能把新 warning 归为既有问题。

- [ ] **Step 4: 核对最终任务契约和 Git 状态**

```bash
rg -n "source_kind|start_offset_sec|duration_sec|ffmpeg_bin|-c.*copy" \
  crates/media-core/src/source_gateway.rs \
  crates/media-gateway/src \
  crates/media-gateway/tests/gateway.rs
git status --short --branch
git log -5 --oneline
```

Expected:

- Core Prefetch 请求包含三个内部时间片字段。
- Gateway 命令只出现 `-c copy`，没有具体编码器和滤镜。
- Gateway 成功后 `resolved_spec.input.start_offset_sec` 被清除，`record.duration_sec` 未被清除。
- 工作树干净，当前 `master` 只领先远端本次文档与实现提交。
