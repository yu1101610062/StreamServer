# 13. 路线图与实施拆解

## 1. 文档目标

本文件把 V1 蓝图拆成可执行开发批次，定义依赖关系、交付物和完成标准。文档中的阶段顺序就是推荐实施顺序。

## 2. 阶段划分

### M0 文档冻结

交付物：

- `docs/` 全套开发前文档
- 技术评审记录

完成标准：

- 任务模型、API、RPC、DDL、测试计划评审通过

### M1 工程骨架

交付物：

- Cargo workspace
- `media-core`、`media-agent`、`media-domain` 基础 crate
- 配置加载、日志、健康检查

完成标准：

- 服务可启动
- 基础配置和日志链路打通

### M2 数据与状态机

交付物：

- PostgreSQL 迁移
- 核心实体模型
- 任务状态机
- 幂等请求表和基础仓储

完成标准：

- 可创建 Task 并持久化
- 状态迁移有自动化测试

### M3 Agent 控制流与能力探测

交付物：

- gRPC `ControlPlane`
- 节点注册、心跳、能力探测
- 本地执行对象抽象

完成标准：

- 节点可上线
- FFmpeg/ZLM 能力可落库

### M4 ZLM 主链路

交付物：

- `ZlmAdapter`
- HookReceiver
- `live_relay` 与 `rtp_receive` 最小闭环

完成标准：

- `live_relay` 可跑通
- Hook 可入库并驱动状态变化

### M5 FFmpeg 主链路

交付物：

- `ExecutionPlan`
- `file_transcode`
- `file_to_live`
- `multicast_bridge`

完成标准：

- 3 类 FFmpeg 主链路均有成功用例

### M6 北向 API 与前端骨架

交付物：

- 任务、模板、节点、录像、调试接口
- 前端 6 个页面骨架

完成标准：

- 前端可完成创建任务、查看详情、查看日志、查看节点

### M7 恢复与硬化

交付物：

- 三方对账恢复
- 故障注入测试
- 调试接口收口
- 审计与权限

完成标准：

- Core/Agent/ZLM 重启均有可验证恢复行为

## 3. 依赖关系

```text
M0 -> M1 -> M2 -> M3
M3 -> M4
M3 -> M5
M4 + M5 -> M6
M4 + M5 + M6 -> M7
```

## 4. 并行建议

- `media-domain` 与 DDL 可先并行。
- `Agent RPC` 与 `ZLM Adapter` 可并行，但都依赖统一事件模型。
- 前端在 `03-api` 冻结后即可开始页面骨架开发。

## 5. 每阶段输出检查

每个里程碑结束时，至少产出：

- 代码
- 自动化测试
- 文档同步更新
- 演示脚本或验证记录
