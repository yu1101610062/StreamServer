# stream_ingest / ZLM / FFmpeg 一次性重构方案（无兼容代码）

日期：2026-04-15

目标约束：

1. 尽可能保证现有功能。
2. 尽可能不做转码，除非明确指定或容器/协议不允许 copy。
3. 功能尽可能交给 ZLMediaKit，FFmpeg 仅做补充。
4. 不保留旧的 GPU 自动调度 / per-task 动态探测链路；保留节点级固定 GPU 转码后端；不保留 sidecar 兼容路径，不保留旧北向编解码兼容字段。
5. 新增明确 copy 白名单：视频 `H.264/H.265(HEVC)/VP8/VP9/AV1`，音频 `AAC/G711/OPUS/MP3`；白名单内优先 copy，白名单外统一自动转码。

---

## 一、最终行为定义

### 1. stream_ingest 的结束语义

#### 连续型任务（continuous）
满足任一条件即视为连续型：

- `task_type=stream_ingest` 且 `input.source_mode=live` 且 `record.duration_sec=None`
- `task_type=stream_ingest` 且 `input.source_mode=vod` 且 `input.loop_enabled=true` 且存在任意在线播放协议暴露（`expose.any_playback_enabled()==true`）

连续型任务只允许以下两种正常结束：

- 用户主动停止
- 明确达到 wall-clock 录制时长（仅当配置了 `record.duration_sec`）

其余退出都不是成功，而是异常退出：

- managed ffmpeg 退出码为 0 也不算成功
- ZLM proxy / RTP server 短时掉流也不立即终态，进入 grace + 恢复判定

#### 有界任务（bounded）
其余 `stream_ingest` 任务均视为有界任务：

- 快录（fast record）
- 非 loop 的 VOD replay
- 设置了明确时长且不属于 continuous 的场景

有界任务允许：

- 到时长后成功结束
- 文件/流自然 EOF 后成功结束（前提是语义上就是有界任务）

---

### 2. 录制时长语义

- `stream_ingest + live/realtime`：`record.duration_sec` 表示现实时间（wall clock）
- `stream_ingest + fast record`：`record.duration_sec` 表示输出文件时长（media timeline）

因此：

- realtime/live 路径不再给 FFmpeg 追加 `-t`
- fast record 保留 `-t`
- realtime/live 的停止由 agent 的 wall-clock watchdog 完成，不再交给 FFmpeg 媒体时间轴决定

---

### 3. 录制实现统一原则

#### 统一结论
`stream_ingest` 的在线录制统一改成 **ZLM record first**：

- `record.format=mp4` -> ZLM `startRecord(type=mp4)`
- `record.format=hls` -> ZLM `startRecord(type=hls)`
- `record.format=both` -> ZLM 分别启动 mp4 + hls 录制

删除 managed ingest 的 `mp4 companion ffmpeg` sidecar 机制。

理由：

- 你要求功能尽量交给 ZLM
- sidecar 需要双进程/双拉源或双链路，复杂且脆
- sidecar 使 mp4-only 比 both 更重，方向与目标相反
- sidecar 让主路与录制路看到的错误点不一致，易出现“在线还在，录制先挂”或相反

---

### 4. 编解码与内部接入载体统一原则

#### 4.1 明确的 copy 白名单
统一把“是否允许保留原编码接入”从模糊的容器探测，收敛为**显式白名单 + 容器/协议校验**：

- 视频白名单：`H264`、`H265/HEVC`、`VP8`、`VP9`、`AV1`
- 音频白名单：`AAC`、`G711(PCMA/PCMU)`、`OPUS`、`MP3`

统一规则：

- 轨道编解码在白名单内，且当前链路容器/协议允许 -> `copy`
- 轨道编解码不在白名单内 -> 自动转码
- 用户通过 `bitrate/fps/gop` 等显式提出处理诉求 -> 自动转码

默认自动转码落点：

- 视频 -> `H264`
- 音频 -> `AAC`

> 也就是“白名单 copy，白名单外统一转码”，不再按历史散落逻辑决定。

#### 4.2 ZLM proxy 与 managed FFmpeg 的职责边界
继续遵循 **ZLM first**，但要把 codec 能力边界说清楚：

- 能被 ZLM `addStreamProxy` 直接承接的输入，优先 ZLM proxy
- 但 proxy 路径只用于 **H264/H265 + AAC/G711/OPUS** 这一类 ZLM proxy API 明确支持的负载组合
- `VP8/VP9/AV1` 视频、`MP3` 音频这类“白名单内允许 copy、但 proxy API 不承接”的输入，不走 ZLM proxy，统一走 managed FFmpeg 作为接入 shim
- managed FFmpeg 在这类场景里只做**解封装/重发布**，不做转码，继续把原编码 copy 进 ZLM

因此最终边界变成：

- **ZLM proxy**：能直接吃的源，直接吃
- **managed FFmpeg copy ingress**：proxy 吃不了，但白名单内可保留原编码的源
- **managed FFmpeg transcode ingress**：白名单外或链路/封装不允许 copy 的源

#### 4.3 internal publish baseline
为了兑现 `VP8/VP9/AV1/OPUS` 的 copy 接入，internal publish 继续用 `RTMP -> ZLM`，但把它明确升级为 **Enhanced RTMP 基线能力**：

- ZLM 节点必须开启 `rtmp.enhanced=1`
- agent 启动时把它作为 capability/self-check 的硬约束
- 节点所带 ZLM 版本必须包含 `VP8/VP9/AV1` 全协议支持与 enhanced RTMP 支持，否则该节点不参与这批白名单 copy 任务
- 若节点要宣称支持 `VP8/VP9/AV1/OPUS` 的 copy ingest，但本地 ZLM 未开启 enhanced RTMP，则 agent 直接 fail-fast，不允许注册成可用 ingest 节点

这样：

- `H264/H265/AAC/G711/MP3` 走普通/增强 RTMP 都可
- `VP8/VP9/AV1/OPUS` 走增强 RTMP 保留原编码

#### 4.4 stream_ingest managed realtime/live
realtime/live 的默认策略从“KeepSourceFamily + Aac fallback”升级为**白名单 copy 策略**：

- 视频默认策略：`VideoOutputPolicy::CopyWhitelistedElseH264`
- 音频默认策略：`AudioOutputPolicy::CopyWhitelistedElseAac`

最终效果：

- 输入若是 `H264/H265/VP8/VP9/AV1 + AAC/G711/OPUS/MP3`，且当前接入链路容器/协议允许，默认 copy
- 白名单外编码统一自动转成 `H264 + AAC`
- 不再人为把 realtime ingest 硬压成 H264 路线
- 也不再把“能不能 copy”建立在临时探测之上，而是建立在**白名单 + 载体能力**之上

---

### 5. GPU 策略统一原则

这次删掉的是“系统自动猜这个任务应不应该上 GPU”和“任务执行时临时 probe 决定 encoder”，不是删掉 GPU 转码能力本身。

- 删除 core 的 `GpuPreferred` 自动调度偏好
- 是否落到 GPU 节点，完全由现有 `resource.required_labels` 决定
- 一旦任务被上层标签调度到 GPU 节点，并且 planner 判定这次任务确实需要转码，则该节点按**节点级固定转码后端**执行 GPU 转码
- copy 路径不因为“任务已经在 GPU 节点上”而主动转码
- capability snapshot 不再参与 per-task 动态切换，只用于节点启动自检、心跳画像和观测

#### 节点级固定后端约定

保留节点级 `AGENT_ACCELERATION_MODE`，但语义改为**节点固定转码后端声明**：

- `cpu`：该节点的转码后端固定为 CPU
- `gpu`：该节点的转码后端固定为 NVIDIA NVENC

要求：

- 调度靠标签；执行靠节点本地后端配置
- GPU 节点标签与 `AGENT_ACCELERATION_MODE=gpu` 必须一致，不一致时 agent 启动直接 fail-fast
- 不再在任务执行期通过 `ffmpeg -encoders/-decoders/-hwaccels` 或 `nvidia-smi` 做每任务探测

---

## 二、必须删除的旧逻辑

### A. 北向兼容字段

#### 删除 `crates/media-domain/src/task.rs`

从 `ProcessSpec` 删除：

- `video_codec`
- `audio_codec`
- `profile`
- `preset`

从 `ResourceSpec` 删除：

- `need_gpu`

同步删除所有围绕这些字段的逻辑判断：

- `process_requires_video_transcode()` 中对 `video_codec/profile/preset` 的判断
- `process_requires_audio_transcode()` 中对 `audio_codec` 的判断
- `media-core/src/repository.rs` 中 `strip_legacy_dispatch_fields()` 的兼容清理逻辑

> 不再做旧字段吞吐、裁剪、透传。旧字段变成纯粹的“未知字段”，不参与任何行为。

---

### B. managed ingest mp4 sidecar

#### 删除 `crates/media-agent/src/runtime.rs`

删除以下结构与逻辑：

- `companion_recording: Option<CompanionProcessPlan>`
- `CompanionProcessPlan`
- `CompanionProcessKind`
- `CompanionProcessState`
- `CompanionProcessMetadata`
- `build_stream_ingest_mp4_recording_plan()`
- `spawn_companion_process_monitor()`
- `spawn_adopted_companion_process_monitor()`
- `companion_recording_from_handle()`
- `update_companion_recording_metadata()`
- `ManagedRuntime.companion_pids`
- `ManagedRuntime.suppress_companion_events`
- `start_process_task()` 中的 companion spawn / wait / kill 逻辑
- orphan adopt 中 companion 接管逻辑
- `attach_file_artifact_metadata()` 中对 companion outputs 的回退读取

> 一次性删净，不留“以后也许还会用”的兼容壳。

---

### C. 删除任务内 GPU 动态探测，不删除 GPU 转码能力

#### 修改 `crates/media-agent/src/runtime.rs`

删除或停用以下**per-task 动态探测**调用链：

- `probe_gpu_devices()`
- `ffmpeg_supports_hwaccel()`
- `ffmpeg_supports_encoder()`
- `ffmpeg_supports_decoder()`
- `maybe_add_cuda_decoder()`

保留节点级后端选择，但不再在任务执行期临时探测。

`resolve_transcode_selection_for_input_family()` 改为**节点固定后端决策**：

- 节点 `AGENT_ACCELERATION_MODE=cpu`
  - H264 输出 -> `libx264`
  - HEVC 输出 -> `libx265`
- 节点 `AGENT_ACCELERATION_MODE=gpu`
  - H264 输出 -> `h264_nvenc`
  - HEVC 输出 -> `hevc_nvenc`
- 音频转码 -> `aac`

建议逻辑：

```rust
let video_encoder = match (settings.acceleration_mode.trim(), output_family) {
    ("gpu", VideoCodecFamily::Hevc) => "hevc_nvenc",
    ("gpu", _) => "h264_nvenc",
    ("cpu", VideoCodecFamily::Hevc) => "libx265",
    _ => "libx264",
};
```

约束：

- 只有 planner 判定“这次任务需要转码”时，才走上述 encoder 选择
- copy 路径仍然优先 `copy`
- 这次先保证**GPU 编码确定性**；硬解码不作为任务期自动行为
- capability snapshot 仍保留在 `capability.rs`，但用于节点启动自检/心跳画像，不进入 per-task probe 决策

---

## 三、按文件修改说明

## 1. `crates/media-domain/src/task.rs`

### 1.1 新增 TaskSpec 语义辅助函数

在 `impl TaskSpec` 中新增：

```rust
pub fn stream_ingest_is_continuous(&self) -> bool
pub fn stream_ingest_uses_wall_clock_record_duration(&self) -> bool
pub fn stream_ingest_requires_realtime_pacing(&self) -> bool
```

#### 建议定义

```rust
pub fn stream_ingest_is_continuous(&self) -> bool {
    if self.task_type != TaskType::StreamIngest {
        return false;
    }

    match self.input.source_mode {
        Some(SourceMode::Live) => self.record.duration_sec.is_none(),
        Some(SourceMode::Vod) => {
            self.input.loop_enabled.unwrap_or(false)
                && self.expose.any_playback_enabled()
                && self.record.duration_sec.is_none()
        }
        None => false,
    }
}

pub fn stream_ingest_uses_wall_clock_record_duration(&self) -> bool {
    self.task_type == TaskType::StreamIngest
        && self.record.enabled.unwrap_or(false)
        && self.record.duration_sec.is_some()
        && (
            self.input.source_mode == Some(SourceMode::Live)
                || self.stream_ingest_record_mode()
                    == Some(StreamIngestRecordMode::Realtime)
        )
}

pub fn stream_ingest_requires_realtime_pacing(&self) -> bool {
    self.task_type == TaskType::StreamIngest
        && self.input.source_mode == Some(SourceMode::Vod)
}
```

### 1.2 删除旧字段

- 从 `ProcessSpec` 删除 `video_codec/audio_codec/profile/preset`
- 从 `ResourceSpec` 删除 `need_gpu`

### 1.3 验证逻辑同步收缩

- 删除对上述旧字段的任何验证
- 保留 `bitrate/fps/gop` 作为“显式要求转码”的唯一北向处理控制项

---

## 2. `crates/media-core/src/repository.rs`

### 2.1 删除 legacy strip 逻辑

删除：

- `strip_legacy_dispatch_fields()`
- `task_spec_overlay()` 中对它的调用

原因：

- 旧字段已经不再属于平台模型
- 不保留兼容清洗器

### 2.2 overlay 保持纯净

`task_spec_overlay()` 只覆盖当前仍存在的字段，不再做历史字段裁剪。

---

## 3. `crates/media-core/src/control_plane.rs`

### 3.1 删除自动 GPU 偏好

修改 `task_execution_preference()`：

- `file_transcode` -> `CpuOnly`
- `stream_bridge` -> `CpuOnly`
- `stream_ingest` -> `CpuOnly`

推荐进一步一步到位：

- 删除 `ExecutionPreference::GpuPreferred`
- `pick_best_session_target()` 只走单次 `select(false)`
- `gpu_execution_eligible()` 不再参与调度路径选择

如果这次只做行为收敛，不做结构删减，则最少也要保证：

- 调度不再因为 `process.mode` 自动偏向 GPU 节点

### 3.2 调度唯一入口改成 labels

最终规则：

- 调度只认 `required_labels`
- 不再根据任务内容猜“应不应该上 GPU”

---

## 4. `crates/media-agent/src/runtime.rs`

### 4.1 `start_process_task()`

#### 必改项

- `worker_kind` 从错误的 `WorkerKind::ZlmProxy` 改成 `request.task_type.default_worker_kind()`

#### 删除项

- 整段 companion metadata 注入
- 整段 companion spawn 逻辑
- 整段 companion wait/kill 逻辑

### 4.2 `build_stream_ingest_plan()`

保留总入口，但职责改成：

- `Fast` -> `build_stream_ingest_fast_record_plan()`
- 其余 -> `build_stream_ingest_realtime_plan()`

建议把原 `build_file_to_live_plan()` 直接改名为：

- `build_stream_ingest_realtime_plan()`

避免继续把 live ingest 混成“file_to_live”的历史命名。

另外把 realtime 入口路由规则显式化：

- `can_use_zlm_stream_proxy(spec, probe)` 仅在源协议满足、且轨道集合属于 `H264/H265 + AAC/G711/OPUS` 这类 proxy-safe 集合时返回 true
- 只要出现 `VP8/VP9/AV1` 视频或 `MP3` 音频，即使 codec 在 copy 白名单内，也不走 ZLM proxy，而是走 managed realtime plan
- managed realtime plan 内部再根据白名单决定 `copy` 还是自动转码

### 4.3 `build_stream_ingest_realtime_plan()`

#### 输入 pacing 规则

- `spec.stream_ingest_requires_realtime_pacing()==true` 才追加 `-re`
- true live ingest 不再加 `-re`

#### live 输入稳定化

对非 VOD 输入统一加输入侧稳定化选项：

- `-thread_queue_size 1024`
- `-use_wallclock_as_timestamps 1`
- `-fflags +genpts+discardcorrupt`
- `-err_detect ignore_err`
- 组播输入额外加 `-max_delay 500000`

目的：

- 降低坏 TS / 时间戳异常导致的提前退出
- 降低 live 输入出现瞬断或抖动时的脆弱性

#### 编码策略

`append_process_args()` 调用改成：

- `default_mode = "copy_or_transcode"`
- `VideoOutputPolicy::CopyWhitelistedElseH264`
- `AudioOutputPolicy::CopyWhitelistedElseAac`

也就是：

- 视频 `H264/H265/VP8/VP9/AV1` 优先 copy
- 音频 `AAC/G711/OPUS/MP3` 优先 copy
- 白名单外统一自动转成 `H264 + AAC`
- 不再强制走 H264，也不再只把 `HEVC + AAC/MP3` 视为 copy-safe

#### 时长控制

- 若 `spec.stream_ingest_uses_wall_clock_record_duration()` 为 true，则**不追加** `-t`
- 否则（fast / file-bound）保留 `-t`

#### 录制路径

统一：

- 主 FFmpeg 只负责推 internal ZLM stream
- `recording = build_live_relay_recording(spec, &work_dir)?`
- `record.format=mp4/hls/both` 都走 ZLM record
- `outputs` 中加入 `recording.root_path`
- `managed_file_output_kind` 对 realtime ingest 置空（`None`）

> realtime ingest 的录像不再被视为 agent 本地文件托管输出，而是 ZLM record 产物。

### 4.4 `build_stream_ingest_fast_record_plan()`

快录保持 FFmpeg 单进程产物输出，但编码策略也统一切到**白名单 copy**，不再保留 `KeepSourceFamily` 的无条件继承。

#### 保留：

- `-t`
- `VideoOutputPolicy::CopyWhitelistedElseH264`
- `AudioOutputPolicy::CopyWhitelistedElseAac`
- 单进程 mp4/hls/both 输出

#### 不保留：

- 任何 task 内 GPU encoder/decoder 动态 probe 与自动切换
- 白名单外编码在 fast record 中继续被直接 copy 的旧机会主义行为

### 4.5 `append_process_args()` / `resolve_process_selection()`

#### 行为要求

- `passthrough` -> 强制 `copy/copy`
- `copy_or_transcode` -> 自动根据**白名单 + 容器/协议能力**决定 copy 与否
- `force_transcode` -> 只受 bitrate/fps/gop 这种明确处理要求驱动

具体 codec 规则：

- 视频 `H264/H265/VP8/VP9/AV1`：满足载体约束时 `copy`
- 音频 `AAC/G711/OPUS/MP3`：满足载体约束时 `copy`
- 其他视频编码：统一转 `H264`
- 其他音频编码：统一转 `AAC`

#### 删除旧 manual codec 控制

- 不再从 spec 读 video/audio codec/profile/preset

### 4.6 `resolve_process_selection()` / `resolve_transcode_selection_for_input_family()`

这两个阶段一起改成**白名单 copy 判定 + 节点固定后端转码**：

#### 先做 codec 白名单判定

新增统一辅助函数：

```rust
fn video_family_copy_whitelisted(f: VideoCodecFamily) -> bool
fn audio_family_copy_whitelisted(f: AudioCodecFamily) -> bool
```

规则固定为：

```rust
video: H264 | Hevc | Vp8 | Vp9 | Av1 => true
audio: Aac | G711 | Opus | Mp3 => true
others => false
```

然后由 `resolve_process_selection()` 先决定：

- 白名单内且当前输出容器/协议允许 -> `copy`
- 白名单外 -> 标记该轨必须转码

#### 再做转码后端选择

一旦某轨被判定必须转码，才进入节点固定后端映射：

```rust
let video_encoder = match settings.acceleration_mode.trim() {
    "gpu" => "h264_nvenc",
    _ => "libx264",
};
let audio_encoder = "aac";
```

若以后确有明确自动回落 HEVC 的场景，再单独引入 `ForceHevc` 分支；本次统一默认回落 `H264 + AAC`，避免“白名单外自动转什么”继续分叉。

删除全部 per-task GPU probe / nvenc 能力探测 / cuvid 动态分支。  
保留节点级固定 backend 映射：GPU 节点转码走 NVENC，CPU 节点转码走 libx264。

### 4.7 `should_auto_restart_process()`

改成：

- 连续型 managed `stream_ingest`：只要不是用户主动停、不是录制显式致命失败、且曾经 online，任何退出（包括 exit 0）都允许本地恢复
- 非连续型 managed `stream_ingest`：维持原有“异常退出才恢复”的策略

建议逻辑：

```rust
if continuous && stream_online(handle) {
    return true;
}
```

然后再对非 continuous 走原来的 `!exit_status.success()` 分支。

### 4.8 child wait 终态分类

在 `start_process_task()` 的 child wait 分类里新增一个优先分支：

- `status.success() && continuous stream && !was_stopped` -> `failed`

事件消息：

- `continuous stream exited unexpectedly`
- `reason = unexpected_stream_exit`

这样可以保证：

- continuous 任务不会再因为 exit 0 被当成 `succeeded`
- 如果本地 restart 成功，函数会提前 return
- 如果 restart 失败，就以 failed 交给 core 恢复

### 4.9 `classify_adopted_exit()`

同步改成同样语义：

- adopted continuous managed stream ingest 不再 `succeeded`
- 改成 `failed`
- message: `adopted continuous stream exited unexpectedly`

### 4.10 `spawn_startup_probe_monitor()`

#### 当前问题

- 只负责“上线前探测一次”
- 一旦 online 就 return
- 无法承接 realtime/live 的 wall-clock duration 控制

#### 修改后职责

这个 monitor 不再是一次性 startup probe，而是：

- 启动确认器
- realtime/live 录制的 wall-clock duration watchdog

#### 具体行为

1. 若超时仍未 online -> `startup_timeout` -> fail
2. 若 online 且需要录制 -> 立即调用 `start_stream_recording()`
3. 若录制启动失败 -> 直接 fatal（不再 degrade）
4. 若 `recording.started && recording.duration_sec.is_some()`：
   - 周期检查 `recording_duration_reached()`
   - 到时后：
     - `completion_reason = record_duration_reached`
     - `runtime.stop_requested = true`
     - 调 `stop_live_relay_recording()`
     - 向主 FFmpeg 发 `SIGTERM`
     - 由 child wait 路径产出最终 `succeeded`
5. 若不需要 wall-clock 监控，则在首次 running 后退出 monitor

### 4.11 `should_fail_on_recording_start_error()`

改成：

```rust
fn should_fail_on_recording_start_error(recording: &LiveRelayRecording) -> bool {
    true
}
```

也就是：

- 只要用户显式打开了录制
- 录制启动失败就让任务失败
- 不再“recording_degraded 后继续假装成功”

### 4.12 `spawn_live_relay_monitor()`

#### 当前问题

- online 之后一次 `Ok(false)` 就直接终态
- 对瞬断太敏感

#### 修改方案

加入离线 grace：

- `const STREAM_OFFLINE_GRACE_POLLS: u32 = 3;`
- 连续 3 次轮询都 offline 才判定真正掉流
- 中间任意一次恢复 online，计数归零

判定到真正掉流后：

- `stop_requested` -> `canceled`
- `completion_reason=record_duration_reached` -> `succeeded`
- `auto_close_enabled` -> `canceled`
- 其余 -> `failed`

> 这里不再把瞬时 miss 放大成终态。

### 4.13 `spawn_rtp_receive_monitor()`

#### 当前问题

- `Ok(None)` 直接移除 runtime，只发 `rtp_server_closed` warn，不给 terminal
- 没有真正 startup timeout / graceful offline 判定

#### 修改方案

新增：

- startup timeout（30 秒内未拿到有效媒体 -> failed）
- running 之后的 missing grace（连续 miss N 次才 failed）

终态规则：

- stop_requested -> `canceled`
- startup timeout -> `failed`
- running 后 server disappeared -> `failed`

不再只发 warn 后直接把 runtime 摘掉。

### 4.14 `attach_file_artifact_metadata()`

删除对 companion outputs 的回退扫描。

最终规则：

- managed file output 只从 `outputs` 和 `success_check` 取 artifact metadata
- realtime stream ingest 的录制产物由 ZLM hook / record_files 管

---

## 5. `crates/media-agent/src/capability.rs`

保留：

- capability snapshot
- ffmpeg protocol/format/encoder/decoder 探测
- GPU 设备与运行时采样
- ZLM 基线配置/能力采样

但改变用途：

- 仅用于节点画像、可观测性、运维页
- 不再被 runtime.rs 用于任务内编码器选择
- 用于节点启动自检，确认该节点是否满足新的 ingest baseline

新增节点启动硬检查：

- 若 agent 配置为 ingest 节点，则必须采到 `rtmp.enhanced=1`
- 若缺失该能力，则节点不注册或直接启动失败

目的：

- 保证 `VP8/VP9/AV1/OPUS` 的 copy ingest 不会在 internal publish 环节被悄悄降级成转码
- 保证“白名单可 copy”是节点级真实能力，而不是运行期碰运气

如果要进一步收口，可把以下函数改为“仅 capability 模块内部使用”：

- `ffmpeg_supports_hwaccel`
- `ffmpeg_supports_encoder`
- `ffmpeg_supports_decoder`

---

## 四、状态机变化

### managed realtime/live stream_ingest

#### 旧状态机

`STARTING -> RUNNING -> child exit(0) -> SUCCEEDED`

#### 新状态机

- 正常直播无时长：
  `STARTING -> RUNNING -> source jitter/EOF -> RECOVERING(local/core) -> RUNNING`
- 录制到时：
  `STARTING -> RUNNING -> duration reached -> STOPPING -> SUCCEEDED`
- 用户停止：
  `STARTING/RUNNING -> STOPPING -> CANCELED`
- 真正异常：
  `STARTING/RUNNING -> FAILED`

### ZLM proxy / RTP server

#### 旧逻辑

- 一次轮询 miss 即终态或直接移除 runtime

#### 新逻辑

- `RUNNING -> offline grace -> FAILED/CANCELED/SUCCEEDED`
- 不再因为一次 miss 直接误杀

---

## 五、重点场景的修改后结果

### 场景 1：UDP 组播 live ingest，未设置 duration

#### 期望
一直跑，源抖动自动恢复，不应正常结束。

#### 修改后

- 不加 `-re`
- 开 live stabilization
- child 若 exit 0 也不算成功
- 若已 online 后退出，走 recover/fail，不会 `succeeded`

### 场景 2：HEVC + AAC，既要在线流又要录 MP4，还要尽量不转码

#### 修改后

- 主 FFmpeg 优先 copy 推到 internal ZLM（RTMP）
- MP4 录制交给 ZLM `startRecord(mp4)`
- 删除第二个 FFmpeg

### 场景 2b：VP9 + OPUS 或 AV1 + MP3 接入

#### 修改后

- 不走 ZLM `addStreamProxy` 直代理
- 统一走 managed FFmpeg 作为接入 shim
- 只要节点满足 `rtmp.enhanced=1`，就按原编码 copy 推到 internal ZLM
- 在线播放和录制继续交给 ZLM
- 不因为 codec 是 `VP9/AV1/MP3` 就直接转码

### 场景 2c：MPEG2 Video + G722 Audio 接入

#### 修改后

- 视频不在白名单 -> 自动转 `H264`
- 音频不在白名单 -> 自动转 `AAC`
- 若在 GPU 节点上转码，则视频走 `h264_nvenc`

### 场景 3：坏 TS 文件录制时中间丢帧

#### 修改后

- fast record 仍走 FFmpeg，但输入增加 `discardcorrupt/genpts/ignore_err`
- realtime/live 不再让 `-t` 和媒体时间轴互相打架
- 更不容易“没到 duration 就提前结束”

### 场景 4：VOD loop 当直播放

#### 修改后

- 保留 `-re`
- 保留 `-stream_loop -1`
- 若无时长，则是 continuous
- 若配置了 `record.duration_sec`，按 wall-clock 截止

---

## 六、测试改造清单（必须一起改）

## 1. `crates/media-agent/src/runtime.rs` 单测

### 需要删除/改写的旧测试

- `build_file_to_live_plan_uses_companion_mp4_recording_process`
- `attach_file_artifact_metadata_uses_companion_recording_outputs`
- 所有 companion sidecar 相关测试
- 所有 GPU task-level encoder 选择相关测试

### 新增测试

#### realtime/live 计划构建

1. `build_stream_ingest_realtime_plan_uses_zlm_record_for_mp4`
2. `build_stream_ingest_realtime_plan_whitelists_hevc_for_copy`
3. `build_stream_ingest_realtime_plan_whitelists_vp9_opus_for_copy`
4. `build_stream_ingest_realtime_plan_whitelists_av1_mp3_for_copy`
5. `build_stream_ingest_realtime_plan_transcodes_non_whitelist_codecs_to_h264_aac`
6. `build_stream_ingest_realtime_plan_live_input_does_not_use_re`
7. `build_stream_ingest_realtime_plan_vod_replay_uses_re`
8. `build_stream_ingest_realtime_plan_wall_clock_duration_does_not_append_t`
9. `build_stream_ingest_fast_record_plan_media_duration_keeps_t`
10. `build_stream_ingest_fast_record_plan_respects_whitelist_copy_policy`
11. `build_stream_ingest_realtime_plan_adds_live_stabilization_flags_for_multicast`
12. `build_stream_ingest_realtime_plan_routes_vp9_av1_mp3_inputs_to_managed_not_proxy`
13. `build_stream_ingest_realtime_plan_requires_enhanced_rtmp_for_vp8_vp9_av1_opus_copy`

#### 生命周期语义

14. `should_auto_restart_process_restarts_continuous_stream_on_zero_exit`
15. `classify_adopted_exit_marks_continuous_stream_exit_as_failed`
16. `child_exit_success_does_not_succeed_for_continuous_stream`

#### ZLM 轮询宽限

17. `live_relay_monitor_requires_consecutive_offline_before_failure`
18. `rtp_receive_monitor_requires_consecutive_missing_before_failure`
19. `rtp_receive_monitor_fails_after_startup_timeout_without_media`

#### 录制严格性

20. `recording_start_error_is_fatal_when_record_enabled`
21. `startup_probe_monitor_stops_process_after_wall_clock_duration_reached`

## 2. `crates/media-core/src/control_plane.rs` 单测

### 需要改写

- 所有 “gpu_preferred_tasks_prefer_gpu_eligible_nodes” 一类测试
- 改成验证：
  - 没有 `required_labels` 时，GPU 节点和 CPU 节点同等候选
  - 有 `required_labels=["gpu"]` 时，才只选 GPU 标签节点

## 3. `crates/media-domain/src/task.rs` 单测

新增：

1. `stream_ingest_is_continuous_for_live_without_duration`
2. `stream_ingest_is_continuous_for_looped_vod_with_playback`
3. `stream_ingest_uses_wall_clock_record_duration_for_realtime_mode`
4. `stream_ingest_uses_wall_clock_record_duration_for_live_mode`
5. `stream_ingest_fast_record_uses_media_duration`
6. `video_family_copy_whitelist_accepts_h264_hevc_vp8_vp9_av1`
7. `audio_family_copy_whitelist_accepts_aac_g711_opus_mp3`
8. `non_whitelist_families_require_transcode`

---

## 七、推荐的提交拆分（但最终以一次合并提交落地）

虽然最终要“一次性改完”，但代码落地时建议按以下顺序在同一个 merge request 内提交，方便 review：

1. **模型收口**
   - 删除 task spec 旧字段
   - 删除 repository legacy strip
   - 删除 core 自动 GPU 偏好

2. **runtime 收敛**
   - 删 companion sidecar
   - stream_ingest realtime plan 改 ZLM-first
   - KeepSourceFamily + live stabilization
   - wall-clock duration watchdog

3. **生命周期修正**
   - continuous 语义
   - exit 0 不再成功
   - adopted exit 修正
   - rtp/live_relay grace + terminal 语义

4. **测试补齐**

最终 squash 成一个提交即可，不保留中间兼容 commit。

---

## 八、这次不做兼容的明确项

以下项明确不做兼容保留：

- 不保留 `need_gpu`
- 不保留 `video_codec/audio_codec/profile/preset`
- 不保留 mp4 sidecar 路径
- 不保留 per-task GPU encoder/decoder 动态探测与自动切换
- 不保留 continuous stream 的 `exit 0 == success`
- 不保留 rtp server disappeared 只发 warn 不给终态的旧行为

---

## 九、落地后的系统边界

### FFmpeg 负责

- 拉源
- 在必要时做最小转码
- 对 `VP8/VP9/AV1/MP3` 这类 proxy API 不直吃但允许 copy 的输入做接入 shim
- 推 internal ZLM stream
- fast record 直接产物

### ZLM 负责

- 协议分发
- 在线播放暴露
- realtime/live 录制（mp4/hls/both）
- live relay / rtp 接入状态确认
- enhanced RTMP 承接 `H265/VP8/VP9/AV1/OPUS` 的原编码发布

这正好符合目标：

- 白名单内默认 copy
- 白名单外统一转码
- 能交给 ZLM 的不让 FFmpeg 再做一遍
- proxy API 吃不下的白名单编码，由 FFmpeg 只做“接入补位”，不做多余转码

