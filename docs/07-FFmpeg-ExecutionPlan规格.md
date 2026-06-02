# 07. FFmpeg ExecutionPlan 规格

## 1. 文档目标

本文件定义 `media-agent` 执行 FFmpeg/ffprobe 的统一计划模型。平台不允许直接保存或下发原始 FFmpeg 命令，所有执行必须先渲染为 `ExecutionPlan`。

## 2. 统一对象模型

```json
{
  "plan_id": "0195...",
  "task_id": "0195...",
  "attempt_no": 1,
  "kind": "file_transcode",
  "probe": {
    "enabled": true,
    "timeout_ms": 10000
  },
  "inputs": [],
  "filters": [],
  "outputs": [],
  "runtime_policy": {},
  "artifact_collectors": []
}
```

## 3. 输入模型

| 字段 | 说明 |
| --- | --- |
| `kind` | `file`, `rtsp`, `rtmp`, `hls`, `http_mp4`, `udp_mpegts_multicast`, `rtp_multicast` |
| `url` | 规范化后的输入地址 |
| `input_options` | 输入级 FFmpeg 参数键值对 |
| `network_binding` | 本地网卡、`localaddr`、`reuse`、`ttl` 等 |
| `probe_timeout_ms` | 探测超时 |

约束：

- 组播输入必须显式带出 `interface_ip` 或 `localaddr`。
- 本地文件路径必须通过 allowlist 校验。

## 4. 过滤与处理模型

`process` 字段解析为三层：

1. 编解码决策层。
2. 滤镜层。
3. 流映射层。

固定模式：

- `passthrough`
- `copy_or_transcode`
- `force_transcode`

## 5. 输出模型

| 字段 | 说明 |
| --- | --- |
| `kind` | `file`, `udp_mpegts_multicast`, `rtp_multicast`, `rtmp_push` |
| `url` | 输出地址；`file` 由平台托管生成，`rtmp_push` 为外部 RTMP/RTMPS 地址 |
| `format` | `mp4`, `flv`, `mpegts`, `matroska` 等 |
| `muxer_options` | muxer 级参数 |
| `stream_options` | 码率、GOP、preset 等 |
| `on_fail` | `abort_all`, `ignore`, `retry_output` |

## 6. 命令渲染规则

渲染顺序固定：

1. 组装全局参数：`-hide_banner -nostdin -y`
2. 若启用进度采集，附加 `-progress pipe:1`
3. 逐个追加输入参数与 `-i`
4. 追加映射、编解码与滤镜参数
5. 逐个追加输出参数与目标地址

默认全局参数：

- `-loglevel info`
- `-stats_period 1`
- `-threads 0`

视频编码约束：

- 当选中的视频编码器是 `h264_nvenc`，且输入主视频像素格式为高 bit-depth（如 `yuv420p10le`、`p010le`）时，ExecutionPlan 必须显式追加 `-vf format=yuv420p -pix_fmt yuv420p`，不尝试保留 10-bit。
- 原因不是平台不希望保真，而是 `H.264 10-bit NVENC` 在现网 GPU/驱动组合上不具备稳定可移植性：
  - 当前生产基线 `FFmpeg 8.1` 的 `h264_nvenc` 不作为 `high10` profile 的稳定支持面。
  - 即使升级到更新的 FFmpeg / `nv-codec-headers`，在真实设备验证中，`RTX 5080 + driver 595.58.03` 仍会在 `h264_nvenc -profile:v high10` 打开阶段被设备拒绝。
  - 老显卡和不支持的驱动组合更不能假定支持该能力；若直接尝试 10-bit H.264 NVENC，会把任务失败从“可接受的质量回退”升级成“启动即报错”。
- 因此，平台对 `h264_nvenc` 的策略是“稳定优先”：高 bit-depth 输入统一回落到 `8-bit yuv420p`。
- 如果业务目标是保留 10-bit，应优先选择以下路径，而不是 `h264_nvenc`：
  - 保持 `HEVC` 直通
  - 使用 `hevc_nvenc`
  - 使用 CPU `libx264 High 10`

禁止项：

- 禁止直接拼接用户原始字符串。
- 禁止透传未知全局参数。

## 7. 能力校验

在真正启动 FFmpeg 之前，必须完成以下检查：

1. 输入协议存在于 `ffmpeg_protocols`。
2. 输出 muxer 存在于 `ffmpeg_formats`。
3. 指定编码器存在于 `ffmpeg_encoders`。
4. 若使用硬编，目标节点 GPU 能力满足。

不满足时返回 `412 PRECONDITION FAILED`，任务不进入 `STARTING`。

## 8. 进度解析

统一从 `-progress pipe:1` 读取键值：

- `frame`
- `fps`
- `stream_0_0_q`
- `bitrate`
- `total_size`
- `out_time_ms`
- `dup_frames`
- `drop_frames`
- `speed`
- `progress`

Agent 将其转换为 `task_progress` 事件。

## 9. 失败分类

| 分类 | 判定 |
| --- | --- |
| `INVALID_SPEC` | 渲染前参数校验失败 |
| `CAPABILITY_MISSING` | 节点能力不满足 |
| `INPUT_UNREACHABLE` | 输入源不可达或 ffprobe 失败 |
| `PROCESS_EXIT_NONZERO` | FFmpeg 非 0 退出 |
| `OUTPUT_BACKPRESSURE` | 某输出持续阻塞导致失败 |
| `RESOURCE_EXHAUSTED` | CPU、内存、磁盘或句柄耗尽 |

## 10. 产物收集

产物收集器固定支持：

- 文件存在性校验
- 文件大小校验
- ffprobe 二次探测
- 缩略图或封面抓取

对 `file_transcode`，只有产物收集成功后才能进入 `SUCCEEDED`。

## 12. 恢复规则

- Agent 本地必须保存每次渲染后的最终命令行、工作目录、PID 文件和输出清单。
- 恢复时优先接管仍在运行的进程，不重新渲染。
- 若进程已退出但产物完整，则允许直接进入 `SUCCEEDED`。
