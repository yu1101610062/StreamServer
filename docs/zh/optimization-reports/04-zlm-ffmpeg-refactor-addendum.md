# stream_ingest / ZLM / FFmpeg 重构方案 v4

更新日期：2026-04-15

本版相对 v3 的关键修正：

1. 不再把 `VP8/VP9/AV1/MP3` 一律排除在 ZLM 直代边界之外。
2. 直接以 **最新 master 线 ZLMediaKit** 作为唯一支持基线，不保留旧版兼容逻辑。
3. 明确把 **managed FFmpeg -> ZLM 内部推入协议** 从 RTMP/FLV 改为 **RTSP/TCP** 作为默认且唯一主路径。
4. `RTMP/HTTP-FLV` 相关逻辑统一按“普通 RTMP”与“enhanced-RTMP”区分处理。

---

## 一、支持基线（不保留兼容代码）

### 1.1 ZLM 基线
所有 agent 绑定的 ZLMediaKit 节点必须满足：

- 采用包含 **2025-10** 之后 VP8/VP9/AV1/Opus/Enhanced-RTMP 支持补强的 master 线版本
- `conf/config.ini` 中 `rtmp.enhanced=1`

不再对旧版 ZLM、旧 codec 边界、旧 RTMP 扩展模式保留兼容分支。

### 1.2 内部设计基线

- stream_ingest 的录制仍然统一走 ZLM `startRecord`
- managed FFmpeg 只做以下事情：
  - 非 ZLM 原生入口的接入补位
  - 白名单外轨道的必要转码
  - 坏流/脏流/需要修饰时间戳时的修复性接入
- managed 路径内部推入 ZLM 时，统一使用 **RTSP/TCP ANNOUNCE**
- 删除旧的内部 `RTMP/FLV` 推入主路径

---

## 二、编码白名单与“最小转码”规则

### 2.1 copy 白名单

视频：
- H.264
- H.265 / HEVC
- VP8
- VP9
- AV1

音频：
- AAC
- G711（PCMA / PCMU）
- Opus
- MP3

### 2.2 规则定义

白名单是 **按轨道(track-wise)** 生效，而不是“整任务全 copy / 全转码”。

因此规则改成：

- 视频在白名单内 -> 视频允许 copy
- 视频不在白名单内 -> 仅视频转码
- 音频在白名单内 -> 音频允许 copy
- 音频不在白名单内 -> 仅音频转码

默认转码回落：

- 视频 -> H.264
- 音频 -> AAC

如果任务进入转码路径且任务被标签调度到 GPU 节点，则：

- 视频转码走 GPU encoder
- 音频仍走 CPU
- 不需要转码的轨道保持 copy

---

## 三、重新定义 ZLM native ingest 边界

### 3.1 不能再继续使用的旧判断
旧判断：

- `VP8/VP9/AV1/MP3` => 一律不能按 ZLM 直代规划

该判断删除。

### 3.2 新判断
是否优先走 ZLM native ingest，不再只看旧 API 文档中的 codec 说明，而是按 **当前 master 能力基线 + 入口类型** 判定。

#### A. addStreamProxy 类入口
适用于：
- RTSP
- RTMP
- HLS
- HTTP-TS
- HTTP-FLV

新规则：

- 不再因为 `VP8/VP9/AV1/MP3` 就强制降级到 managed FFmpeg
- 只要 source 协议本身在 ZLM 原生入口覆盖范围内，且任务不需要 FFmpeg 修复/转码，就优先尝试 ZLM native ingest
- `RTMP/HTTP-FLV` 必须额外区分 codec 是否需要 enhanced-RTMP

#### B. RTP / GB28181 类入口
适用于：
- ES / PS / TS RTP
- GB28181 RTP

新规则：

- 仍然优先走 ZLM RTP server / RTP ingest 路径
- 不再因为 `VP8/VP9/AV1/MP3` 先验否决 ZLM 原生 RTP 接入
- 但如果源流存在明显抖动、PTS/丢帧/坏包问题，可升级为 managed FFmpeg 修复后再推入 ZLM

#### C. MP4 文件入口
适用于：
- file-to-live
- loop VOD pseudo-live

新规则：

- 以 `loadMP4File` / ZLM native MP4 live 化能力作为首选
- 不需要 FFmpeg 时不要起 managed FFmpeg
- 若任务要求剪前 N 秒、修坏索引、输入文件本身损坏，则再退回 FFmpeg

#### D. fMP4 外部源入口
本版不把“外部 fMP4 源直接接入”提升为默认 native-first。

原因：

- 已确认 ZLM 对 fMP4 的输出/播放支持很强
- 但公开 API / 文档里，未看到与 `addStreamProxy` 对等、明确面向“外部 fMP4 source ingest”的稳定入口说明

因此：

- **外部 fMP4 source** 先保持 managed FFmpeg ingress
- 不做旧版兼容代码
- 也不在本轮把它强行提升成默认 ZLM native ingest

---

## 四、内部推入协议改造：RTMP/FLV -> RTSP/TCP

### 4.1 本轮最终决策

managed FFmpeg 需要把流推回本机 ZLM 时：

- **默认且唯一主路径：RTSP/TCP**
- 删除旧的 RTMP/FLV 主推入路径
- 不再围绕 internal RTMP 做 codec 规划

### 4.2 为什么改 RTSP 更合适

原因不是“RTSP 一定更强”，而是对本项目目标更合适：

1. **codec 包络更自然**
   - 在当前 ZLM README 口径下，RTSP 对 `H264/H265/AAC/G711/OPUS/MJPEG/MP3/VP8/VP9/AV1/MP2` 支持是直接平铺的
   - RTMP 则天然分成“普通 RTMP”与“enhanced-RTMP”两层

2. **更符合白名单 copy 策略**
   - 对 `VP8/VP9/AV1/Opus/MP3` 等，走 RTSP 内部推入不需要再额外背负 enhanced-RTMP 条件分支

3. **更符合 ZLM-first**
   - 把流原样交给 ZLM 后，后续 RTMP/HLS/TS/fMP4/MP4/WebRTC 暴露和录制都交由 ZLM 处理
   - FFmpeg 只负责接入或必要修复

4. **能直接缓解“HEVC 在线 + MP4 录制 + 不转码”问题**
   - 当前难点很大程度来自内部先推 `flv+rtmp`
   - 改成 RTSP 推入后，HEVC / VP8 / VP9 / AV1 这类流进入 ZLM 的路径更顺，随后录制交给 ZLM `startRecord`

### 4.3 新的 internal publish 规则

managed FFmpeg -> local ZLM：

- URL 统一改为 `rtsp://127.0.0.1:<rtsp_port>/<app>/<stream>`
- 默认使用 `-f rtsp -rtsp_transport tcp`
- 启动探活、媒体上线检测、录制启动全部基于新的 internal publish schema 重写
- 运行时状态判断改成 **schema-agnostic** 或显式使用 `rtsp`，不再写死 `rtmp`

### 4.4 是否保留 internal RTMP fallback

本轮不保留兼容路径。

也就是说：

- **不再保留 internal RTMP 作为并行后备主链路**
- 如果节点环境连基本的 FFmpeg RTSP ANNOUNCE -> ZLM RTSP publish 都不成立，agent 启动直接 fail-fast

---

## 五、RTMP / HTTP-FLV 重新分层处理

### 5.1 结论
`RTMP/HTTP-FLV` 不能再按“只要白名单 codec 就一定端到端 copy”理解。

必须拆成两层：

#### 1）ingest 侧能不能原样接入
#### 2）expose / consume 侧能不能原样消费

### 5.2 规划规则

#### 普通 RTMP 可视为“安全基线”的组合
优先按最保守口径处理：

- H.264
- AAC / G711 / MP3

#### 需要 RTMP 扩展或 enhanced-RTMP 的组合
按“受限可 copy”处理：

- H.265
- Opus
- VP8
- VP9
- AV1

新规则：

- 若任务要求对外暴露 RTMP/HTTP-FLV，且 codec 命中上述受限组合：
  - 仅当节点 ZLM 已启用 `rtmp.enhanced=1`，且业务明确接受 enhanced-RTMP 路线时，允许保留 copy
  - 否则转码到普通 RTMP 安全基线（默认 H264 + AAC）

### 5.3 结果
这样就不会再出现：

- 入口 codec 在白名单里，于是 planner 盲目 copy
- 但实际对外 RTMP/FLV 消费方并不支持
- 最后功能退化或播放器黑屏

---

## 六、stream_ingest 的新 planner 结构

### 6.1 决策顺序

#### 第一步：识别任务语义
- continuous
- bounded

#### 第二步：识别输入入口是否属于 ZLM native ingest
- addStreamProxy
- openRtpServer / RTP ingest
- loadMP4File
- managed FFmpeg ingress

#### 第三步：逐轨判定是否可 copy
- video_copy_allowed
- audio_copy_allowed

#### 第四步：判定暴露协议是否接受当前 codec
- rtsp_ok
- rtmp_ok
- hls_ok
- ts_ok
- fmp4_ok
- mp4_ok
- webrtc_ok

#### 第五步：做最小变更
优先级：

1. ZLM native ingest + 全 copy
2. managed FFmpeg ingress + 全 copy + RTSP internal publish
3. managed FFmpeg ingress + 单轨转码 + RTSP internal publish
4. managed FFmpeg ingress + 双轨转码 + RTSP internal publish

### 6.2 不再允许的旧行为

- realtime 路径默认 `ForceH264`
- 因为内部推入是 FLV/RTMP 所以先天压缩到 H264 轨道
- `VP8/VP9/AV1/MP3` 先验排除 ZLM 路线

---

## 七、录制方案同步调整

### 7.1 原则不变

- 录制优先交给 ZLM `startRecord`
- 删除 mp4 companion FFmpeg sidecar

### 7.2 新前提
由于 managed ingress 改成 RTSP/TCP 推入 ZLM，且 codec 白名单扩大：

- HEVC / VP8 / VP9 / AV1 / MP3 / Opus 白名单流，只要 ZLM 对该流 + 目标录制格式支持，就不再为了录制而附加转码
- 对外在线流与录制都尽量使用同一份原始码流

### 7.3 何时仍然转码
仅当：

- 某轨不在白名单
- 或录制 / 暴露目标协议不接受该 codec 组合
- 或业务显式给出 bitrate/fps/gop 等需要重编码的参数

---

## 八、落地改动（代码层）

### 8.1 media-agent/runtime
重构重点：

- 删除 internal RTMP target builder
- 新增 internal RTSP target builder
- managed ingest 命令行统一输出 `-f rtsp -rtsp_transport tcp`
- startup probe / online check 按 RTSP internal publish 改写
- continuous lifecycle / 0 退出恢复 / wall-clock duration 方案保持 v3 结论
- record 仍只走 ZLM `startRecord`

### 8.2 media-agent/capability
删除 per-task 动态 codec/GPU 探测；保留 node-level 静态快照，仅用于：

- 节点自检
- 启动 fail-fast
- 注册时上报环境画像

启动必须校验：

- ZLM 基线版本/能力符合要求
- `rtmp.enhanced=1`
- internal RTSP publish 基本可用

### 8.3 media-core/control_plane
planner 更新：

- 新增 protocol+codec matrix 规划逻辑
- 将“白名单 copy”改成“逐轨 + 暴露协议约束”判定
- GPU 节点仍只由 labels 选择，不做自动偏向

---

## 九、对“切换到 RTSP 推入是不是更容易”的明确回答

答案：**是，对你这次目标来说更容易，而且应该直接改。**

更准确地说：

- 如果目标是“默认 copy、白名单扩大、能交 ZLM 就交 ZLM、不要再被 internal FLV/RTMP 卡住”，
- 那么 managed FFmpeg -> ZLM 改成 **RTSP/TCP 内部推入**，会比继续坚持 internal RTMP/FLV 更干净。

不是因为 RTSP 在所有场景都绝对优于 RTMP，
而是因为：

- 你现在要承接的 codec 集合已经明显超过普通 FLV/RTMP 的舒适区
- ZLM 当前 master 对 RTSP / RTP / MP4 / HLS / TS / fMP4 的 codec 包络更友好
- RTMP 这条链路天然带着 enhanced-RTMP/播放器兼容性的额外矩阵

因此本轮方案里，internal publish 直接切 RTSP/TCP，是最符合整体目标的做法。

