# Runtime Process P08：进程组和 PID 防护

## 任务目标

强化 FFmpeg runtime 的进程停止模型：优先让每个 managed process 使用独立 process group，stop/force kill 针对 process group 执行，并逐步加入 PID 复用防护。目标是降低子进程残留和极小概率误杀风险。

## 当前证据

当前停止路径主要依赖：

- `libc::kill(pid, signal)`。
- `signal_runtime_pids` 对主进程和 companion pids 发信号。
- `schedule_force_kill_if_running` 延迟强杀仍按 pid 检查 runtime map。

这对普通场景可用，但媒体进程可能派生子进程；同时 PID 复用在极端情况下可能造成 force kill 误杀窗口。

## 实施清单

- 在 Unix 平台下，managed process spawn 前使用 `CommandExt::pre_exec` 调用 `libc::setpgid(0, 0)`，让 runtime 主进程成为独立 process group leader。
- 在 `ManagedRuntime` 中记录：
  - `pid`
  - `pgid`
  - companion pids/pgids，如 companion 不单独建组则明确策略。
- stop 时优先 `killpg(pgid, SIGTERM)`，保留 pid fallback。
- force kill 时优先 `killpg(pgid, SIGKILL)`，保留 pid fallback。
- 记录进程 start time：
  - Linux 可从 `/proc/{pid}/stat` 读取。
  - force kill 前校验 pid start time 一致。
- 对非 Linux 或读取失败路径保留现有 pid 检查行为。
- 更新 persisted metadata，使恢复后能知道 pgid/start time；注意兼容旧 runtime.json 缺字段。

## 验收标准

- 新启动的 managed process 有独立 process group。
- stop 和 force kill 能清理同一 process group 下的子进程。
- PID start time 不匹配时，不执行 force kill，并记录 warning。
- 旧 persisted runtime 缺 pgid/start time 时仍可被 adopt/stop。
- ZLM-only runtime 不受 process group 改动影响。

## 测试场景

- 启动一个会派生子进程的 mock command，stop 后确认 process group 内进程都退出。
- 模拟 pid start time 不匹配，确认 force kill 被跳过。
- 旧 metadata 无 pgid/start time，adopt 后 stop 仍走 fallback。
- companion recording 进程在 stop/rollback 中被正确清理。

## 实施记录

本轮实现将进程组防护限定在 managed process/companion process，ZLM-only runtime 不记录本地 process identity，也不改变 ZLM API 清理策略。

已固化的策略：

- 主 managed process 和 companion process 在 spawn 前通过 `pre_exec` 调用 `setpgid(0, 0)`，分别成为独立 process group leader。
- 内存态 `ManagedRuntime` 记录 `ProcessIdentity { pid, pgid, pid_start_time }`；主进程放在 `process`，伴随进程放在 `companion_processes`。
- 持久化 metadata 增加 `process`；`companion_recording` 增加可选 `pgid` 和 `pid_start_time`，旧 runtime.json 缺字段时继续从 pid fallback。
- stop 和 rollback 优先 `killpg(pgid, signal)`，pgid 缺失或 group 不存在时 fallback 到 `kill(pid, signal)`。
- 延迟 SIGKILL 前校验 Linux `/proc/{pid}/stat` start time；如果当前 start time 与记录值不一致，则跳过 force kill 并记录 warning。
- 如果主进程已退出但 pgid 仍存在，force kill 仍可清理同组残留子进程；pid-only 旧 runtime 保持原有 pid 语义。

新增测试覆盖：

- managed process 启动后记录 pgid/start time，并写入 persisted `runtime.json`。
- stop task 会清理同 process group 下由 mock command 派生的子进程。
- pid start time 不匹配时，延迟 force kill 被跳过。

## 依赖和风险

- 建议在 P04 rollback 后执行，避免启动失败时新 process group 泄漏。
- `pre_exec` 只能做 async-signal-safe 操作，闭包内不要分配或记录日志。
- 这是 Linux/native 运行时硬化任务，需注意 macOS 或测试环境差异。
