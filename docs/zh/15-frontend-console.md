# 15. Web 管理台页面规格

## 1. 文档目标

本文件定义 Web 管理台的基础原型范围，覆盖页面清单、路由、数据源、核心字段和最小交互。当前控制台实现基于 `Vue 3 + TypeScript + Vite`，以组件化页面、类型化 API client 和独立任务创建页为基础结构。

## 2. 设计原则

- 管理台是控制面工具，不是播放门户。
- 所有页面以信息密度和可调试性优先。
- 列表页先保证可筛选、可排序、可跳转，再追求复杂交互。
- 控制面数据接口只访问 `media-core`；录像、在线 HTTP 流和文件产物可由浏览器直连工作节点地址。
- 默认轮询刷新控制面数据，避免依赖手工刷新。
- 有限取值字段优先使用枚举下拉，不要求用户记忆内部枚举值。
- 任务创建默认面向非技术用户提供引导说明，同时保留专家模式与规格预览。

## 3. 路由清单

| 路由 | 页面 | 说明 |
| --- | --- | --- |
| `/overview` | 系统总览 | 全局指标、最近任务、节点健康 |
| `/login` | 登录页 | 密码登录、Bearer Token 登录 |
| `/api-docs` | 外部 API 文档 | 对接说明与请求示例 |
| `/tasks` | 任务中心 | 任务列表、筛选、创建、重试、停止、克隆 |
| `/tasks/new` | 新建任务 | 引导式创建、专家模式、规格预览 |
| `/tasks/:id` | 任务详情 | 基本信息、状态、事件、日志、规格差异 |
| `/streams` | 流中心 | 在线流、播放地址、关联任务、viewer 数 |
| `/multicast` | 组播中心 | 组播任务、网卡、TTL、地址、端口 |
| `/records` | 录像中心 | 录像文件、大小、时长、来源、检索 |
| `/file-artifacts` | 文件产物 | 桥接输出与转码输出的 HTTP 地址、文件路径 |
| `/security` | 安全设置 | 修改密码、机器 API 白名单 |
| `/nodes` | 节点中心 | 节点健康、能力、负载、ZLM 概览 |
| `/debug` | 调试台 | ZLM 调试、会话、玩家、关流、踢会话 |

## 4. 页面规格

### 4.1 任务中心

列表字段：

- `task_id`
- `name`
- `type`
- `status`
- `priority`
- `assigned_node`
- `created_by`
- `created_at`
- `updated_at`

筛选项：

- 状态
- 类型
- 节点
- 关键字
- 创建时间

操作：

- 新建任务
- 启动
- 停止
- 取消
- 重试
- 克隆

### 4.2 任务详情

页签：

- 概览
- 事件
- 日志
- `requested_spec`
- `resolved_spec`

概览卡片：

- 当前状态
- 当前 Attempt
- 执行节点
- 回调状态
- 最近错误
- 录像摘要
- 流绑定摘要

概览面板补充：

- 展示 `common.callback_url`
- 展示最近一次任务回调的事件类型、状态、时间、HTTP 状态和错误摘要
- 未配置回调时明确展示“未配置”

### 4.3 流中心

列表字段：

- `schema`
- `vhost`
- `app`
- `stream`
- `task_id`
- `node`
- `viewer_count`
- `recording`
- `play_urls`

说明：

- `viewer_count` 以后端从节点 ZLM `getMediaList.totalReaderCount` 富化结果为准。
- `play_urls` 由后端根据在线 schema 和节点 `agent_stream_addr` 生成，不依赖前端本地拼接。
- 在线 schema 包含 `rtmp` 时，`play_urls` 同时包含 RTMP 与对应 HTTP-FLV (`.live.flv`) 地址。
- `play_urls` 表示同一条内部流当前可暴露的播放协议地址集合，不表示任务并行输出了多个独立目标。

操作：

- 跳转任务
- 复制播放地址
- 关闭流

### 4.4 组播中心

列表字段：

- `task_id`
- `mode`
- `group`
- `port`
- `interface_ip`
- `ttl`
- `node`
- `status`

额外信息：

- 最近码率
- 最近错误
- 上下游绑定

### 4.5 录像中心

列表字段：

- `record_id`
- `task_id`
- `stream`
- `file_path`
- `http_url`
- `file_size`
- `time_len`
- `start_time`
- `source`

说明：

- 面向录像成品索引，不展示仅用于实时播放的 HLS 切片文件。
- `HLS` 录制在列表中按 `m3u8` 播放列表展示，不展开底层 `ts` segment。
- `file_path` 显示节点挂载前缀裁剪后的相对路径，例如 `/node-192_168_6_10-hls/<task-id>/index.m3u8`。

操作：

- 筛选日期
- 复制路径
- 复制 HTTP 地址
- 打开录像 HTTP 地址
- 跳转任务

### 4.6 文件产物

列表字段：

- `artifact_id`
- `artifact_kind`
- `task_id`
- `node_id`
- `file_name`
- `file_path`
- `http_url`
- `file_size`
- `created_at`

操作：

- 按产物类型、任务和时间筛选
- 复制路径
- 复制 HTTP 地址
- 打开文件产物
- 跳转任务

说明：

- `file_path` 显示节点挂载前缀裁剪后的相对路径，例如 `/node-192_168_6_10-mp4/<task-id>/output.mp4`。

### 4.7 节点中心

列表字段：

- `node_name`
- `healthy`
- `last_seen_at`
- `cpu_percent`
- `mem_percent`
- `running_tasks`
- `network_mode`
- `zlm_version`

详情信息：

- 能力矩阵
- 当前任务
- 最近心跳
- ZLM 统计

### 4.8 调试台

管理员专用，包含：

- 媒体列表查询
- Session 列表查询
- 玩家列表查询
- 线程负载与对象统计
- 踢会话
- 批量踢会话
- 关闭流
- 抓图
- Hook 时间线

## 5. 任务创建页

以独立页面 `/tasks/new` 实现，默认进入引导式创建，同时保留专家模式。

引导式创建固定流程：

1. 选择推荐场景。
2. 选择任务类型与任务名称。
3. 填写输入源。
4. 填写处理方式。
5. 按任务类型填写“内部流与播放暴露”或“直接输出目标”。
6. 如为 `stream_ingest`，单独填写录制。
7. 配置恢复与调度。
8. 查看自然语言摘要、`requested_spec` 与 `resolved_spec` 预览。
9. 提交创建。

专家模式要求：

- 保留高级 JSON 覆盖入口。
- 保留最终 `resolved_spec` 预览。
- 不恢复后端模板和任务预设能力。

动态表单规则：

- 任务类型切换时，只显示当前类型相关字段。
- 提交前必须显示最终 `resolved_spec` 预览。
- `stream_ingest` 之外的任务类型不显示 `record.*`。
- `record.enabled = false` 时隐藏录制相关字段。
- `record.enabled = true` 时展示 `record.format`、`record.duration_sec`、`record.segment_sec`、`record.as_player` 等录制字段；录制目录由系统托管，不暴露自定义路径输入。
- `schedule.start_mode = at|cron` 时仅展示对应调度字段。
- `stream_ingest` 只展示 `stream.*` 和 `expose.*`，不再展示“推流目标 URL”。
- `stream_bridge` / `file_transcode` 只展示 `publish.*`，不再展示 `stream.*` / `expose.*`。
- `input.kind=hls|http_ts` 时必须显式展示 `input.source_mode`；其他输入类型按规则自动锁定 `live` 或 `vod`。
- `input.kind=file` 时只展示“相对路径”输入框，页面需明确提示工作目录根是 `/data/media/work`，标准离线实例的宿主机映射目录是 `/home/streamserver/data/media/work`，前导 `/` 会被自动忽略。
- 有限取值字段统一使用下拉枚举，不使用自由文本框。
- 引导式模式下需要显示内联教程说明、推荐默认值、示例和常见误区提示。

## 6. 状态呈现规则

| 状态 | 颜色建议 | 说明 |
| --- | --- | --- |
| `RUNNING` | 绿色 | 执行中 |
| `STARTING`, `DISPATCHING`, `RECOVERING` | 蓝色 | 过程态 |
| `STOPPING` | 橙色 | 正在回收 |
| `FAILED`, `LOST` | 红色 | 失败或失联 |
| `CREATED`, `VALIDATING`, `QUEUED` | 灰色 | 未开始或排队 |
| `SUCCEEDED` | 绿色描边 | 已完成 |
| `CANCELED` | 灰色描边 | 已取消 |

## 7. 权限规则

- 普通业务调用方不可见 `/debug`。
- 只读用户隐藏所有写操作按钮。
- 节点信息默认仅管理员可见。

## 8. 接口依赖

| 页面 | 主要接口 |
| --- | --- |
| 登录与会话 | `GET /me`, `POST /auth/login`, `POST /auth/refresh`, `POST /auth/logout` |
| 安全设置 | `POST /auth/change-password`, `GET /security/machine-allowlist`, `PUT /security/machine-allowlist` |
| 任务中心 | `GET /tasks`, `POST /tasks`, `POST /tasks/{id}/start|stop|cancel|retry|clone` |
| 任务详情 | `GET /tasks/{id}`, `GET /tasks/{id}/events`, `GET /tasks/{id}/logs`, `GET /tasks/{id}/resolved-spec` |
| 流中心 | `GET /streams`, `POST /debug/zlm/close-stream` |
| 组播中心 | `GET /tasks?type=stream_bridge`, `GET /nodes` |
| 录像中心 | `GET /records` |
| 文件产物 | `GET /file-artifacts` |
| 节点中心 | `GET /nodes`, `GET /nodes/{id}/heartbeats`, `GET /debug/zlm/statistic`, `GET /debug/zlm/threads-load`, `GET /debug/zlm/work-threads-load` |
| 调试台 | `GET /debug/zlm/media`, `GET /debug/zlm/sessions`, `GET /debug/zlm/players`, `GET /debug/zlm/statistic`, `GET /debug/zlm/threads-load`, `GET /debug/zlm/work-threads-load`, `GET /debug/hooks`, `GET /debug/zlm/snap`, `POST /debug/zlm/kick-session`, `POST /debug/zlm/kick-sessions` |
