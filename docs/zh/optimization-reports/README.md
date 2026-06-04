# 专项优化报告状态索引

本目录保存历史专项分析、优化方案和 PR 任务拆分。这里的文档不是当前实现契约，文中“当前代码”“当前证据”和源码行号只代表写作时的快照。

当前实现契约以以下文档为准：

- [API 规格](../03-api.md)
- [架构与部署拓扑](../01-architecture.md)
- [Agent-Core RPC](../04-agent-core-rpc.md)
- [FFmpeg ExecutionPlan 与媒体策略](../05-media-execution-plan.md)
- [Native 部署](../08-native-deployment.md)

## 状态说明

| 状态 | 含义 |
| --- | --- |
| 历史分析 | 问题或设计背景，不能直接当作当前代码事实。 |
| 历史任务 | 已被后续实现吸收或取代，保留用于追溯。 |
| 部分仍相关 | 仍有可复用问题判断，但必须先按当前代码复核。 |
| 待规划 | 仍是已知缺口，需要单独规划和实现。 |

## 文档状态

| 文档 | 状态 | 当前使用方式 |
| --- | --- | --- |
| [01 调度总问题清单](./01-scheduling-issues.md) | 历史分析 | 用于理解调度/状态机问题来源；具体问题是否仍存在需重新核对。 |
| [02 调度慢问题优化指南](./02-scheduling-latency-optimization.md) | 部分仍相关 | 调度 tick、串行 dispatch 等方向仍可复核；Agent 启动限流等证据已随 RuntimeManager 重构过时。 |
| [03 ZLM/FFmpeg 重构方案](./03-zlm-ffmpeg-refactor.md) | 历史任务 | 作为 ZLM/FFmpeg 边界演进记录；当前能力以 API 规格和代码为准。 |
| [04 ZLM/FFmpeg 重构补充](./04-zlm-ffmpeg-refactor-addendum.md) | 历史任务 | 作为重构补充记录；当前执行策略以 ExecutionPlan 文档和代码为准。 |
| [05 ZLM 流接入矩阵](./05-zlm-ingest-matrix.md) | 历史分析 | 作为协议能力判断背景；当前输入/输出能力以 API 规格为准。 |
| [06 Agent 录像清理](./06-agent-recording-cleanup.md) | 历史任务 | 作为产物清理方案背景；当前产物接口和清理行为以实现和运维文档为准。 |
| [07 Runtime Process P01](./07-runtime-process-p01-async-local-executor.md) | 历史任务 | Runtime async 化任务已被后续实现吸收。 |
| [08 Runtime Process P02](./08-runtime-process-p02-control-plane-runtime-jobs.md) | 历史任务 | 控制面 runtime job 化已被 RuntimeManager 路径吸收。 |
| [09 Runtime Process P03](./09-runtime-process-p03-async-bounded-adopt.md) | 历史任务 | adopt async 化任务已被 RuntimeManager 接管路径吸收。 |
| [10 Runtime Process P04](./10-runtime-process-p04-process-start-rollback.md) | 历史任务 | 启动 rollback guard 作为历史任务保留。 |
| [11 Runtime Process P05](./11-runtime-process-p05-zlm-start-rollback.md) | 历史任务 | ZLM 启动 rollback 作为历史任务保留。 |
| [12 Runtime Process P06](./12-runtime-process-p06-atomic-runtime-persistence.md) | 历史任务 | runtime 持久化原子写入作为历史任务保留。 |
| [13 Runtime Process P07](./13-runtime-process-p07-lock-boundary-audit.md) | 历史任务 | 锁边界审计作为历史任务保留。 |
| [14 Runtime Process P08](./14-runtime-process-p08-process-group-pid-guards.md) | 历史任务 | 进程组和 PID 防护作为历史任务保留。 |
| [15 RuntimeManager P01](./15-runtime-manager-p01-read-model.md) | 历史任务 | RuntimeReadModel 铺路任务已进入当前实现。 |
| [16 RuntimeManager P02](./16-runtime-manager-p02-facade-actor.md) | 历史任务 | facade actor 阶段已被后续 RuntimeManager 实现取代。 |
| [17 RuntimeManager P03](./17-runtime-manager-p03-scheduling-session.md) | 历史任务 | 调度/session 迁移已进入 RuntimeManager 当前路径。 |
| [18 RuntimeManager P04](./18-runtime-manager-p04-state-mirror.md) | 历史任务 | mirror 阶段已被 actor 权威状态路径取代。 |
| [19 RuntimeManager P05](./19-runtime-manager-p05-managed-process-start-outcome.md) | 历史任务 | managed process start outcome 化作为历史任务保留。 |
| [20 RuntimeManager P06](./20-runtime-manager-p06-zlm-start-outcome.md) | 历史任务 | ZLM start outcome 化作为历史任务保留。 |
| [21 RuntimeManager P07](./21-runtime-manager-p07-monitor-internal-events.md) | 历史任务 | monitor internal event 化已进入当前 RuntimeManager 路径。 |
| [22 RuntimeManager P08](./22-runtime-manager-p08-stop-actor.md) | 历史任务 | stop actor 化已进入当前 RuntimeManager 路径。 |
| [23 RuntimeManager P09](./23-runtime-manager-p09-recording-actor.md) | 历史任务 | recording actor 化已进入当前 RuntimeManager 路径。 |
| [24 RuntimeManager P10](./24-runtime-manager-p10-adopt-recovery-actor.md) | 历史任务 | adopt/recovery actor 化已进入当前 RuntimeManager 路径。 |
| [25 RuntimeManager P11](./25-runtime-manager-p11-remove-registry-writes.md) | 历史任务 | 生产路径 registry/runtimes 写依赖已由 RuntimeManager backend store 收口。 |
| [26 RuntimeManager P12](./26-runtime-manager-p12-api-test-doc-cleanup.md) | 历史任务 | API、测试和文档收尾已进入当前 RuntimeManager 契约；日志保留另行规划。 |

## 后续待规划项

- 日志保留规则：`Agent` 本地日志和 Core 日志索引保留周期需要单独设计实现或明确外部运维依赖。
