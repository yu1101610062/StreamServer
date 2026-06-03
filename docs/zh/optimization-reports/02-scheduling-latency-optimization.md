## 1. `QUEUED -> DISPATCHING` 这一段，确实有调度侧的硬延迟

先说调度器本身。

`scheduler` 是固定 **5 秒** 扫一次，而且是**串行** dispatch 的：`crates/media-core/src/scheduler.rs:16, 45-53, 104-112`。
对自动重试、节点恢复后重派发、之前没抢到节点而留在 `QUEUED` 的任务，这就是一个很直白的硬下限。

而且 `list_due_at_tasks()` 虽然表上有 `status, priority desc, created_at asc` 的索引，实际查询却是按 **`created_at asc`** 拿任务，不按 `priority` 出队：`migrations/0001_init.sql:303-304`，`crates/media-core/src/repository.rs:1469-1492`。
这意味着一批任务堆积时，高优先级任务也可能被早来的低优先级任务拖住。

再往下看，core 侧对节点容量的感知也有滞后：

* agent 心跳是 **10 秒一次**：`crates/media-agent/src/control_plane.rs:43, 136-163`
* core 的 dispatch reservation 只在 **heartbeat 的 running_tasks 增加**时才弹掉，或者在 `rejected/succeeded/failed/canceled` 时释放：`crates/media-core/src/control_plane.rs:589-617, 1200-1202`
* 但 `accepted / starting / running` 本身**不会立刻释放 reservation**

所以同一节点一批任务连发时，前面的任务即使已经在 agent 本地接住了，core 侧的容量视图也可能要再等**一个心跳周期**才彻底跟上。这个会直接造成“后面的任务明明节点已经在干活了，但平台还觉得这个节点没腾出来”的体感。

这里还有个细节：**手工点击 start** 或者创建 immediate 任务，接口会直接调 `dispatch_task()`，不一定经过 5 秒轮询：`crates/media-core/src/main.rs:803-823, 1928-1941`。
所以如果你说的是“我手工点开始后，状态还很久不动”，那更大概率不是 scheduler，而是下面两段。

---

## 2. `DISPATCHING -> STARTING` 这一段，agent 侧有明显的启动前阻塞

这个阶段我觉得是你们现在最容易低估的。

agent 端所有 `StartTask` 共用一个全局启动闸门，只有 **4 个并发 permit**：`crates/media-agent/src/control_plane.rs:45-47, 57-58, 357-364`。
也就是说，同一个节点上只要前面有 4 个启动比较慢的任务，后面的任务连“我已经接单了”都不会立刻回。

更关键的是：agent 不是一收到 `StartTask` 就先回 `accepted`，而是要等 `executor.start_task(&request)` **整段执行完**之后，才发 `accepted` 和 `starting`：`crates/media-agent/src/control_plane.rs:364-392`。
而 `executor.start_task()` 里面并不轻：

### 对 managed ffmpeg 任务

`copy_or_transcode` 和 `passthrough` 路径会先同步跑 `ffprobe` 预探测输入，默认超时 **7000 ms**：`crates/media-agent/src/runtime.rs:321, 2884-2896, 2897-2947, 3335-3456`。
这意味着一个任务可能在 agent 本地“闷头预检”好几秒，core 还停留在 `DISPATCHING`。

而且这个 `ffprobe` 是同步轮询实现，里面直接 `std::thread::sleep(...)`，不是 `spawn_blocking`：`crates/media-agent/src/runtime.rs:3427-3456`。
所以它不只是慢，还会占着 Tokio worker 线程跑。

### 对 ZLM proxy / RTP server 任务

agent 会先同步调用 ZLM API：

* `addStreamProxy`：`crates/media-agent/src/runtime.rs:1407-1415`
* `openRtpServer`：`crates/media-agent/src/runtime.rs:1503-1506`

这个 HTTP client 本身超时是 **3 秒**：`crates/media-agent/src/runtime.rs:455-458`。
所以 ZLM 稍慢一点，`accepted` 也会跟着晚。

这就解释了一个很常见的现象：
**平台看起来像“控制中心下发慢”，其实是 agent 侧在 permit 排队、ffprobe、调 ZLM API，状态还没来得及往回报。**

这里有个很实用的结论：

* 对 **file_transcode / bridge** 这类没有 `startup_probe` 的任务，只要 child 真起起来了，代码会立刻推进到 `running`：`crates/media-agent/src/runtime.rs:1099-1132`
* 所以这类任务如果也慢，主因通常就是：**4 并发闸门 + 启动前预检**

还有一个配置层面的现成优化口子：
`force_transcode` 会直接走强制转码路径，不先用 `ffprobe` 做 copy/transcode 决策；但 `copy_or_transcode` 和 `passthrough` 都会探测输入：`crates/media-agent/src/runtime.rs:2884-2896, 2897-2947`。
也就是说，**如果业务能接受固定转码策略，`force_transcode` 本身就会比自动探测更快起。**

---

## 3. `STARTING -> RUNNING` 这一段，很多任务是“定义上就故意慢”

这个阶段不是单纯“状态没更新”，而是当前代码把 `RUNNING` 定义成了更严格的条件：

### 流任务不是“进程起来”就算 running

而是要等 **流真的在 ZLM 里 online**。

相关常量：

* startup probe timeout：**30 秒**
* probe poll interval：**1 秒**
  见 `crates/media-agent/src/runtime.rs:315-316`

实际逻辑：

* managed process 有 `startup_probe` 时，会起 `spawn_startup_probe_monitor()`：`crates/media-agent/src/runtime.rs:1099-1110, 4686-4916`
* live relay 走 `spawn_live_relay_monitor()`：`crates/media-agent/src/runtime.rs:1482-1491, 4921-5397`
* rtp receive 走 `spawn_rtp_receive_monitor()`：`crates/media-agent/src/runtime.rs:1575-1584, 5400-5536`

这些 monitor 都是在轮询 ZLM：

* `zlm_stream_online()` 查 `/index/api/getMediaList`：`crates/media-agent/src/runtime.rs:5587-5595`
* rtp 还要先查 `/index/api/listRtpServer`，然后再从 `getMediaList` 找 stream binding：`crates/media-agent/src/runtime.rs:5598-5638, 5432-5487`

所以对这类任务来说，“很久才变 running”有两种完全不同的含义：

1. **真的慢**：流迟迟没在线
2. **其实本地资源早就准备好了，但你们把 running 定义成了 media online 之后才给**

最典型的是 RTP 接收任务。
`openRtpServer` 成功以后，agent 会立刻记一条 `rtp_server_opened` 事件：`crates/media-agent/src/runtime.rs:1557-1574`。
但平台主状态不会因此变 `RUNNING`，因为真正的 `running` 要等到**上游设备真的开始推流，ZLM 里也能看到对应 stream binding**：`crates/media-agent/src/runtime.rs:5432-5487`。
所以如果摄像头/设备晚了 10 秒发第一包，这 10 秒全都会算在 `STARTING` 里。

live relay 也是类似。
`addStreamProxy` 成功后会立即发 `zlm_proxy_created`：`crates/media-agent/src/runtime.rs:1466-1481`，但 core 并不会把这个事件当状态推进，只是记 event；`record_agent_task_event()` 只真正处理 `accepted / starting / running / stopping / rejected / terminal` 这几个：`crates/media-core/src/repository.rs:2817-2995`。
所以任务其实已经“在 ZLM 里建好了代理”，主状态仍然还是 `STARTING`。

还有一个会额外拉长 `STARTING -> RUNNING` 的点：
**录制启动是放在 running 之前做的。**

`startup_probe_monitor()` 和 `live_relay_monitor()` 都是先尝试 `startRecord`，然后才发 `running`：`crates/media-agent/src/runtime.rs:4760-4912, 4957-5008`。
也就是说，只要你开了录制，`RUNNING` 里还额外混进了“录制子功能初始化”的耗时。



## 最值得先做的优化，我按收益排一下

### 第一组：最划算，马上就能降体感时延

1. **把 agent 的 `accepted` 前移**
   不要等 `executor.start_task()` 全做完再回。
   最少也要在拿到 permit 后立刻回一个 `accepted`；更进一步，permit 前就回 `queued_on_agent`。
   这样 `DISPATCHING` 不会因为 ffprobe / ZLM API / sidecar 启动而卡住。

2. **把 `START_TASK_CONCURRENCY_LIMIT` 从写死 4 改成可配置，最好按任务类型拆开**
   现在 4 个慢 ffprobe 任务，就能把同节点后面的轻量 proxy / rtp 任务一起排住：`crates/media-agent/src/control_plane.rs:47, 357-364`。

3. **把 ffprobe 预检移到 blocking 线程池，或者尽量减少预检**
   当前实现是同步阻塞的：`crates/media-agent/src/runtime.rs:3335-3456`。
   至少别让它直接占 Tokio worker。

4. **把 `RUNNING` 和 “media online” 分开**
   新增一个 `READY / LISTENING / PROVISIONED` 一类状态。
   用现成的 `zlm_proxy_created`、`rtp_server_opened` 这些事件推进主状态，不要让任务在 `STARTING` 里一挂十几秒。
   现在这些事件已经有了，只是 core 没拿来驱动状态：`crates/media-agent/src/runtime.rs:1466-1481, 1557-1574`，`crates/media-core/src/repository.rs:2817-2995`。

### 第二组：直接减少排队等待

5. **scheduler 改事件驱动，或者至少把 tick 从 5 秒降到 1 秒**
   对自动重试、无节点后恢复、普通 `QUEUED` 扫描，这个收益很直接：`crates/media-core/src/scheduler.rs:16, 45-53`。

6. **scheduler dispatch 改成有上限的并发，不要串行扫**
   现在 due tasks 是一个一个 `await` 下发的：`crates/media-core/src/scheduler.rs:49-57`。
   任务一多，吞吐会明显差。

7. **出队顺序改成 `priority desc, created_at asc`**
   现在表索引都准备好了，但查询没用上 priority：`migrations/0001_init.sql:303-304`，`crates/media-core/src/repository.rs:1469-1492`。

8. **dispatch reservation 不要等 heartbeat 才释放**
   至少在 `accepted / starting` 时就做 in-memory load 更新，或者直接释放 reservation。
   现在最多会滞后一个 10 秒心跳周期：`crates/media-core/src/control_plane.rs:589-617, 1200-1202`，`crates/media-agent/src/control_plane.rs:43, 136-163`。

### 第三组：不一定改代码，先靠配置就能提速

9. **对能接受固定策略的任务，优先用 `force_transcode`**
   这样可以绕过启动前 `ffprobe` 决策：`crates/media-agent/src/runtime.rs:2942-2947`。

10. **合理下调 `input.probe_timeout_ms`**
    它现在默认会走到 **7000 ms** 的 ffprobe 超时：`crates/media-agent/src/runtime.rs:321, 3419-3424`。
    对稳定输入源，调低会很有效；但调太低会增加误判。

11. **能走 fast record 模式的流录制，尽量走 fast**
    这个模式本身就把 `startup_probe` 关掉了，所以 child 起起来就能更快进 running：`crates/media-agent/src/runtime.rs:1908-1912, 2284-2415, 1099-1132`。


一句话收口：

**有优化空间，而且挺大。**
你们现在“变成 running 慢”，一部分是调度和容量视图滞后，一部分是 agent 启动前同步阻塞，更大的一部分其实是把 `RUNNING` 定义成了“媒体真的在线”之后才算。
从收益上看，最该先动的是：**ACK 前移、启动并发闸门改造、ffprobe 预检下沉、`READY/LISTENING` 状态拆出来、scheduler 从 5 秒轮询改成更实时。**
