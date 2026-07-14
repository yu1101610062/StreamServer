# Media Gateway 点播时间片预取设计

## 背景与目标

当前 `media-gateway` 对点播 HTTP 源执行普通 GET，并在读取完整响应后把文件发布到共享存储。即使任务只需要录像中的一小段，Gateway 仍会下载整个源文件，之后 Agent 才通过 `input.start_offset_sec` 和 `record.duration_sec` 做运行时截取。

本次改造让 Gateway 在预取阶段直接取得指定时间片：

- 偏移量沿用 `input.start_offset_sec`。
- 长度沿用 `record.duration_sec`。
- Gateway 只做时间截取，不转码、不改变音视频内容参数。
- 凡经 Gateway 成功重写的任务，发送给 Agent 的 `resolved_spec.input.start_offset_sec` 必须清除，避免重复偏移。
- `requested_spec` 保留调用方原始参数，`record.duration_sec` 继续传给 Agent，以维持任务生命周期和录像结束语义。

## 约束与精度边界

时间片通过 FFmpeg 输入侧快速定位和码流复制生成。Gateway 必须使用 `-c copy`，不得加入视频或音频编码器、滤镜、缩放、改帧率、改码率等转码参数。

因此，输出中的已编码音视频包及其编码参数保持不变，包括编码格式、分辨率、帧率、像素格式、声道和采样率。为了生成一个独立可播放的时间片，容器索引、时间戳、MP4 metadata 或 HLS 分片边界可以重新生成；输出文件不承诺与源文件字节一致。

码流复制无法同时保证任意帧精确起点。实际起点受源视频关键帧和源站 seek/Range 能力约束，允许落在目标时间附近的可解码关键帧处。若无法在不转码的前提下生成有效时间片，Prefetch 必须失败，不能自动转码。

Gateway 不会在共享存储中落一份完整源文件。网络侧能否直接跳过偏移量之前的数据取决于源站是否支持 HTTP Range、HLS 分片定位以及源容器索引；源站不支持快速定位时，FFmpeg 可能仍需传输或读取前段数据才能到达目标时间。这属于上游能力边界，不能通过通用下载协议保证只传输目标时间片对应的字节。

## 方案选择

采用“FFmpeg 直接读取远端源并执行时间片码流复制”：

```text
media-core
  -> POST /api/prefetch
     { task_id, source_url, target_path, source_kind,
       start_offset_sec, duration_sec }
  -> media-gateway 启动 FFmpeg
     -ss <offset> -i <source> -t <duration> -map ... -c copy
  -> 暂存输出验证成功后发布到共享存储
  -> Core 将 resolved_spec 改写为 file 输入并清除 start_offset_sec
  -> Agent 从时间片文件的 0 秒开始处理
```

不采用以下方案：

- HTTP `Range`：字节区间不能通用、可靠地换算成媒体时间。
- 完整下载后本地截取：仍消耗完整文件的网络传输和等待时间，不能实现本次目标。
- 强制转码：会改变内容参数，违反 Gateway 作为下载服务的边界。

## Core 侧改造

`GatewayAction::Prefetch` 和 Gateway 的内部请求增加：

- `source_kind`：取当前 `InputKind`，限定为 `http_mp4`、`http_ts` 或 `hls`，用于选择与源类型一致的输出封装。
- `start_offset_sec: Option<u32>`：从 `spec.input.start_offset_sec` 取得；`0` 归一化为未设置。
- `duration_sec: Option<u32>`：从 `spec.record.duration_sec` 取得；领域校验已保证已设置值大于 `0`。

Gateway 启用且任务符合现有 HTTP 点播路由条件时，Core 始终把上述时间参数传入 Prefetch。Gateway 成功后：

1. `input.kind` 改为 `file`。
2. `input.source_mode` 保持 `vod`。
3. `input.url` 改为共享存储相对路径。
4. `input.start_offset_sec` 设置为 `None`。
5. `record.duration_sec` 保持不变。

清除偏移量属于 Gateway 重写的统一后置条件。Relay 和 Prefetch 的成功结果都会清除 `resolved_spec.input.start_offset_sec`；正常情况下 Relay 任务不会携带有效点播偏移量，但统一清除可以保证发送给 Agent 的契约一致。

Gateway 未启用或任务未命中 Gateway 路由时，现有 Agent 偏移处理保持不变。

## Gateway 侧改造

### 配置

`GatewayConfig` 增加 `ffmpeg_bin`。启动时按以下优先级解析：

1. `MEDIA_GATEWAY_FFMPEG_BIN`
2. `FFMPEG_BIN`
3. `ffmpeg`

命令必须通过 `tokio::process::Command` 直接传递参数，不经过 shell，防止 URL 或路径被解释为命令文本。

运行 `media-gateway` 的主机必须提供 FFmpeg。Native worker/all-in-one 可复用安装器生成的 `FFMPEG_BIN`；独立 Gateway 或 core-only 主机必须通过 `MEDIA_GATEWAY_FFMPEG_BIN` 指向可执行文件。FFmpeg 缺失不影响无时间参数的普通 HTTP 下载，但任何时间片 Prefetch 都必须明确失败。

### 下载分支

- `start_offset_sec` 和 `duration_sec` 均未设置：继续走现有 Reqwest 普通 GET，保持完整响应字节下载行为。
- 任意一个时间参数已设置：走 FFmpeg 时间片分支。
  - 只有偏移量：从指定偏移下载到源结束。
  - 只有长度：从源开头下载指定长度。
  - 两者都有：从指定偏移下载指定长度。

FFmpeg 的基础语义为：

```text
-hide_banner -nostdin -y -loglevel error
[-ss <start_offset_sec>]
-i <source_url>
[-t <duration_sec>]
-map 0:v? -map 0:a? -map 0:s?
-map_metadata 0
-c copy
<封装与暂存输出参数>
```

不得在失败时重试为转码命令。

### 封装与发布

- `http_mp4`：输出仍为 MP4，显式选择 MP4 muxer。
- `http_ts`：输出仍为 MPEG-TS，显式选择 MPEG-TS muxer。
- `hls`：输出仍为 VOD HLS，生成完整播放列表和本地分片，所有媒体流使用码流复制。

MP4/TS 使用带正确媒体扩展名的任务级暂存文件，FFmpeg 成功退出且文件非空后再原子重命名为目标文件。

HLS 使用任务级暂存目录生成播放列表和分片；FFmpeg 成功退出、播放列表和至少一个分片存在后，再把完整目录发布到目标任务目录。Core 只有在状态变为 `ready` 后才调度 Agent，因此 Agent 不会看到未完成的 HLS 包。

### 状态和错误

Prefetch 保持现有 `pending -> ready|failed` 状态模型。

内部 Prefetch 状态响应增加 `time_slice_applied` 证明字段。只有 FFmpeg 时间片路径完成、输出验证通过并成功发布后，Gateway 才返回 `true`；普通完整下载、`pending` 和 `failed` 均返回 `false`。Core 请求了正偏移量或长度时，只接受同时携带 `ready` 和 `time_slice_applied=true` 的响应；旧 Gateway 缺少该字段时按 `false` 处理并拒绝重写，避免静默丢失时间窗口。无时间参数的 Prefetch 不要求该证明，因此仍兼容旧 Gateway 响应。

以下情况标记为 `failed`：

- FFmpeg 不存在或无法启动。
- FFmpeg 非零退出。
- 输出文件为空。
- HLS 缺少播放列表或媒体分片。
- 暂存内容无法安全发布到目标路径。

失败时清理本次暂存文件或暂存目录，不删除此前已经发布且不属于本次执行的文件。错误原因记录 FFmpeg 的简短 stderr；不得回退为完整下载，也不得把原始 URL 直接交给 Agent。

Core 继续使用现有 Prefetch 轮询和超时机制。超时或 Gateway 失败时，任务沿用现有 `source_gateway_failed` 失败路径。

## 兼容性

- 不新增北向任务字段，不改变任务 API。
- 无时间参数的点播任务保持现有完整下载行为。
- Gateway 关闭时，Agent 继续按现有 `input.start_offset_sec`/`record.duration_sec` 逻辑处理。
- Gateway 开启并成功生成时间片时，Agent 不再执行偏移，但仍保留长度字段。
- 现有 `requested_spec` 仍是原始请求真相；只修改实际调度使用的 `resolved_spec`。

## 测试与验收

### Core 单元测试

- 点播 Prefetch action 正确携带 `source_kind`、偏移量和长度。
- `start_offset_sec=0` 被归一化为未设置。
- Prefetch 成功改写后 `input.start_offset_sec == None`。
- Relay 成功改写后同样清除偏移字段。
- `record.duration_sec` 在改写后保持不变。
- Gateway 未命中的任务不清除偏移字段。

### Gateway 单元和集成测试

- 无时间参数继续走 HTTP 完整下载。
- 偏移量和长度正确生成 `-ss`、`-t`，且 `-ss` 位于 `-i` 前。
- 命令包含 `-c copy`，且不包含任何具体视频/音频编码器或滤镜参数。
- MP4、TS、HLS 分别选择正确封装和发布方式。
- FFmpeg 非零退出、缺少输出和缺少 HLS 分片时状态为 `failed` 并清理暂存内容。
- 成功后只返回共享存储相对路径，不泄露暂存路径。

测试使用可控的 FFmpeg 测试程序记录参数并生成最小输出，避免依赖外网。补充一个本机真实 FFmpeg 时间片测试，验证输出的 codec、宽高、帧率和音频参数与输入一致，且输出时长位于关键帧级截取的允许误差范围内。

### 回归验证

```text
cargo fmt --check
cargo test -p media-gateway
cargo test -p media-core source_gateway
cargo test --workspace
cargo check --workspace
git diff --check
```

Native bundle 启动 smoke 增加 `MEDIA_GATEWAY_FFMPEG_BIN`，部署文档说明 Gateway 时间片依赖 FFmpeg runtime，并明确“不转码、关键帧级起点”的行为边界。

## 完成标准

给定一个 Gateway 可访问的 HTTP 点播源、`input.start_offset_sec=600` 和 `record.duration_sec=180`：

1. Gateway 只在共享存储发布约第 600 秒开始、长度约 180 秒的可播放时间片，不落完整源文件；网络读取量受源站 seek/Range 能力约束。
2. 输出音视频编码参数与输入一致，执行日志中不存在转码编码器或滤镜。
3. Core 保存的 `requested_spec.input.start_offset_sec` 仍为 `600`。
4. 发送给 Agent 的 `resolved_spec.input.start_offset_sec` 不存在。
5. 发送给 Agent 的 `resolved_spec.record.duration_sec` 仍为 `180`。
6. Gateway 或 FFmpeg 失败时任务进入 `source_gateway_failed`，不会回退到完整下载或 Agent 直连源站。
