# 09. 测试计划与质量门禁

## 1. 文档目标

本文件定义 V1 的测试层次、环境要求、关键用例和验收标准。代码完成不等于交付完成，只有满足本文件中的场景和指标，才视为功能闭环。

## 2. 测试层次

| 层次 | 范围 | 负责方 |
| --- | --- | --- |
| 单元测试 | 参数解析、状态迁移、默认值合并、错误映射 | 后端 |
| 集成测试 | 数据库、API、ZLM 适配、Agent RPC、FFmpeg 渲染 | 后端 |
| 端到端测试 | native 实例下跑完整任务链路 | 后端 + QA |
| 故障注入测试 | Core/Agent/ZLM 重启、网络断连、磁盘异常 | 后端 + QA |
| 手工联调 | 播放、推流、录像、组播、调试页 | QA + 运维 |

## 3. 环境矩阵

### 3.1 开发环境

- 本地 native all-in-one 实例
- 重点验证 API、状态机、基础 UI、FFmpeg 渲染

### 3.2 集成环境

- 至少 1 个 `media-core`
- 至少 1 个 `media-agent + ZLM + FFmpeg`
- PostgreSQL
- 可用组播网络

## 4. 必测场景

### 4.1 通用

- 创建任务并立即启动。
- 创建任务但手动启动。
- 幂等键重复提交。
- 非法状态执行操作，返回 `409`。
- 模板渲染与 `resolved_spec` 冻结。

### 4.2 `live_relay`

- RTSP 输入成功拉起并生成内部流。
- 启用录制并收到 `record_file_created`。
- 停止后 ZLM 内部流和录制都被清理。
- 输入源断开后任务进入 `LOST` 或 `FAILED`。

### 4.3 `file_transcode`

- 本地文件转 MP4 成功。
- 失败文件返回明确错误分类。
- 产物缺失时不得进入 `SUCCEEDED`。

### 4.4 `file_to_live`

- 文件推到本地 ZLM 并能在流中心看到。
- 推流过程中停止任务，FFmpeg 进程被优雅终止。
- ZLM 重启后触发恢复流程。

### 4.5 `stream_bridge` 组播模式

- 组播输入桥接到 ZLM 成功。
- 持续组播输出成功。
- 错误网卡或 TTL 配置能返回明确错误。

### 4.6 `rtp_receive`

- 成功打开 RTP 接收端口。
- 收不到数据时进入超时流程。
- 恢复时能通过 `listRtpServer` 对账。

## 5. 故障注入场景

- `media-core` 运行中重启。
- `media-agent` 运行中重启。
- ZLM 重启并重新发送 keepalive。
- PostgreSQL 短暂不可用。
- 磁盘目录不可写。
- Agent 与 Core 的 gRPC 流断开 30 秒。

## 6. 验收标准

### 6.1 功能验收

- 5 种任务类型均有至少 1 条成功链路。
- 每种任务类型至少有 1 条失败链路和 1 条恢复或降级链路。
- Web 管理台 6 个页面均可正常展示数据并执行基础操作。

### 6.2 一致性验收

- `requested_spec` 与 `resolved_spec` 可在详情页查看。
- 任务状态、Attempt 状态、日志、事件、录像文件能关联到同一个 `task_id`。
- 重试生成新 Attempt，克隆生成新 Task。

### 6.3 稳定性验收

- Core 重启后，运行中的任务能在 30 秒内进入恢复流程。
- Agent 断连 30 秒内节点会被标记为不健康。
- 重复 Hook 不会重复入库录像或重复驱动状态迁移。

## 7. 自动化优先级

必须自动化：

- 参数解析与默认值合并
- 状态机迁移
- 幂等键处理
- DDL 迁移
- API 合同测试

可先手工：

- 真实摄像头接入
- 复杂组播网络联调
- 浏览器播放兼容性

## 8. 与 Linux CI 一致的本地质量门

服务端发布门禁以 Linux AMD64 为准，由 `.github/workflows/server-ci.yml`
执行。请从仓库根目录使用 Rust 1.85.0、Node.js 20、PostgreSQL 16 和 18.3 运行
以下命令。测试数据库账号必须具备创建和删除临时数据库的权限。下面的
可复制示例会为两个数据库版本分别启动仅供测试的临时容器。

```bash
set -euo pipefail
test "$(uname -s)" = "Linux"
test "$(uname -m)" = "x86_64"
sudo apt-get update
sudo apt-get install -y --no-install-recommends \
  libdbus-1-dev pkg-config postgresql-client protobuf-compiler
rustup toolchain install 1.85.0 \
  --profile minimal --component rustfmt --component clippy
rustup default 1.85.0
python3 -m pip install --disable-pip-version-check --no-deps -r tests/requirements-ci.txt
python3 tests/ci_workflow_contract_test.py
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cleanup_postgres() {
  docker rm -f streamserver-ci-postgres >/dev/null 2>&1 || true
}
trap cleanup_postgres EXIT
export REQUIRE_TEST_DATABASE=1
for POSTGRES_VERSION in 16 18.3; do
  cleanup_postgres
  docker run --rm --detach --name streamserver-ci-postgres \
    --env POSTGRES_PASSWORD=test --publish 5432:5432 "postgres:${POSTGRES_VERSION}"
  export TEST_DATABASE_URL=postgresql://postgres:test@127.0.0.1:5432/postgres
  timeout 60 bash -c 'until psql "${TEST_DATABASE_URL}" -c "select 1" >/dev/null 2>&1; do sleep 1; done'
  cargo test --locked --workspace --all-targets
  cleanup_postgres
done
trap - EXIT
(
  cd crates/media-core/frontend
  npm ci
  npm run typecheck
  npm run test
)
```

Rust workspace 测试会分别在 PostgreSQL 16 和 18.3 上执行一次。
在这组精确门禁以外，不设置 `REQUIRE_TEST_DATABASE` 时仍保留 PostgreSQL
不可用便跳过数据库测试的本地开发体验。CI 将其设为 `1`，因此缺少数据库
配置、创建数据库失败或迁移失败都会使测试失败，不会被当作 skip。
这组命令是 Linux AMD64 服务端门禁。只在 Windows 出现的 workspace 失败，
包括 `media-agent` 编译失败，不应直接判定为服务端回归；必须先在 Linux
AMD64 上复现。桌面端打包仍由独立的 `desktop-client.yml` 负责。

原生 bundle 构建也保持独立，继续由 `server-native-bundles.yml` 执行，
不混入快速服务端质量门禁。
