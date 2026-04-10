# StreamServer 并发与调度审计报告

- 审计时间：2026-03-31
- 审计范围：`/home/0x7c00/RustroverProjects/StreamServer`
- 重点：多任务并发、调度重试、容量控制、断连一致性
- 结论：**原始审计结论成立；但截至 2026-03-31 夜间，P0/P1 的 1-5 项修复已补齐，当前主要剩 P2 链路校验与更强集成覆盖。**

> 修复进展更新（2026-03-31 夜间）
>
> - P0-1：`QUEUED` 任务统一再派发 —— 已完成
> - P0-2：`DISPATCHING` 发送失败补偿回滚 —— 已完成
> - P1-3：`max_runtime_slots` 硬性并发限制 —— 已完成
> - P1-4：core 侧 reserved slots / inflight dispatch —— 已完成
> - P1-5：节点断连自动回收在途任务 —— 已完成

## 一、总体结论

当前实现的核心问题不是“线程安全崩溃”，而是**任务调度状态机与实时负载之间缺乏闭环**：

1. 任务一旦首次派发失败，可能**永久卡在 `QUEUED` 或 `DISPATCHING`**。
2. 节点容量 `max_runtime_slots` 目前只参与**心跳上报与排序**，**没有形成硬性准入约束**。
3. 派发决策依赖心跳快照，缺少**派发时的临时占位/预留**，容易在短时突发下把一批任务压到同一个节点。
4. 节点断连后只标记节点 unhealthy，**没有自动回收/重排该节点上的在途任务**。

因此，项目当前更像“**尽力而为的派发器**”，而不是“**具备失败补偿的调度器**”。

---

## 二、关键发现

### 发现 1（P0）`QUEUED` 任务缺少统一兜底重试路径，首次派发失败后可能永久滞留

#### 现象
- 创建 immediate 任务时，若没有可用节点，`dispatch_task()` 的 `NoConnectedNode` / `NodeDisconnected` 会被 API 层吞掉。
- 但 `dispatch_task()` 调用前已经通过 `ensure_task_queued()` 把任务从 `VALIDATING` 推进到了 `QUEUED`。
- 后台 scheduler **并不会扫描通用 `QUEUED` 队列**，只处理：
  - `schedule_start_mode = 'at'` 且 `status = VALIDATING` 的任务
  - cron 调度模板
- agent 明确 `rejected` 后，任务也会被重新写回 `QUEUED`，但同样缺少自动再派发机制。

#### 影响
以下场景都会产生“挂队列不再自动恢复”的问题：
- immediate 任务创建时恰好无节点在线
- at 定时任务到点时节点不可用
- 任务被节点拒绝后重新入队

#### 证据
- 创建任务时吞掉无节点错误：
  - `crates/media-core/src/main.rs:225-233`
- `dispatch_task()` 先把任务推进队列再挑节点：
  - `crates/media-core/src/control_plane.rs:111-123`
  - `crates/media-core/src/repository.rs:1466-1505`
- scheduler 只扫 `at`/cron，不扫通用 queued：
  - `crates/media-core/src/scheduler.rs:45-57`
  - `crates/media-core/src/repository.rs:893-908`
  - `crates/media-core/src/repository.rs:911-940`
- agent rejected 后任务回到 `QUEUED`：
  - `crates/media-core/src/repository.rs:2027-2040`

#### 建议
- 增加一个统一的 `dispatch_pending_tasks()`：周期性扫描 `QUEUED` 且 `resolved_spec is not null` 的任务并再派发。
- 节点上线、心跳恢复、agent rejected、session close 后，都应触发一次 queued sweep。
- `at`/cron 只负责“生成待执行任务”，不要承担“唯一重试入口”。

---

### 发现 2（P0）任务在写入 `DISPATCHING` 后才发送到 agent，发送失败没有回滚，可能卡死在 `DISPATCHING`

#### 现象
`dispatch_task()` 的顺序是：
1. 选中目标 session
2. `prepare_task_dispatch()` 写库：
   - attempt 置为 `PENDING`
   - lease 写入 `task_leases`
   - task 置为 `DISPATCHING`
3. 再通过 gRPC stream 发送 `StartTask`
4. 如果发送失败，只关闭 session 并返回 `NodeDisconnected`

但**没有把任务从 `DISPATCHING` 回滚到 `QUEUED`**。

#### 影响
如果在“写库成功但消息未送达”这个窗口发生断连：
- 任务会残留为 `DISPATCHING`
- 还带着 `assigned_node_id`
- 若后续没有人工操作或额外补偿逻辑，任务可能长期悬挂

这属于典型的“**状态先行、消息失败、缺补偿**”问题。

#### 证据
- 先 prepare，再 send：
  - `crates/media-core/src/control_plane.rs:111-141`
- prepare 阶段已把 task 写成 `DISPATCHING` 并写 lease：
  - `crates/media-core/src/repository.rs:1561-1665`
- 发送失败只 `close_session()`，未回滚任务：
  - `crates/media-core/src/control_plane.rs:139-141`
- `close_session()` 只把节点标记 unhealthy：
  - `crates/media-core/src/control_plane.rs:436-454`

#### 建议
- 方案 A：发送失败时立刻补偿事务，把任务退回 `QUEUED`、清空 `assigned_node_id`、删除 lease。
- 方案 B：引入 dispatch 超时清扫器：`DISPATCHING` 超过阈值且未收到 `accepted/starting/running` 事件，则自动回滚重排。
- 最好两者都做：**即时补偿 + 定时兜底**。

---

### 发现 3（P1）`max_runtime_slots` 目前只是观测指标，不是容量约束，存在超发风险

#### 现象
agent 侧有 `max_runtime_slots` 配置，但当前只用于心跳中的 `slot_usage` 计算；没有看到任何：
- semaphore
- admission control
- 超额拒绝
- core 侧硬性过滤满载节点

core 侧只是把 `slot_usage` / `running_tasks` 当成排序因子，选择“相对更空”的节点，而不是“只选择未满节点”。

#### 影响
- 单节点环境下，调度器会继续向同一节点派发新任务，即使该节点已达到配置上限。
- 多节点环境下，若所有节点都接近满载，也不会停发，只会选一个“相对没那么满”的节点继续压上去。
- 这会放大 CPU、IO、带宽、ZLM 会话数争用，导致尾延迟和失败率上升。

#### 证据
- `max_runtime_slots` 仅出现在 agent 配置/心跳采样：
  - `crates/media-agent/src/config.rs:69-94`
  - `crates/media-agent/src/heartbeat.rs:19-52`
- core 只是按 load 排序选最小值：
  - `crates/media-core/src/control_plane.rs:504-540`
  - `crates/media-core/src/control_plane.rs:820-845`
- agent `start_task()` 没有本地并发上限检查：
  - `crates/media-agent/src/runtime.rs:309-321`

#### 建议
- agent 侧引入硬性 runtime slot gate（推荐 `tokio::Semaphore` 或显式 admission check）。
- core 侧 `pick_best_session()` 应过滤掉 `slot_usage >= 1.0` 或 `running_tasks >= max_slots` 的节点。
- 心跳只适合做“调度倾向”，**不能代替容量约束**。

---

### 发现 4（P1）突发任务会基于陈旧心跳做决策，短时间内容易把一批任务压到同一节点

#### 现象
session 负载只在收到 heartbeat 时更新；调度时不会做“乐观占位”或“预留一个 slot 再选下一个”。

这意味着在两次 heartbeat 之间，如果连续触发多次 `dispatch_task()`：
- 每次看到的都是同一份旧负载快照
- 调度器可能重复选中同一个“当前看起来最空”的节点

#### 影响
这会导致 burst traffic 下的**节点倾斜（burst skew）**：
- 某一节点在 1~10 秒内接到远超预期的一串任务
- 其余节点来不及被选到
- 配合“无硬上限”问题时风险更大

#### 证据
- heartbeat 到 session load 的更新路径：
  - `crates/media-core/src/control_plane.rs:483-501`
- 调度选择直接读取当前 session load，不做预留：
  - `crates/media-core/src/control_plane.rs:111-123`
  - `crates/media-core/src/control_plane.rs:504-540`
- agent 心跳间隔为 10 秒：
  - `crates/media-agent/src/control_plane.rs:32`

#### 建议
- 在 core 侧引入 `inflight_dispatches` / `reserved_slots`，选中节点后立刻做内存级预留。
- 收到 `accepted/running/rejected/dispatch timeout` 时释放预留。
- 若要更稳，可以把 reservation 也写入数据库或分布式缓存，以支持多实例 core。

---

### 发现 5（P1）节点断连后只标记 unhealthy，没有自动回收该节点上的在途任务

#### 现象
session 断开时，当前逻辑只做：
- 从内存 session map 删除
- 调用 `update_node_health(node_id, false, None)`

没有看到：
- 扫描该节点的 `DISPATCHING/STARTING/RUNNING/RECOVERING` 任务
- 标记 `LOST`
- 回收 lease
- 重新排队/重派发

而 `mark_task_lost()` 的触发点主要来自 ZLM hook（如 `on_rtp_server_timeout`），并不是通用的“agent 断连恢复机制”。

#### 影响
- 普通 process 类任务若节点宕机，core 未必能及时把任务转入 `LOST/QUEUED`
- 任务可能长期停留在旧节点上绑定的中间状态
- 断连恢复依赖外部 hook，会导致行为不一致：有媒体 hook 的任务会动，没 hook 的任务更容易挂住

#### 证据
- 断连只更新节点健康：
  - `crates/media-core/src/control_plane.rs:436-454`
- lost 逻辑通过 ZLM hook 驱动：
  - `crates/media-core/src/main.rs:1225-1245`
  - `crates/media-core/src/repository.rs:2574-2611`
  - `crates/media-core/src/repository.rs:3097-3136`

#### 建议
- 节点 session close 时，拉出该节点的 active tasks：
  - `DISPATCHING` → 回滚 `QUEUED`
  - `STARTING/RUNNING/RECOVERING` → 置 `LOST` 或按策略进入 `QUEUED`
- 再根据任务恢复策略决定是否自动重试。
- 不要把“断连恢复”建立在媒体 hook 是否触发之上。

---

## 三、次要观察

### 观察 A：lease 存在，但没有形成完整的防重/防陈旧闭环

`prepare_task_dispatch()` 会写入 `task_leases`，带 60 秒过期时间；但 agent 侧对 lease 只检查“非空”，没有看到更严格的 token 校验。core 在记录 `TaskEvent/TaskSnapshot/TaskProgress` 时，也主要按 `task_id + attempt_no` 更新状态。

这意味着 lease 目前更像“记录元数据”，而不是严格的一致性防线。

证据：
- 写入 lease：`crates/media-core/src/repository.rs:1611-1629`
- agent 仅检查 lease 非空：`crates/media-agent/src/runtime.rs:309-321`
- core 接收 agent 事件/快照时直接更新任务状态：`crates/media-core/src/repository.rs:1875-2250`

---

## 四、优先级建议

### P0（已完成）
1. 增加通用 queued sweep / pending dispatcher。
2. 为 `DISPATCHING` 引入发送失败补偿与超时回滚。

### P1（已完成）
3. 落实 `max_runtime_slots` 的硬性并发限制。
4. 在 core 侧加入 reserved slots / inflight dispatch 机制，减少 burst 倾斜。
5. 节点断连时自动回收在途任务。

### P2（后续建议）
6. 用 lease token 把 dispatch → accept → snapshot/event 的链路做严格校验。
7. 给上述行为补上集成测试：
   - 无节点创建 immediate 任务后，节点上线能否自动派发
   - prepare 后 send 失败是否自动回滚
   - burst 10 个任务是否会超过 `max_runtime_slots`
   - 节点断连后运行中任务是否被自动置 lost/requeued

---

## 五、验证情况

已执行：
- `cargo test -- --nocapture`

结果：
- 修复后再次执行 `cargo test -- --nocapture`，当前测试集全部通过
- 已补充并通过与本次修复直接对应的回归测试：
  - 满载节点不再继续派发
  - burst 派发会利用 reserved slots 分散到不同节点
  - agent 侧 `max_runtime_slots` 超限会直接拒绝新任务
  - session close 会回收 `DISPATCHING` / `RUNNING` 任务并完成 requeue / retry
- 仍有部分数据库相关测试会在数据库不可达时跳过，因此**lease token 严格校验与更强端到端覆盖仍建议作为下一批工作继续推进**

---

## 六、最终判断

**原始问题判断成立，但 P0/P1 主缺口现已完成修补。**

也就是说：
- 节点短暂离线
- 派发窗口断连
- 突发批量下发任务
- 节点达到并发上限

这些此前高风险场景，现在都已具备明确的补偿或限流路径，不再停留在“尽力而为的派发器”状态。

当前更值得继续推进的是：
- lease token 的严格链路校验
- 更强的端到端/数据库集成覆盖
- 多实例 core 场景下 reservation 的一致性策略
