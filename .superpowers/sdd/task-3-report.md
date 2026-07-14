# Task 3 实施报告：真实媒体、Native smoke 与部署文档验收

## 状态与提交

- 状态：DONE
- 提交：`777da5dcb79154bc5208c967a37a0251f4850a45`
- 主题：`test: 验证网关点播时间片不转码`
- 变更范围：仅提交了 brief 指定的 3 个文件；没有修改 Gateway Rust 实现，也没有改写或 amend Task 1/Task 2 提交。

## 变更内容

1. 新增可执行脚本 `scripts/smoke-media-gateway-timeslice.sh`：
   - 使用 FFmpeg 生成 12 秒 H264/AAC MP4 测试源；
   - 通过本地 HTTP 源站和真实 `media-gateway` 进程调用 `/api/prefetch`；
   - 请求 `start_offset_sec=4`、`duration_sec=4`；
   - 轮询 `/api/prefetch/{task_id}` 到 `ready`；
   - 使用 FFprobe 比较输入/输出的 codec、宽高、帧率、采样率和声道数；
   - 使用 FFprobe 验证输出时长处于 `3.0..=5.5` 秒；
   - 脚本模式为 `100755`。
2. 更新 `scripts/verify-native-bundle-on-target.sh`：Gateway 启动 smoke 显式设置
   `MEDIA_GATEWAY_FFMPEG_BIN="${VERIFY_ROOT}/runtime/ffmpeg/cpu/bin/ffmpeg"`。
3. 更新 `docs/zh/08-native-deployment.md`：说明普通下载与时间片分流、输入侧 seek/`-t`/`-c copy`、关键帧精度、FFmpeg 配置回退顺序，以及无 Range/快速 seek 时的网络读取语义。

## 工具前置检查

本机依赖存在，未触发 BLOCKED：

```text
/opt/homebrew/bin/ffmpeg
/opt/homebrew/bin/ffprobe
/usr/bin/curl
/Library/Frameworks/Python.framework/Versions/3.13/bin/python3
ffmpeg/ffprobe version 8.1.2
```

## RED：旧 Gateway 必须忽略时间参数并失败

为避免修改主 checkout 或历史提交，在当前指定 worktree 内临时切到 Task 2 前的 Gateway 提交 `6e42615`，将旧二进制构建到 ignored target 目录后立即切回任务分支：

```bash
git switch --detach 6e42615
cargo build -p media-gateway --target-dir target/task3-red-gateway
git switch codex/media-gateway-vod-timeslice
```

最终 smoke fixture（包含下述 `+faststart` 修正）执行 RED：

```bash
set +e
MEDIA_GATEWAY_BIN=target/task3-red-gateway/debug/media-gateway \
  bash scripts/smoke-media-gateway-timeslice.sh
red_rc=$?
set -e
printf 'FINAL_RED_EXIT=%s\n' "$red_rc"
test "$red_rc" -ne 0
```

相关输出：

```text
Traceback (most recent call last):
  File "<stdin>", line 3, in <module>
AssertionError: 12.0
FINAL_RED_EXIT=1
```

失败原因符合预期：`6e42615` 的旧 `/api/prefetch` 忽略时间参数并下载完整 12 秒源文件；codec identity 比较已通过，时长断言拦截了旧行为。

## 首次 GREEN 失败与根因定位

首次按 brief 原始 fixture 运行当前 `b0403fd` 时，FFprobe 返回输出 `streams=[]`。没有弱化 codec 断言，而是保留真实 HTTP/API 链路并做直接复现：

```text
Python/3.13.5 SimpleHTTP 对 Range: bytes=1000-1999 返回 HTTP/1.0 200 OK（不是 206）
FFmpeg 日志：partial file / Error during demuxing
FFmpeg 退出码：0
输出大小：262 bytes
FFprobe：streams=[]
```

根因是默认生成的 MP4 把 `moov` 索引放在文件末尾，而 Python SimpleHTTP 不支持 Range。FFmpeg 输入侧 seek 发起随机读取时收到整文件 `200`，最终虽然以 0 退出，却只写出无媒体流的 MP4 容器头。

最小假设验证是在测试源生成命令中增加 `-movflags +faststart`，只把合成 MP4 的索引移到文件开头，不改变 Gateway 行为、HTTP API 路径或断言。相同非 Range HTTP 服务下，直接 FFmpeg 复现结果为：

```text
faststart_http_size=172632
streams: h264 video 320x180 25/1; aac audio 48000 Hz mono
duration: 4.080000
```

任务 owner 已确认采用该最小 fixture 修正。修正后重新对最终脚本执行了完整 RED/GREEN，而不是沿用修正前的 RED 记录。

## GREEN：当前 Gateway 通过真实 HTTP API smoke

命令：

```bash
cargo build -p media-gateway
bash scripts/smoke-media-gateway-timeslice.sh
```

相关输出：

```text
Finished `dev` profile [unoptimized + debuginfo]
media-gateway time-slice smoke passed
```

同一最终脚本随后按 brief 再次执行：

```bash
chmod +x scripts/smoke-media-gateway-timeslice.sh
bash -n scripts/smoke-media-gateway-timeslice.sh
test -x scripts/smoke-media-gateway-timeslice.sh
bash scripts/smoke-media-gateway-timeslice.sh
```

结果：退出码 0，输出 `media-gateway time-slice smoke passed`。

## Native 与最终集中验证

Native 脚本更新后先单独执行：

```bash
bash -n scripts/verify-native-bundle-on-target.sh
```

结果：PASS。

提交前 fresh verification 命令：

```bash
set -euo pipefail
command -v ffmpeg
command -v ffprobe
command -v curl
command -v python3
cargo build -p media-gateway
cargo test -p media-gateway
bash -n scripts/smoke-media-gateway-timeslice.sh
test -x scripts/smoke-media-gateway-timeslice.sh
bash scripts/smoke-media-gateway-timeslice.sh
bash -n scripts/verify-native-bundle-on-target.sh
rg -F 'MEDIA_GATEWAY_FFMPEG_BIN=\"\${VERIFY_ROOT}/runtime/ffmpeg/cpu/bin/ffmpeg\" \' \
  scripts/verify-native-bundle-on-target.sh
rg -F 'Gateway 主机通过 `MEDIA_GATEWAY_FFMPEG_BIN` 指定 FFmpeg' \
  docs/zh/08-native-deployment.md
git diff --check
printf '%s\n' 'FINAL_VERIFICATION_PASSED'
```

相关输出摘要：

```text
cargo build -p media-gateway: PASS
cargo test -p media-gateway: 10 passed, 0 failed
bash -n scripts/smoke-media-gateway-timeslice.sh: PASS
real API/FFprobe smoke: media-gateway time-slice smoke passed
bash -n scripts/verify-native-bundle-on-target.sh: PASS
executable/static assertions: PASS
git diff --check: PASS
FINAL_VERIFICATION_PASSED
```

## Self-review

- 真实 smoke 的时间片只能由 Gateway `/api/prefetch` 生成；脚本直接调用 FFmpeg 的部分仅用于生成 12 秒输入 fixture，不绕过 Gateway 做切片。
- API 请求显式包含 `source_kind=http_mp4`、`start_offset_sec=4`、`duration_sec=4`，并等待 Gateway 状态进入 `ready`。
- FFprobe 对输入和 Gateway 输出做流级 identity 比较，覆盖 `codec_type`、`codec_name`、`width`、`height`、`r_frame_rate`、`sample_rate`、`channels`；空流输出会失败，不能以文件存在代替媒体正确性。
- 时长由 Gateway 输出文件的 format duration 验证，`12.0` 秒旧行为已由 RED 证明可被捕获。
- Native 环境变量使用远端 `VERIFY_ROOT` 转义形式，位于 Gateway 进程启动环境中；脚本整体 `bash -n` 通过。
- 文档文字与 brief 一致，明确了不转码语义、关键帧约束和 FFmpeg fallback 顺序。
- staged diff 仅包含 brief 指定的 3 个文件；`git diff --cached --check` 通过，提交创建脚本模式为 `100755`。
- 没有修改 Task 1/Task 2 Rust 实现，没有碰主 checkout，也没有 amend 既有提交。

## Concerns / 限制

- 无阻塞 concern。
- fixture 增加 `-movflags +faststart` 是为兼容本机 Python SimpleHTTP 的非 Range 行为；它只调整合成输入的容器索引位置，真实 smoke 仍完整经过 HTTP 源站、Gateway API、FFmpeg stream copy 和 FFprobe 验收。
- 本任务没有实际 Linux Native bundle/目标主机，因此 Native 变更按 brief 做了 shell 语法和静态路径验证，没有执行完整 `verify-native-bundle-on-target.sh --bundle ... --host ...`。
- 未运行已知会受现有 macOS `media-agent` Linux-only 编译问题影响的 full-workspace suite；按任务边界运行了相关 `media-gateway` focused suite（10/10）和真实媒体 smoke。

# Task 3 Review 修复：发布前媒体校验与 ready 观测

## 状态与范围

- 状态：DONE。
- 修复基线：`777da5dcb79154bc5208c967a37a0251f4850a45`。
- 提交主题：`fix: 验证网关时间片输出后再发布`；本节与修复代码在同一新提交中，SHA 见最终返回。
- 修改 `crates/media-gateway/src/prefetch.rs`、`crates/media-gateway/tests/gateway.rs`、`scripts/smoke-media-gateway-timeslice.sh`，并将本报告一并提交。
- 普通无时间参数的 reqwest 下载分支未修改；校验仅位于单文件和 HLS 两条 FFmpeg 时间片发布路径。

## 根因与约束验证

代码路径复核确认：原单文件分支只检查 staged file 非空，HLS 分支只检查 playlist/segment 非空，随后直接 rename；因此 FFmpeg 退出 0 但生成无媒体流容器时会错误进入 `ready`。smoke 轮询则只在看到 `ready` 时 break，没有在循环耗尽后证明真的观察到 `ready`。

在写实现前，用实际 FFmpeg 验证了拟采用命令：读取 staged 输入，强制 `-map 0:v:0`，可选 `-map '0:a?'`，使用 `-c copy -f null -`。它接受真实 H.264 媒体、拒绝 streamless MP4，且输入哈希不变：

```text
valid streams: video
zero streams: valid unchanged: yes
zero validation rc: 234
zero validation output: Stream map '' matches no streams.
To ignore this, add a trailing '?' to the map.
Failed to set value '0:v:0' for option 'map': Invalid argument
Error parsing options for output file -.
Error opening output files: Invalid argument
zero unchanged: yes
```

同一命令也通过了真实 HLS playlist/segment，并保持全部 staged 文件哈希不变：

```text
real HLS validation passed; staged files unchanged: yes
```

## RED：退出 0、非空但无效的 staged 媒体会被发布

先只增加 fake-FFmpeg 和回归测试，不修改生产实现。fake 的生成调用退出 0 并写入非空无效内容；校验调用单独记录参数并退出 9。

单文件 RED 命令：

```bash
cargo test -p media-gateway invalid_nonempty_ffmpeg_output_fails_validation_and_cleans_staging -- --exact --nocapture
```

关键原始输出：

```text
running 1 test
thread 'invalid_nonempty_ffmpeg_output_fails_validation_and_cleans_staging' (17235823) panicked at crates/media-gateway/tests/gateway.rs:516:5:
assertion `left == right` failed
  left: String("ready")
 right: "failed"
test invalid_nonempty_ffmpeg_output_fails_validation_and_cleans_staging ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 10 filtered out
```

HLS RED 命令：

```bash
cargo test -p media-gateway invalid_hls_output_fails_validation_and_cleans_staging -- --exact --nocapture
```

关键原始输出：

```text
running 1 test
thread 'invalid_hls_output_fails_validation_and_cleans_staging' (17236181) panicked at crates/media-gateway/tests/gateway.rs:707:5:
assertion `left == right` failed
  left: String("ready")
 right: "failed"
test invalid_hls_output_fails_validation_and_cleans_staging ... FAILED
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 10 filtered out
```

两次 RED 都在状态断言处按预期失败，证明测试抓到的是“无效输出被发布为 ready”，不是测试语法、fixture 或进程启动错误。

## 实现

1. 新增 `validate_staged_media`，通过现有 `tokio::process::Command::new(ffmpeg_bin)` 直接执行已配置的 FFmpeg；未使用 shell，也未增加 FFprobe 或其他运行时依赖。
2. 校验参数固定为 mandatory video、optional audio、`-c copy` 和 null muxer；它完整读取 staged 输出，不转码、不写回或修改输出。
3. 单文件在 staged file 非空检查后、rename 前校验；HLS 在 playlist/segment 结构检查后、目录 rename 前校验 staged playlist。
4. 任一校验失败沿用原 error cleanup：单文件删除 staged file，HLS 删除整个 staged directory；不回退普通下载，也不发布 final target。
5. success fake-FFmpeg 显式区分第二次 validation invocation 并返回成功；成功测试断言校验输入路径包含 `.part`，且参数含 `0:v:0`、`0:a?`、`-c copy`、`-f null -`。
6. smoke 增加 `ready_observed`，只有实际看到 `ready` 才置 true；轮询耗尽会打印最后状态并退出 1。

## GREEN：Focused 回归

单文件命令同 RED，修复后输出：

```text
running 1 test
test invalid_nonempty_ffmpeg_output_fails_validation_and_cleans_staging ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 10 filtered out; finished in 0.31s
```

HLS 命令同 RED，修复后输出：

```text
running 1 test
test invalid_hls_output_fails_validation_and_cleans_staging ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 10 filtered out; finished in 0.32s
```

两项测试都同时断言：状态为 `failed`、final target 不存在、staging 目录为空、校验确实使用 staged `.part` 输入及 stream-copy/null 参数。

## 全量 media-gateway 与真实 smoke

命令：

```bash
cargo test -p media-gateway
```

原始结果摘要：

```text
Running unittests src/lib.rs
test result: ok. 0 passed; 0 failed

Running unittests src/main.rs
running 1 test
test tests::gateway_ffmpeg_path_prefers_gateway_then_agent_then_path_default ... ok
test result: ok. 1 passed; 0 failed

Running tests/gateway.rs
running 11 tests
test prefetch_target_path_stays_under_work_root_and_not_uploads ... ok
test time_slice_requires_source_kind_and_positive_duration ... ok
test relay_requires_registered_task_and_token ... ok
test prefetch_downloads_http_source_to_shared_storage_path ... ok
test failed_ffmpeg_marks_prefetch_failed_without_publishing_target ... ok
test missing_ffmpeg_output_marks_prefetch_failed ... ok
test hls_without_media_segment_fails_without_publishing_directory ... ok
test invalid_nonempty_ffmpeg_output_fails_validation_and_cleans_staging ... ok
test prefetch_time_slice_uses_input_seek_duration_and_stream_copy ... ok
test prefetch_hls_time_slice_publishes_playlist_and_segments_together ... ok
test invalid_hls_output_fails_validation_and_cleans_staging ... ok
test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

Doc-tests media_gateway
test result: ok. 0 passed; 0 failed
```

其中 `prefetch_downloads_http_source_to_shared_storage_path` 继续使用任意非媒体字节并成功，证明无时间参数的 reqwest 路径没有被 FFmpeg 校验接管。

真实媒体命令：

```bash
cargo build -p media-gateway
bash scripts/smoke-media-gateway-timeslice.sh
```

关键原始输出与退出状态：

```text
Finished `dev` profile [unoptimized + debuginfo]
media-gateway time-slice smoke passed
exit 0
```

另用导出的 curl/sleep stub 让 health 和 POST 成功、200 次状态查询始终返回 pending，同时不让 target/ffprobe 偶然决定结果。命令退出与最后状态输出：

```text
pending smoke exit: 1
prefetch did not reach ready: {"status":"pending"}
```

## 格式、语法与静态检查

执行：

```bash
cargo fmt --check
bash -n scripts/smoke-media-gateway-timeslice.sh
bash -n scripts/verify-native-bundle-on-target.sh
test -x scripts/smoke-media-gateway-timeslice.sh
git diff --check
test -z "$(git diff -- crates/media-gateway/Cargo.toml Cargo.toml Cargo.lock)"
```

结果：全部 exit 0，无 stdout/stderr；没有新增或修改依赖。

关键静态输出：

```text
109:        validate_staged_media(ffmpeg_bin, &playlist_path).await?;
181:        validate_staged_media(ffmpeg_bin, &stage_path).await?;
232:async fn validate_staged_media(ffmpeg_bin: &Path, input_path: &Path) -> anyhow::Result<()> {
241:        OsString::from("0:v:0"),
243:        OsString::from("0:a?"),
245:        OsString::from("copy"),
247:        OsString::from("null"),
60:ready_observed=false
64:    <<<"${status}" && { ready_observed=true; break; }
69:if [ "${ready_observed}" != true ]; then
70:  printf 'prefetch did not reach ready: %s\n' "${status}" >&2
```

新增 Rust 路径中不存在 `ffprobe`、`sh` 或 `bash` 进程启动；`run_ffmpeg` 仍只通过 `Command::new(ffmpeg_bin)` 直接调用配置的 FFmpeg。

## Self-review

- 校验严格发生在 stage 与 publication 之间；single-file 和 HLS 都没有 publication-before-validation 窗口。
- mandatory `0:v:0` 能区分 review 复现的 streamless 输出；`0:a?` 允许无音频视频；`-c copy -f null -` 不解码转码且不改变 staged 文件。
- 失败不 fallback，API 状态进入 `failed`，`source_url` 不返回，final target 和 stage 均清理。
- 成功 fake 明确建模 validation 第二次调用；两条成功测试都检查实际 staged `.part` 输入和完整校验参数。
- 普通下载函数与早返回条件没有 diff，现有 reqwest 回归继续通过。
- smoke 的后续 FFprobe/时长断言只能在 `ready_observed=true` 后执行；pending 负路径已实际得到 exit 1。
- 没有修改 Task 1/Task 2 历史提交，没有 amend，没有触碰主 checkout。

## Concerns / 限制

- 无阻塞 concern。
- 发布前校验会额外顺序读取一次时间片/HLS 媒体，这是强制验证完整 staged 输出的预期成本；使用 stream copy/null，不产生转码成本或新文件。
- 本 review fix 按 findings 运行了 focused、全量 `media-gateway`、真实媒体 smoke、格式/语法/静态检查；没有新增 Linux Native 现场执行条件，完整目标机 bundle 验证限制仍与原报告一致。
