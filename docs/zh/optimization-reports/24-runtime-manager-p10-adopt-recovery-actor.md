# RuntimeManager P10：adopt/recovery actor 化

## 任务目标

把 adopt 和 recovery 路径纳入 RuntimeManager actor。actor 先处理已有状态，worker 扫描 persisted runtimes 并探测 ZLM/process，最后由 actor commit adopted/recovered runtime。

## 前置条件

- P09 recording actor 化完成。
- async bounded adopt 已完成。
- monitor internal event 和 start outcome 化已完成。

## 实施清单

- actor 收到 `AdoptOrphans`：
  - 先从 `RuntimeManagerState` 找已有 matching runtime。
  - 已存在 runtime 更新 session/zlm metadata 后加入 adopted。
  - 缺失项交给 adopt worker。
- adopt worker 负责：
  - scan persisted runtimes。
  - process pid 快速探测。
  - ZLM proxy/RTP bounded concurrency 探测。
  - 对可恢复项返回 `RuntimeAdoptOutcome`。
  - 对需重启项生成 restart request 或 start outcome。
- actor commit adopted runtime：
  - 分配新 generation。
  - 插入 state/backend。
  - publish read model。
  - 启动 monitor。
  - reply adopted handles。
- recovery/restart 统一通过 `RuntimeStartOutcome` 回 actor commit。
- control-plane 对未 adopted 的 filter 项继续发送 orphaned event，除非本 PR 明确把该职责搬入 manager。

## 验收标准

- adopt worker 不直接 registry.track。
- adopted runtime monitor 使用 generation。
- persisted runtime 恢复失败不会污染 actor state。
- 已存在 runtime 不重复占 slot。
- recovery/restart 走 actor commit，不绕过权威状态。

## 测试场景

- active runtime reattach 后 session_epoch 更新。
- persisted process runtime adopted 后 monitor 接入 actor。
- ZLM proxy online 时 adopted，offline 时按现有语义 restart 或 orphaned。
- RTP server local port 更新后 adopted。
- adopt 失败项不进入 read model。

## 依赖和风险

- adopt 是最后迁移的复杂路径，不要提前执行。
- persisted runtime 兼容旧 metadata，缺 generation/pgid 字段时仍可恢复。
- orphaned event 的职责边界要保持 Core 协议语义不变。
