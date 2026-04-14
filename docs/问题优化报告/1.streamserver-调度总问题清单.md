# StreamServer 调度总问题清单（2026-04-14）

> 结论基于代码静态审查。未在当前环境执行 `cargo test` / 联调复现。

## 总结

当前项目的调度问题不是单点 bug，而是一组**状态机缺少栅栏（fencing）+ 断连恢复策略过于激进 + 子节点幂等不足**的组合问题。它们叠加后，会出现：

- 任务之间互相影响
- 任务卡在“开始中 / 结束中”
- 任务在多个状态之间来回跳
- 停止后又被重新拉起
- 节点超时后中心重发，旧节点晚到上报把新状态覆盖
- 同一任务在不同节点/不同 runtime 同时存在

---

## P0：会直接导致重发 / 重跑 / 状态横跳 / 分裂脑（split-brain）

### 1）停止失败被当成启动拒绝，任务会被重新排队

**链路**

- core 在 stop API 中先把任务切到 `STOPPING`，再发 stop 命令：
  - `crates/media-core/src/main.rs:835-842`
  - `crates/media-core/src/control_plane.rs:245-288`
- agent 侧 stop 失败发的是通用 `rejected`：
  - `crates/media-agent/src/control_plane.rs:417-460`
- core 收到 `rejected` 后，直接把任务改回 `QUEUED`：
  - `crates/media-core/src/repository.rs:2915-2950`
- scheduler 每 5 秒重新派发 `QUEUED` 任务：
  - `crates/media-core/src/repository.rs:1469-1496`
  - `crates/media-core/src/scheduler.rs:16,45-53`

**现象**

`RUNNING -> STOPPING -> QUEUED -> STARTING/RUNNING`

这正是“结束中又回到开始中 / 停止后又起来了”。

---

### 2）旧事件、旧节点、旧 attempt 没有被挡住，迟到上报可以覆盖当前状态

**链路**

`record_agent_task_event()` 对 `accepted / starting / recovering / running / stopping / rejected / terminal` 的任务更新，基本都是按 `task_id` 直接改 `tasks`：

- `crates/media-core/src/repository.rs:2793-2999`
- `complete_task_attempt()`：`crates/media-core/src/repository.rs:4276-4335`
- `promote_task_running()`：`crates/media-core/src/repository.rs:4338-4384`

没有校验：

- 这个事件的 `attempt_no` 是否仍然是当前 `current_attempt_no`
- 这个事件的 `node_id` 是否仍然是当前 `assigned_node_id`
- 这个事件是否匹配有效 lease

**现象**

- 旧节点晚到一个 `running`，能把新 attempt 覆盖回 `RUNNING`
- 旧节点晚到一个 `failed/succeeded/canceled`，能把当前任务直接盖成终态
- 旧节点晚到一个 `starting`，能把任务重新写回 `STARTING`

这是“状态横跳”的核心根因。

---

### 3）同一 attempt 会被复用，放大迟到上报问题

**链路**

- `prepare_task_dispatch()` 在 `current_attempt_no > 0` 时直接复用旧 attempt：
  - `crates/media-core/src/repository.rs:2355-2520`
  - 关键点：`2396-2400`
- dispatch rollback 只是把任务改回 `QUEUED`：
  - `crates/media-core/src/repository.rs:2522-2590`
- agent `rejected` 后也只是回到 `QUEUED`：
  - `crates/media-core/src/repository.rs:2915-2950`

**现象**

一个 attempt 会经历多次：

- 重派发
- 节点切换
- 重下发
- stop/restart

于是旧消息和新消息共用同一个 `attempt_no`，更难判断谁是当前有效消息。

---

### 4）子节点超时 / session 断开后，中心确实会重发；旧节点之后再上报，会形成 split-brain

**链路**

1. core 对控制面流 30 秒无消息就断开：
   - `crates/media-core/src/control_plane.rs:41,372-403`
2. 断开后调用 `recover_tasks_for_disconnected_node()`：
   - `crates/media-core/src/control_plane.rs:534-563`
   - `crates/media-core/src/repository.rs:1872-2016`
3. 对 `STARTING / RUNNING / RECOVERING` 任务，先标成 `LOST`，如果配置允许则自动 `enqueue_retry()`：
   - `crates/media-core/src/repository.rs:1958-2014`
   - `crates/media-core/src/repository.rs:1776-1869`
   - `crates/media-core/src/repository.rs:4387-4447`
4. agent 自己会持续重连：
   - `crates/media-agent/src/control_plane.rs:89-105`
5. 新连接建好后，core 会立刻下发一个**空过滤条件**的 `AdoptOrphans`：
   - `crates/media-core/src/control_plane.rs:346-356`
6. agent 对空过滤条件会尝试接管/恢复**所有**本地 persisted runtime：
   - `crates/media-agent/src/control_plane.rs:319-346`
   - `crates/media-agent/src/runtime.rs:138-145,627-877`
7. 对恢复出来的 runtime，agent 还可能基于旧持久化状态重新启动，而且复用旧 `attempt_no`、旧 `lease_token`：
   - `crates/media-agent/src/runtime.rs:788-793,867-872`
   - `crates/media-agent/src/runtime.rs:4170-4207`
8. core 收到这些晚到的 `running/failed/canceled` 时，没有 fencing 校验，仍然会收下：
   - `crates/media-core/src/repository.rs:2793-2999,4276-4384`

**结论**

你提到的“子节点超时后再上报，控制中心会重新发任务吗”——**会**。而且更严重：

- 中心已经把任务标 LOST 并可能重试到别的节点
- 旧节点 reconnect 后又把旧 runtime 接回来，甚至重新拉起
- 旧 runtime 的晚到事件会覆盖新 attempt

这就是标准的**分裂脑**问题。

---

### 5）agent 的 `StartTask` 不是幂等的，重复下发可能真的起双份

**链路**

- runtime registry 按 `runtime_id` 记，不按 `(task_id, attempt_no)` 唯一：
  - `crates/media-agent/src/runtime.rs:45-104`
- `start_task()` 不检查该 task/attempt 是否已存在，只检查 lease 非空和 slot：
  - `crates/media-agent/src/runtime.rs:463-487`
- stop 时只从 `HashMap.values()` 找第一个匹配 runtime：
  - `crates/media-agent/src/runtime.rs:83-89,489-506`

**现象**

- 同一任务同一 attempt 可能真的起多份 runtime
- stop 只停掉其中一个
- 另一个仍在跑并继续上报状态

这会表现成“停不干净 / 又自己回到 running / 一个任务影响另一个 stop 结果”。

---

### 6）stop/start 在 agent 侧是异步竞态，stop 可能先失败，随后 start 又成功

**链路**

- `handle_start_task()` 只是 `tokio::spawn` 一个异步启动任务，然后立即返回：
  - `crates/media-agent/src/control_plane.rs:349-414`
- 真正的 runtime 创建发生在后台；如果此时 stop 已到达：
  - `handle_stop_task()` 立即执行 `executor.stop_task()`：`417-460`
  - 如果 runtime 还没注册进去，会返回 `RuntimeNotFound`
  - agent 把它发成通用 `rejected`
- 之后后台 start 仍可能继续成功，发 `accepted/starting/snapshot`

**现象**

- 用户点“停止”，core 收到 `rejected` 以为停失败并回队列
- 但后台 start 还会把任务真正拉起来

这会直接制造“停止失败 + 又启动 + 状态来回跳”。

---

### 7）控制流断了，异步启动任务不会取消；中心可能已经重试，旧节点本地却还在继续启动

**链路**

- `handle_start_task()` 启动后脱离 session 生命周期：`crates/media-agent/src/control_plane.rs:349-414`
- 这些后台任务使用的是旧 sender，发送结果大多被忽略：
  - `365-409` 中大量 `let _ = ... .await`
- 一旦控制流断线，core 会按“节点掉线”处理并可能自动重试：
  - `crates/media-core/src/control_plane.rs:372-403,534-563`
  - `crates/media-core/src/repository.rs:1872-2016`

**现象**

- core 以为旧节点失联，已经把任务重发到新节点
- 旧节点本地异步启动仍在继续
- reconnect 后旧 runtime 被 adopt 回来

这也是 split-brain 的一条路径。

---

### 8）lease 机制没有真正形成 fencing，基本处于“写了但不用”的状态

**链路**

- 派发时会写 `task_leases`，60 秒过期：
  - `crates/media-core/src/repository.rs:2456-2475`
- `StartTask` 里确实携带了 `lease_token`：
  - `proto/control_plane.proto:124-132`
  - `crates/media-core/src/control_plane.rs:198-208`
- 但 agent 上报的 `TaskEvent / TaskProgress / TaskSnapshot` 协议里没有 `lease_token` 字段：
  - `proto/control_plane.proto:84-122`
- core 落库时也没有按 lease 做比对；代码里几乎只有 insert/delete，没有 read/validate：
  - `crates/media-core/src/repository.rs:2456-2475,4164-4173`

**结论**

当前 lease 只能证明“派发时生成过一个 token”，不能阻止旧节点、旧 attempt、迟到事件污染当前状态。

---

## P1：高概率造成“互相影响 / 卡状态 / 错归因 / 错恢复”

### 9）`STOPPING` 不参与断连恢复，任务会卡在“结束中”

**链路**

- 节点断连恢复只处理：
  - `DISPATCHING / STARTING / RUNNING / RECOVERING`
  - `crates/media-core/src/repository.rs:1878-1883`
- 不处理 `STOPPING`

**现象**

stop 命令发出后，如果：

- 消息没送达
- 送到一半节点掉线
- session 因超时/异常被 close

任务就可能永久停在 `STOPPING`。

---

### 10）Adopt-orphans 是“全量接管”，不是“中心授权接管”

**链路**

- core 建链后直接发送空条件 `AdoptOrphans`：
  - `crates/media-core/src/control_plane.rs:346-356`
- agent 端空 filter 会匹配全部：
  - `crates/media-agent/src/runtime.rs:138-145`
- `adopt_orphans()` 会扫 registry + persisted runtimes，把所有符合条件的都接回来：
  - `crates/media-agent/src/runtime.rs:627-877`

**问题本质**

这意味着“是否接回旧任务”是 agent 自己决定的，不是中心基于当前任务真相决定的。

**后果**

- 中心已经判 LOST/重试/终态的任务，旧节点还能自己接回
- 旧 runtime 和新 attempt 可同时存在

---

### 11）重连时如果旧 session 还没超时清理，新 session 会被拒绝，形成假死窗口

**链路**

- `bootstrap_session()` 如果同 `node_id` 已存在，会直接 `already_exists`：
  - `crates/media-core/src/control_plane.rs:291-304`
- core 只有在 stream timeout / stream end / payload error 时才 close 老 session：
  - `crates/media-core/src/control_plane.rs:372-403,534-563`

**现象**

- 子节点其实已经恢复，但 30 秒内还连不上
- 这段时间中心会继续把旧 session 当成在线候选
- 如果又派任务过去，send 失败后才 rollback + close
  - `crates/media-core/src/control_plane.rs:212-224`

**后果**

会出现一段“看似在线、实际不可用”的死区，造成额外的 dispatch 失败和误恢复。

---

### 12）单条坏消息能把整个节点会话打断，连带所有任务一起恢复/重试

**链路**

- `process_stream()` 只要某条 payload 处理失败就 `break`：
  - `crates/media-core/src/control_plane.rs:396-399`
- 然后统一 `close_session()`：
  - `crates/media-core/src/control_plane.rs:402,534-563`

**后果**

- 某个任务的一条坏事件
- 某次数据库写失败
- 某条异常 snapshot

都会把整个节点上的任务一起拖入“断连恢复”。

这是“一个任务影响其他任务”的系统级放大器。

---

### 13）ZLM hook 归因只看流名，不看节点/服务端，任务之间会串线

**链路**

- `record_zlm_stream_event_hook()` 归因时只按 `vhost/app/stream` 查绑定：
  - `crates/media-core/src/repository.rs:3489-3533`
  - `find_stream_binding_for_hook()`：`3966-3999`
- `stream_bindings` 唯一键只有 `(schema, vhost, app, stream)`：
  - `migrations/0001_init.sql:147-160`
- 上游冲突时会直接覆盖 task/attempt：
  - `crates/media-core/src/repository.rs:3171-3194,3571-3592`
- `find_task_for_publish_stream()` 同节点多候选时返回第一条：
  - `crates/media-core/src/repository.rs:3665-3712`

**后果**

- A 任务的 hook 记到 B 任务
- 旧任务/新任务同流名会互相污染
- 录制、推流、无人观看关闭、publish 归因都可能串任务

---

### 14）snapshot 不负责主状态收敛，terminal event 丢了就会卡状态

**链路**

- core 收到 `TaskSnapshot(exited)` 只释放 reservation：
  - `crates/media-core/src/control_plane.rs:509-518`
- `record_agent_snapshot()` 只写事件、pid、binding、artifact，不推进主任务状态：
  - `crates/media-core/src/repository.rs:3077-3126`

**后果**

如果 terminal event 丢了，但 exited snapshot 已到了，任务仍可能停在：

- `STARTING`
- `RUNNING`
- `STOPPING`

---

### 15）adopted/orphaned 事件在 core 侧没有状态语义

**链路**

- agent adopt 时会发 `adopted` 事件：
  - `crates/media-agent/src/runtime.rs:755-772,836-851`
- core 的 `record_agent_task_event()` 并没有处理 `adopted/orphaned` 分支：
  - `crates/media-core/src/repository.rs:2817-2995`

**后果**

旧 runtime 已被节点重新接回，但 core 不会把任务推进到 `RECOVERING/RUNNING`，只能等后续 progress/running/hook 来“补状态”。

于是会出现：

- 实际上任务已经在跑
- 平台上仍显示 `LOST/QUEUED/STARTING`
- 随后又被调度器重发

---

### 16）RTP runtime 消失时，若 hook 没到，任务可能一直挂着

**链路**

- RTP monitor 发现 server 从 ZLM 消失时，只发 `rtp_server_closed` 事件 + exited snapshot：
  - `crates/media-agent/src/runtime.rs:5490-5530`
- core 对 `rtp_server_closed` 没有专门状态处理：
  - `crates/media-core/src/repository.rs:2817-2995`
- 真正能把任务打成 lost 的，是 `on_rtp_server_timeout` hook：
  - `crates/media-core/src/main.rs:1707-1747`

**后果**

如果 hook 没到、晚到、或匹配失败，任务主状态可能持续停在运行态。

---

### 17）手工状态迁移 `transition_task()` 没有 CAS / 行级状态保护，和异步事件存在竞争

**链路**

- 先 `fetch_task_summary()`，后 `update tasks set status=... where id=$4`：
  - `crates/media-core/src/repository.rs:1710-1763`
- 没有 `for update`
- 没有 `where status = old_status`

**后果**

用户操作与 agent 事件并发时，可能出现：

- 用户刚 stop，agent 又把它打回 running
- 任务已终态，用户操作基于旧读又写成其他中间态

---

## P2：容量、可观测性、排障复杂度问题（会放大主问题）

### 18）dispatch reservation 的释放条件不完整，容量统计依赖 heartbeat 的 `running_tasks`

**链路**

- reservation 只在 `rejected/succeeded/failed/canceled` 释放：
  - `crates/media-core/src/control_plane.rs:1200-1202`
- running 不直接释放，而是等 heartbeat 的 `running_tasks` 增量：
  - `crates/media-core/src/control_plane.rs:598-605`
- 但 agent heartbeat 的 `running_tasks = runtime_registry.count()`：
  - `crates/media-agent/src/control_plane.rs:173-185`

**后果**

`running_tasks` 其实包含：

- starting
- stopping
- orphaned
- zombie/未清理 runtime

这会造成调度容量判断偏差。

---

### 19）数据库中的节点健康和控制面在线状态可能互相矛盾

**链路**

- control-plane close 时把节点标 unhealthy：
  - `crates/media-core/src/control_plane.rs:555-563`
- ZLM hook `on_server_keepalive/on_server_started` 又会把节点标 healthy：
  - `crates/media-core/src/main.rs:1878-1887`

**后果**

UI/数据库可能显示节点 healthy，但 control-plane session 实际断着。虽然调度主要用 sessions map，不直接用 DB health，但会严重误导排障。

---

### 20）terminal/lost 后没有清空 `assigned_node_id/current_attempt_no`，会留下旧所有权

**链路**

- `complete_task_attempt()` 只改 status/finished_at：
  - `crates/media-core/src/repository.rs:4276-4335`
- `mark_task_lost()` 也不清 `assigned_node_id/current_attempt_no`：
  - `crates/media-core/src/repository.rs:4387-4447`

**后果**

不是最直接的调度 bug，但会导致：

- stop 找错节点的风险更高
- 诊断时看上去像还归属旧节点
- 为后续错误恢复制造歧义

---

### 21）ManagedProcess 被标成 `ZlmProxy` worker_kind，污染运行时分类

**链路**

- `start_process_task()` 里：
  - `crates/media-agent/src/runtime.rs:921-926`

**后果**

会污染：

- snapshot 语义
- adopt filter 行为
- 排障观察

虽然不是最致命调度 bug，但会加重错误恢复和定位难度。

---

## 最关键的真实故障链路（建议优先按这个排）

### 链路 A：节点超时 -> 中心重试 -> 旧节点回魂 -> 状态横跳

1. 任务在节点 A 运行
2. 控制流 30 秒无消息，core 关闭 session
3. 任务被标 `LOST`，如果允许恢复则自动 retry 到节点 B
4. 节点 A 恢复连接
5. core 给 A 发空过滤 `AdoptOrphans`
6. A 把旧 runtime 接回，甚至重启旧 runtime
7. A 晚到的 `running/failed/canceled` 继续上报
8. core 没有 attempt/node/lease fencing，把旧事件当真
9. 当前任务出现：`LOST -> QUEUED -> DISPATCHING -> RUNNING -> FAILED -> RUNNING` 之类横跳

### 链路 B：stop 与 async start 竞态 -> stop rejected -> scheduler 重发

1. core 下发 start
2. agent 只把 start 放后台队列
3. 用户很快发 stop
4. agent 此时还没创建 runtime，stop 命中 `RuntimeNotFound`
5. agent 发 `rejected`
6. core 把任务改回 `QUEUED`
7. scheduler 再次派发
8. 后台 start 这时又可能成功
9. 最终同一任务可能双份运行，或状态连续横跳

### 链路 C：同流名 hook 串线 -> 任务互相影响

1. 两个任务使用相同/冲突的 `(schema,vhost,app,stream)`
2. `stream_bindings` upsert 覆盖旧绑定
3. ZLM hook 到来时只按流名取最新绑定
4. A 的 hook 记到 B
5. B 的状态/录制/停止逻辑被 A 的流事件驱动

---

## 优先修复顺序

### 第一批：先止血

1. **拆分 start_rejected 和 stop_rejected**，stop 失败绝不能回 `QUEUED`
2. **所有 agent 事件入库前强制校验**：`task_id + attempt_no + node_id + lease_token`
3. **节点断连恢复纳入 STOPPING**
4. **agent 对 `(task_id, attempt_no)` 启动做幂等保护**
5. **禁止空过滤全量 adopt-orphans**，改成中心明确授权 reclaim

### 第二批：修 split-brain

6. **每次重派发都必须新 attempt_no**，不要复用旧 attempt
7. **reconnect / adopt / replay / progress / snapshot 全链路带 lease_token**
8. **core 只接受“当前 attempt + 当前 node + 当前 lease”的事件**
9. **session duplicate node_id 改为挤掉旧 session，而不是拒绝新连接**

### 第三批：修隔离和收敛

10. **stream_bindings 和 hook 归因加入 server_id/node_id**
11. **snapshot 承担兜底收敛主状态**
12. **RTP 关闭不依赖外部 hook 才能进入终态/LOST**
13. **transition_task 改成 CAS / `select ... for update`**

---

## 最少要补的回归测试

1. stop rejected 不能 requeue
2. 旧节点晚到 running 不能覆盖新 attempt
3. 节点 timeout + auto retry + reconnect adopt 不会产生双活
4. 重复 StartTask 不会起双份 runtime
5. stop 在 async start 完成前到达，不会导致状态回 QUEUED
6. 同流名 hook 不会串任务
7. exited snapshot 丢 terminal event 时也能收敛到正确终态

