# StreamServer

StreamServer 当前交付主线是 native 离线包：目标机运行时不需要 Docker，服务由 systemd 托管，FFmpeg、ZLMediaKit 和 PostgreSQL runtime 随包交付或按角色启用。构建阶段可以使用 Docker builder 提取 runtime 资产，运行阶段不依赖 Docker/Compose。

## Native 快速入口

构建 Linux AMD64 native 包：

```bash
./scripts/build-native-bundle.sh --without-gpu
./scripts/build-native-bundle.sh --with-gpu
./scripts/build-native-bundle.sh --control-plane-minimal
```

目标机验收：

```bash
./scripts/verify-native-bundle-on-target.sh \
  --bundle dist/streamserver-native-v0.1.0-linux-amd64-cpu-only-20260602.tar.gz \
  --host <target-host>
```

安装与检查：

```bash
tar -xzf streamserver-native-*.tar.gz
cd streamserver-native-*
./install.sh --check-only
sudo ./install.sh
```

常用运行命令：

```bash
/opt/streamserver/<role>/bin/streamserverctl status
/opt/streamserver/<role>/bin/streamserverctl health
/opt/streamserver/<role>/bin/streamserverctl logs
```

native 包验收报告由目标机验证脚本生成并拉回到：

```text
dist/native-verification-target-<timestamp>.md
```

没有该报告，不能认为 native 包完成目标机验收。完整流程见 [Native 无 Docker 运行时部署](docs/18-native-static-deployment.md)。

## Codec Smoke

release/native 验证建议额外执行本机 FFmpeg codec matrix：

```bash
./scripts/smoke-codec-matrix.sh
```

脚本默认使用 `ffmpeg` 和 `ffprobe`，也可通过环境变量指定随包 runtime：

```bash
FFMPEG_BIN=/opt/streamserver/worker/runtime/ffmpeg/bin/ffmpeg \
FFPROBE_BIN=/opt/streamserver/worker/runtime/ffmpeg/bin/ffprobe \
./scripts/smoke-codec-matrix.sh
```

当前输出格式策略：`mkv` 和 `matroska` 都会使用 FFmpeg muxer `matroska`，文件扩展名为 `.mkv`；WebM 只保留为上传输入能力，暂不作为输出目标格式暴露或接受。

## 文档索引

本目录是 StreamServer 的开发和交付文档集合。[设计文档](docs/设计文档.md) 负责说明总体方案和技术选型；本目录下其余文档负责把总体设计下沉成可执行规格、研发流程、部署和验收步骤。

## 阅读顺序

1. [设计文档](docs/设计文档.md)
2. [01-产品边界与术语](docs/01-产品边界与术语.md)
3. [02-系统上下文与部署拓扑](docs/02-系统上下文与部署拓扑.md)
4. [03-领域模型与状态机](docs/03-领域模型与状态机.md)
5. [04-API规格](docs/04-API规格.md)
6. [05-Agent与Core-RPC规格](docs/05-Agent与Core-RPC规格.md)
7. [06-ZLM适配与Hook规格](docs/06-ZLM适配与Hook规格.md)
8. [07-FFmpeg-ExecutionPlan规格](docs/07-FFmpeg-ExecutionPlan规格.md)
9. [08-数据库设计与DDL](docs/08-数据库设计与DDL.md)
10. [09-前端基础原型与页面规格](docs/09-前端基础原型与页面规格.md)
11. [10-测试计划与验收标准](docs/10-测试计划与验收标准.md)
12. [11-环境准备与依赖基线](docs/11-环境准备与依赖基线.md)
13. [12-研发流程与协作规范](docs/12-研发流程与协作规范.md)
14. [13-里程碑与实施拆解](docs/13-里程碑与实施拆解.md)
15. [14-风险清单与预案](docs/14-风险清单与预案.md)
16. [16-部署架构图](docs/16-部署架构图.md)
17. [18-Native 无 Docker 运行时部署](docs/18-native-static-deployment.md)

## 文档分工

| 文档 | 目标读者 | 作用 |
| --- | --- | --- |
| `设计文档.md` | 架构负责人、评审人 | 说明总体方案、关键选型和能力边界 |
| `01-03` | 产品、后端、前端、测试 | 锁定范围、术语、领域模型和状态语义 |
| `04-07` | 后端、Agent、适配层 | 锁定接口契约、执行模型和第三方集成边界 |
| `08` | 后端、DBA | 锁定数据库表结构、约束、索引和迁移策略 |
| `09` | 前端、后端 | 锁定管理台信息架构、字段、状态和最小交互 |
| `10` | QA、后端、运维 | 锁定测试层次、验收条件和联调方式 |
| `11-14` | 后端、运维、项目负责人 | 锁定环境、协作规范、实施顺序和风险预案 |
| `16` | 后端、运维、架构评审 | 输出可直接用于部署讨论的详细架构图 |
| `18` | 运维、交付、后端 | native 包构建、安装、systemd 运行和目标机验收 |

## 维护规则

- 新增能力前，先更新对应文档，再改代码。
- 接口、状态、表结构变更必须同步更新 `03`、`04/05`、`08` 三份文档。
- native 包或运行时能力变更必须同步更新 README、`18` 和目标机验收脚本。
