# Runtime Process P06：runtime 持久化原子写入

## 任务目标

把关键运行时状态文件 `runtime.json` 改成 temp file + rename 的原子写入，降低进程崩溃、断电、磁盘异常导致半写入 JSON 的风险。`runtime.pid` 和 `runtime.cmd` 可同步采用同样 helper，但最小必做项是 `runtime.json`。

## 当前证据

当前 `crates/media-agent/src/runtime_persistence.rs` 中：

- `persist_runtime_state` 直接 `fs::write(work_dir.join(RUNTIME_STATE_FILE), state_json)`。
- `runtime.pid` 和 `runtime.cmd` 也直接 `fs::write`。
- `scan_persisted_runtimes` 只读取文件名等于 `runtime.json` 的文件，已经天然忽略其他文件名。

## 实施清单

- 新增 `atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()>` helper。
- helper 行为：
  - 写入同目录临时文件，例如 `runtime.json.tmp.<pid>` 或 `runtime.json.tmp`。
  - flush 文件内容；如性能可接受，调用 `sync_all()`。
  - `rename(tmp, path)`。
  - 失败时尽力删除 tmp。
- `runtime.json` 必须使用 atomic write。
- `runtime.pid`、`runtime.cmd` 可复用 helper；如果暂不改，文档和测试要说明只保证 `runtime.json`。
- 扫描 persisted runtime 时继续只读取 `runtime.json`，不要读取 `.tmp`。
- 写失败不能删除已有 `runtime.json`。

## 验收标准

- `runtime.json` 写入通过 temp + rename 完成。
- 新写入失败时，已有 `runtime.json` 仍可解析。
- `scan_persisted_runtimes` 忽略残留 tmp 文件。
- 持久化错误信息仍包含目标路径，便于排查。

## 测试场景

- 已存在合法 `runtime.json`，模拟新写入失败，确认旧文件不被破坏。
- 目录中存在 `runtime.json.tmp`，扫描时不会被当作 runtime state。
- 成功写入后 `runtime.json` 内容可反序列化为 `PersistedRuntimeState`。
- 无 pid/command 时仍会清理旧 `runtime.pid`/`runtime.cmd`。

## 依赖和风险

- 可在 P04/P05 前后独立执行，但 rollback 测试会受益于更可控的 persistence helper。
- 如果 helper 使用固定 tmp 文件名，并发写同一个 runtime dir 可能互相覆盖；优先使用带进程号或随机后缀的 tmp 名。
- 不要扩大扫描范围或改变 persisted runtime 兼容格式。
