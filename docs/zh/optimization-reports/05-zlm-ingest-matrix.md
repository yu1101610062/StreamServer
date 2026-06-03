---

# FFmpeg + ZLMediaKit 内部接入动态选择机制与 Copy / Transcode 决策矩阵

**文档版本**：v1.0
**适用场景**：FFmpeg 将多种来源媒体流接入本机 ZLMediaKit，ZLMediaKit 再统一对外提供 RTMP / HTTP-FLV / RTSP / HLS / fMP4 / MP4 录制
**目标**：在保证链路稳定的前提下，**能 copy 时尽量 copy，仅在必要时做最小转码**

---

## 1. 结论摘要

### 1.1 总体结论

对于 **FFmpeg → 本机 ZLMediaKit** 的内部接入主路径，建议采用：

* **默认主路径：RTMP/FLV**
* **扩展保真路径：enhanced-RTMP**
* **例外兜底路径：RTSP/TCP ANNOUNCE**

这个结论的核心原因不是“RTSP 理论上不行”，而是：

1. **FFmpeg 的 RTSP 输出链路在写 header / SDP 阶段就会对某些 codec 配置提出更严格要求**，尤其是 AAC。
2. **FFmpeg 的 FLV/RTMP 路径对主流组合更务实**，特别是 `H264 + AAC/G711/MP3`。
3. **ZLMediaKit 自身也更适合把 RTMP 作为默认 direct-proxy 主路径**；其配置与 README 都明确体现了 RTMP 的 direct proxy 与 enhanced-RTMP 扩展能力，而 RTSP 官方配置注释则明确指出 RTSP direct proxy 存在额外兼容性问题。 ([GitHub][1])

### 1.2 设计原则

本方案遵循以下原则：

1. **默认优先普通 RTMP**
2. **确有收益时再启用 enhanced-RTMP**
3. **RTSP 不作为默认主入口，而作为保留 copy 入口**
4. **先尝试 copy，失败后只转不兼容的那一路**
5. **除非全部不兼容，否则不要整体转成 `H264 + AAC`**

---

## 2. 范围与前提

### 2.1 输入来源范围

* 本地 `MPEG-TS`
* `HTTP-TS`
* `HLS`
* `MP4 / MOV`
* `MKV`
* `RTSP / RTMP`

### 2.2 主要 codec 范围

**视频：**

* H.264
* H.265 / HEVC
* VP8
* VP9
* AV1

**音频：**

* AAC
* G711（PCMA / PCMU）
* Opus
* MP3
* MP2

### 2.3 ZLMediaKit 能力边界

ZLMediaKit 当前公开资料显示：

* **RTSP** 支持 `H264/H265/AAC/G711/OPUS/MP3/VP8/VP9/AV1/MP2`
* **RTMP** 支持 `H264/H265/AAC/G711/OPUS/MP3`
* **enhanced-RTMP** 支持 `H265/VP8/VP9/AV1/OPUS`

同时，ZLMediaKit 配置中：

* `rtsp.directProxy=1`
* `rtmp.directProxy=1`
* `rtmp.enhanced=1`

且 RTSP 配置注释明确写到：**RTSP direct proxy 存在 GOP/I 帧、SPS/PPS、WebRTC 等额外问题，而 RTMP 原生就没有这些问题，因此 RTMP 天然更适合 direct proxy。** ([GitHub][2])

---

## 3. 为什么不能只看 codec 名字来决定入口

动态选择入口时，**不能只看 `video_codec / audio_codec`**。
至少还需要同时看下面这些信息：

* 输入来源家族：`mpegts/hls`、`mp4/mov`、`mkv`、`rtsp/rtmp`
* 视频 codec
* 音频 codec
* **AAC extradata / AudioSpecificConfig 是否存在**
* **视频 extradata 是否存在**
* 音频采样率、声道数
* 当前节点是否开启 `rtmp.enhanced=1`
* 当前 FFmpeg 构建是否已验证 enhanced-FLV / enhanced-RTMP 路径
* 是否允许一次失败后的快速协议回退

原因很简单：
你已经验证到的 `mpegts/hls + AAC -> RTSP` 问题，本质就不是“codec 名字不兼容”，而是 **FFmpeg 在 RTSP 的 ANNOUNCE/SDP 阶段，需要可用的 AAC config / global headers；没有时会直接在写 header 阶段失败。** FFmpeg 官方文档也说明 `aac_adtstoasc` 的作用是把 ADTS AAC 转成 MPEG-4 AudioSpecificConfig，并指出它常用于 `MP4A-LATM`、`FLV`、`MOV/MP4` 等输出；同时 RTSP muxer 支持 `rtsp_flags=latm`，用于把 AAC 从默认的 `MPEG4-GENERIC` 切到 `MP4A-LATM`。ZLMediaKit 的 RTSP 推流流程文档则表明，RTSP 推流先发送 `ANNOUNCE`，其中 SDP 音频示例正是 `MPEG4-GENERIC` 且携带 `config=`。 ([FFmpeg][3])

---

## 4. 动态选择机制设计

---

### 4.1 总体机制

动态选择入口的正确做法不是“看 codec 后直接三选一”，而是：

1. **探测输入**
2. **生成候选入口列表**
3. **按优先级尝试 copy**
4. **在已知 muxer/header 错误上快速失败**
5. **切换到下一个入口或只转不兼容的一路**

这可以理解为一个 **Ingress Capability Router**，而不是一张硬编码的大表。

---

### 4.2 输入探测字段

建议至少探测以下字段：

```text
source_family
video_codec
audio_codec
video_extradata_present
audio_extradata_present
audio_sample_rate
audio_channels
zlm_rtmp_enhanced_enabled
ffmpeg_build_enhanced_flv_verified
allow_rtsp_latm
```

其中最关键的是：

* `source_family`
* `audio_codec`
* `audio_extradata_present`
* `zlm_rtmp_enhanced_enabled`
* `ffmpeg_build_enhanced_flv_verified`

---

### 4.3 入口优先级

建议固定使用以下优先级：

#### 第一优先级：普通 RTMP/FLV

适用于：

* `video = H264`
* `audio ∈ {AAC, G711, MP3}`

原因：

* FFmpeg 的 FLV muxer 原生就映射了 `AAC / PCM_MULAW / PCM_ALAW / MP3`
* 对主流链路最省心
* ZLMediaKit 也天然倾向 RTMP direct proxy 路线 ([GitHub][4])

#### 第二优先级：enhanced-RTMP

适用于：

* `video ∈ {H265, AV1, VP9}`
* 或 `audio = Opus`

前提：

* 当前节点 `rtmp.enhanced=1`
* 当前 FFmpeg 构建已经实测通过该类流的 enhanced-FLV / enhanced-RTMP 路径

原因：

* FFmpeg 的 FLV muxer 已包含 extended-FLV 路径，覆盖 `HEVC / AV1 / VP9` 视频与 `Opus` 音频
* ZLMediaKit 也显式声明 enhanced-RTMP 覆盖 `H265/VP8/VP9/AV1/OPUS` ([GitHub][4])

#### 第三优先级：RTSP/TCP ANNOUNCE

适用于：

* `MP2` 音频这类 RTMP/FLV 不友好场景
* `VP8` 这类不建议默认塞进 enhanced-RTMP 的场景
* 或某些明确要利用 RTP/RTSP payload 覆盖面的场景

但要加一个硬规则：

> **若 `audio = AAC` 且 `source_family ∈ {mpegts, hls}` 且 `audio_extradata_present = false`，则禁止直接判到 RTSP copy。**

原因：

* FFmpeg RTP/RTSP muxer 支持的 codec 很广，包含 `H264 / HEVC / AAC / MP2 / MP3 / PCMA / PCMU / VP8 / VP9 / AV1 / Opus`
* 但 RTSP 的 AAC 默认走 `MPEG4-GENERIC`，写 SDP 时缺少 global headers / config 就会失败 ([GitHub][5])

---

### 4.4 推荐状态机

```text
Probe input
  ↓
Build candidate list
  ↓
Try best candidate with copy
  ↓
Success → use it
  ↓
Fail on known header/muxer reason?
  ├─ Yes → switch protocol or transcode minimal stream
  └─ No  → stop / mark unsupported
```

---

### 4.5 推荐伪代码

```text
if video == H264 and audio in {AAC, PCMA, PCMU, MP3}:
    candidates = [RTMP]

elif enhanced_enabled and build_verified and (video in {H265, AV1, VP9} or audio == Opus):
    candidates = [ENHANCED_RTMP, RTSP]

elif video == VP8:
    candidates = [RTSP]
    # enhanced-RTMP 只有在端到端验证通过后再放开

elif audio == MP2:
    candidates = [RTSP]

else:
    candidates = [RTSP]

# RTSP hard guard
if audio == AAC and source_family in {MPEGTS, HLS} and not audio_extradata_present:
    remove RTSP copy candidate

# try copy
for candidate in candidates:
    try candidate copy
    if success:
        return copy(candidate)

# minimal transcode fallback
if only audio incompatible:
    return transcode_audio_keep_video()

if only video incompatible:
    return transcode_video_keep_audio()

return transcode_to_h264_aac()
```

---

## 5. 协议能力基线矩阵

下面这张表不是最终业务矩阵，而是**协议层安全基线**。

### 5.1 协议能力基线

| 内部入口          | 视频 copy 安全基线                  | 音频 copy 安全基线                  | 前提                               | 建议定位       |
| ------------- | ----------------------------- | ----------------------------- | -------------------------------- | ---------- |
| RTMP/FLV      | H264                          | AAC / G711 / MP3              | 无需 enhanced                      | 默认主路径      |
| enhanced-RTMP | H265 / AV1 / VP9              | Opus（以及与其配套的 AAC/MP3/G711）    | `rtmp.enhanced=1` 且 FFmpeg 构建已验证 | 扩展保真路径     |
| RTSP/TCP      | H264 / H265 / VP8 / VP9 / AV1 | AAC / G711 / MP3 / MP2 / Opus | AAC 需额外关注 SDP/config             | 例外 copy 路径 |

这张表依据如下：

* FFmpeg `rtpenc.c` 的 `is_supported()` 和发送逻辑覆盖 `H264/HEVC/AAC/MP2/MP3/PCMA/PCMU/VP8/VP9/AV1/Opus`
* FFmpeg `flvenc.c` 明确映射 `AAC/PCMA/PCMU/MP3`，并通过 extended-FLV 路径覆盖 `HEVC/AV1/VP9` 视频与 `Opus` 音频
* ZLMediaKit README 声明 RTMP enhanced 覆盖 `H265/VP8/VP9/AV1/OPUS`
* 但 FFmpeg 当前 FLV 写出主路径里没有把 **VP8** 列入 extended-FLV 视频安全基线，因此 **VP8 不应默认开放 enhanced-RTMP copy**，而应视为“需端到端验证后再放开”的例外项。 ([GitHub][5])

---

## 6. 输入来源修正矩阵

这一层解决的是：**同样 codec，不同来源家族，copy 风险不同。**

### 6.1 来源修正规则

| 输入来源家族        | 音频 codec   |               RTMP/FLV copy | RTSP copy | 结论                       |
| ------------- | ---------- | --------------------------: | --------: | ------------------------ |
| MPEGTS / HLS  | AAC        |                           是 |   **高风险** | 默认优先 RTMP；不要默认 RTSP copy |
| MPEGTS / HLS  | G711 / MP3 |                           是 |         是 | 按主矩阵正常选择                 |
| MPEGTS / HLS  | MP2        |                           否 |         是 | 优先 RTSP 或音频转 AAC         |
| MPEGTS / HLS  | Opus       | 否（普通 RTMP） / 条件允许（enhanced） |         是 | 优先 enhanced-RTMP 或 RTSP  |
| MP4 / MOV     | AAC        |                           是 |      条件允许 | AAC 通常更友好，可先尝试 RTSP copy |
| MKV           | AAC        |                           是 |      条件允许 | 与 MP4/MOV 类似，但仍建议保留失败回退  |
| RTSP / RTMP 源 | AAC        |                           是 |      条件允许 | 先按主矩阵选，失败再回退             |

其中最重要的一条是：

> **`mpegts/hls + AAC -> RTSP copy` 不是安全基线。**

因为 FFmpeg 文档明确指出 `aac_adtstoasc` 适合把 ADTS AAC 转成 `MP4A-LATM / FLV / MOV/MP4` 所需的 AudioSpecificConfig；而 RTSP 的默认 AAC 路径是 `MPEG4-GENERIC`，ZLMediaKit 的 RTSP 推流文档里也展示了 `a=fmtp ... config=...`。因此即便 `aac_adtstoasc` 对 FLV/MP4 常常有帮助，它也**不等于**能稳定解决 RTSP 默认 AAC/SDP 的 header-time 需求。 ([FFmpeg][3])

---

## 7. 最终 Copy / Transcode 选择矩阵

下面这张表是**业务落地时直接可用**的矩阵。

### 7.1 按输入来源族 + 视频 + 音频给出首选入口

| 输入来源族         | 视频               | 音频               | 首选内部入口                   | 是否允许 copy | 不允许时的最小转码                                 |
| ------------- | ---------------- | ---------------- | ------------------------ | --------- | ----------------------------------------- |
| MPEGTS / HLS  | H264             | AAC              | **RTMP**                 | 是         | 若必须走 RTSP，则只转音频为 AAC                      |
| MPEGTS / HLS  | H264             | G711             | **RTMP**                 | 是         | 无                                         |
| MPEGTS / HLS  | H264             | MP3              | **RTMP**                 | 是         | 若 FLV 采样率不支持，则转 AAC                       |
| MPEGTS / HLS  | H264             | MP2              | **RTSP**                 | 是         | 或音频转 AAC 后走 RTMP                          |
| MPEGTS / HLS  | H264             | Opus             | **enhanced-RTMP** / RTSP | 条件允许      | 音频转 AAC                                   |
| MPEGTS / HLS  | H265             | AAC              | **enhanced-RTMP**        | 条件允许      | 若 enhanced 不可用，则视频转 H264；或 RTSP + 音频转 AAC |
| MPEGTS / HLS  | H265             | Opus             | **enhanced-RTMP**        | 条件允许      | 不行则视频转 H264 或音频转 AAC                      |
| MPEGTS / HLS  | VP8              | 任意主流音频           | **RTSP**                 | 是         | 若必须统一 RTMP，则视频转 H264                      |
| MPEGTS / HLS  | VP9 / AV1        | AAC / Opus       | **enhanced-RTMP**        | 条件允许      | 不行则 RTSP 或视频转 H264                        |
| MP4 / MOV     | H264             | AAC              | **RTMP**                 | 是         | 若业务要求 RTSP，可先试 copy，失败仅转音频                |
| MP4 / MOV     | H265             | AAC              | **enhanced-RTMP**        | 条件允许      | 不行则 RTSP 或视频转 H264                        |
| MP4 / MOV     | VP9 / AV1        | AAC / Opus       | **enhanced-RTMP**        | 条件允许      | 不行则 RTSP 或视频转 H264                        |
| MP4 / MOV     | VP8              | 任意主流音频           | **RTSP**                 | 是         | 若必须 RTMP，则视频转 H264                        |
| MKV           | H264             | AAC / G711 / MP3 | **RTMP**                 | 是         | RTSP 仅作为例外路径                              |
| MKV           | H265 / VP9 / AV1 | AAC / Opus       | **enhanced-RTMP**        | 条件允许      | 不行则 RTSP 或视频转 H264                        |
| MKV           | VP8              | 任意主流音频           | **RTSP**                 | 是         | 若必须 RTMP，则视频转 H264                        |
| RTSP / RTMP 源 | H264             | AAC / G711 / MP3 | **RTMP**                 | 是         | 无                                         |
| RTSP / RTMP 源 | H265             | AAC / Opus       | **enhanced-RTMP**        | 条件允许      | 不行则 RTSP 或视频转 H264                        |
| RTSP / RTMP 源 | VP9 / AV1        | AAC / Opus       | **enhanced-RTMP**        | 条件允许      | 不行则 RTSP 或视频转 H264                        |
| RTSP / RTMP 源 | VP8              | 任意主流音频           | **RTSP**                 | 是         | 若必须 RTMP，则视频转 H264                        |
| 任意            | 任意               | MP2              | **RTSP**                 | 是         | 若必须 RTMP，则音频转 AAC                         |
| 任意            | 任意               | 不在基线内的音频         | 视视频而定                    | 否         | 音频转 AAC                                   |
| 任意            | 不在基线内的视频         | 任意               | 视音频而定                    | 否         | 视频转 H264                                  |

---

## 8. 已知强制转码条件

以下组合建议直接判为 **强制最小转码**，不要再做 copy 冒险：

### 8.1 强制音频转码

#### 条件 A

* 内部协议 = `RTSP`
* 音频 = `AAC`
* 输入来源 = `MPEGTS / HLS`
* 且 `AAC extradata 不存在`

**处理：**

* `-c:v copy -c:a aac`

这是最重要的一条硬规则。
原因是 FFmpeg RTSP/SDP 的默认 AAC 路径需要 config/global headers；没有时会直接报
`AAC with no global headers is currently not supported.` ([GitHub][6])

#### 条件 B

* 内部协议 = `RTMP/FLV`
* 音频 = `MP3`
* 采样率不在 `{44100, 22050, 11025}`

**处理：**

* 音频转 AAC

因为 FFmpeg `flvenc.c` 明确限制了 MP3 的 FLV 采样率范围，否则会报 `FLV does not support sample rate ... choose from (44100, 22050, 11025)`。 ([GitHub][4])

#### 条件 C

* 内部协议 = `RTMP/FLV`
* 音频 = `MP2`

**处理：**

* 音频转 AAC

原因是 FFmpeg FLV 基线映射里有 `AAC/PCM_MULAW/PCM_ALAW/MP3`，但没有 MP2。 ([GitHub][4])

---

### 8.2 强制视频转码

#### 条件 D

* 内部协议必须统一为 RTMP
* 视频 = `VP8`

**处理：**

* 视频转 `H264`

原因：ZLMediaKit README 虽声明 enhanced-RTMP 覆盖 VP8，但 FFmpeg 当前 FLV 写出安全基线里未把 VP8 列为 extended-FLV 主路径，因此 VP8 不应作为默认生产基线放开。 ([GitHub][2])

#### 条件 E

* enhanced-RTMP 不可用
* 视频 ∈ `{H265, AV1, VP9}`
* 但业务又要求统一走 RTMP

**处理：**

* 视频转 `H264`

---

## 9. 失败回退策略

建议在实现中保留一次**协议级快速回退**。

### 9.1 推荐回退顺序

#### 场景 1：首选 RTMP

* RTMP copy 失败
  → 若满足 enhanced 条件，切换 enhanced-RTMP
  → 否则只转不兼容音频或视频

#### 场景 2：首选 enhanced-RTMP

* enhanced-RTMP copy 失败
  → 尝试 RTSP copy
  → 不行则最小转码

#### 场景 3：首选 RTSP

* RTSP copy 失败，且错误为 AAC global headers/config 问题
  → 直接 `-c:a aac`
  → 不要反复尝试只靠 `aac_adtstoasc` 去赌

---

### 9.2 建议识别的失败类型

| 错误类型                                                                       | 含义                                  | 建议处理            |
| -------------------------------------------------------------------------- | ----------------------------------- | --------------- |
| `AAC with no global headers is currently not supported`                    | RTSP/AAC 在 SDP 阶段缺 config           | RTSP 下只转音频为 AAC |
| `FLV does not support sample rate ...`                                     | MP3 采样率不适配 FLV                      | 音频转 AAC         |
| `Tag ... incompatible with output codec` / `codec not compatible with flv` | 当前 FLV / enhanced-FLV 不接受该 codec 组合 | 切 RTSP 或做最小转码   |
| 其他网络/握手类错误                                                                 | 不是 codec 问题                         | 不应直接触发转码策略      |

---

## 10. 对 `aac_adtstoasc` 的定位

### 10.1 它能解决什么

`aac_adtstoasc` 的作用是：

* 把 **ADTS AAC**
* 转成 **MPEG-4 AudioSpecificConfig**
* 并移除 ADTS 头

FFmpeg 文档明确指出，它常用于：

* `MP4A-LATM`
* `FLV`
* `MOV/MP4` 及相关格式

而且对 `MP4A-LATM`、`MOV/MP4` 和相关格式会自动插入。
此外，FFmpeg 的输出格式还有 `autobsf`，默认会按输出格式自动应用所需 bitstream filter。 ([FFmpeg][3])

### 10.2 它不能被当成 RTSP/AAC 的通用解法

对你这里的 `RTSP` 输出来说，`aac_adtstoasc` 不能被当成通用解法，原因有两点：

1. **RTSP 先写 ANNOUNCE/SDP，再开始发包**
2. RTSP 默认 AAC packetization 是 `MPEG4-GENERIC`，ZLMediaKit 文档示例里 `a=fmtp` 明确需要 `config=`
3. 如果 FFmpeg 在写 SDP 时拿不到它想要的 global headers / config，header 阶段就已经失败了

所以：

* `aac_adtstoasc` 对 **FLV / MP4** 常常有帮助
* 但对 **RTSP 默认 AAC 路径**，它并不等价于“稳过”

RTSP 的确支持 `rtsp_flags=latm`，把 AAC 改走 `MP4A-LATM`，这是可实验选项；但它应当被视为**单独验证后的例外策略**，而不是生产默认路径。 ([FFmpeg][3])

---

## 11. 最终工程建议

### 11.1 如果追求短期稳定

**结论：选 RTMP/FLV 做内部主路径。**

推荐策略：

* 默认：`RTMP`
* 扩展：`enhanced-RTMP`
* 例外：`RTSP`

优点：

* 主流场景判定树短
* FFmpeg 与 ZLM 组合更务实
* 可明显减少 `AAC + RTSP + SDP/config` 这类前置问题

---

### 11.2 如果追求长期统一

长期不要理解成“把所有东西都硬塞 RTSP”。

更合理的长期统一路线是：

* **一个动态选择器**
* **三条入口车道**

    * 普通 RTMP
    * enhanced-RTMP
    * RTSP 例外路径
* **统一的最小转码回退**

也就是说，长期统一的对象应该是 **选择机制**，而不是 **单一协议**。

---

### 11.3 如果必须继续走 RTSP

建议至少对以下组合强制最小转码：

#### 必须强制音频转码的组合

* `source_family ∈ {MPEGTS, HLS}`
* `audio = AAC`
* `internal_protocol = RTSP`
* `audio_extradata_present = false`

处理方式：

```bash
-c:v copy -c:a aac
```

#### 其他建议

* `VP8`、`MP2` 可以优先保留 RTSP copy
* `H264 + AAC/G711/MP3` 不建议硬改成 RTSP 主路径
* `LATM` 只在你已完成端到端验证时启用，不要作为默认规则

---

## 12. 推荐的生产规则（最终版）

可以直接落成如下规则：

1. **H264 + AAC/G711/MP3**

    * 走 **普通 RTMP copy**

2. **H265 / AV1 / VP9**

    * 若 `rtmp.enhanced=1` 且当前 FFmpeg 构建已验证
      → 走 **enhanced-RTMP copy**
    * 否则
      → 尝试 **RTSP copy**
    * 再不行
      → 视频转 `H264`

3. **Opus**

    * 若 enhanced 路径已验证
      → 走 **enhanced-RTMP**
    * 否则
      → `RTSP copy`
    * 再不行
      → 音频转 `AAC`

4. **VP8**

    * 默认 **RTSP copy**
    * 若必须统一 RTMP
      → 视频转 `H264`

5. **MP2**

    * 默认 **RTSP copy**
    * 若必须统一 RTMP
      → 音频转 `AAC`

6. **AAC + MPEGTS/HLS + RTSP**

    * **不要默认 copy**
    * 直接最小转码：`-c:v copy -c:a aac`

---

这份文档已经可以直接作为设计基线使用。
**要我继续的话，我就把它改成可直接入仓的 `.md` 成稿格式，顺手再补一版 Rust 侧的判定伪代码附录。**

[1]: https://github.com/zlmediakit/ZLMediaKit/blob/master/conf/config.ini "ZLMediaKit/conf/config.ini at master · ZLMediaKit/ZLMediaKit · GitHub"
[2]: https://github.com/zlmediakit/ZLMediaKit?utm_source=chatgpt.com "一个基于C++11的高性能运营级流媒体服务框架"
[3]: https://ffmpeg.org/ffmpeg-all.html "      ffmpeg Documentation
"
[4]: https://github.com/FFmpeg/FFmpeg/blob/master/libavformat/flvenc.c "FFmpeg/libavformat/flvenc.c at master · FFmpeg/FFmpeg · GitHub"
[5]: https://github.com/FFmpeg/FFmpeg/blob/master/libavformat/rtpenc.c "FFmpeg/libavformat/rtpenc.c at master · FFmpeg/FFmpeg · GitHub"
[6]: https://github.com/FFmpeg/FFmpeg/blob/master/libavformat/sdp.c?utm_source=chatgpt.com "FFmpeg/libavformat/sdp.c at master"
