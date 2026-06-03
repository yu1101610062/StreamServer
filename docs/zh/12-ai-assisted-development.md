# 12. AI-assisted Engineering Notes

本文说明 StreamServer 使用 AI 辅助工程的方式、边界和验收原则。它不是把责任交给 AI，而是把 AI 当作需求拆解、代码生成、文档重构和测试补全的工程工具。

## 1. Ownership

项目 owner 对以下事项负责：

- 需求拆解和优先级判断。
- 架构边界、模块职责和数据模型决策。
- 代码 review、测试验收和部署验证。
- 文档准确性、风险取舍和后续重构。
- release/native 包的目标机验证。

AI 生成的代码、文档和脚本必须经过人工审查，并通过仓库内的测试或现场验证。

## 2. 使用范围

适合 AI 辅助的工作：

- 根据现有代码补充测试。
- 重构重复逻辑，但保持模块边界不变。
- 生成文档草稿、README、ADR 和变更说明。
- 梳理风险清单、接口矩阵和测试矩阵。
- 根据真实错误日志定位问题并提出修复补丁。

不应直接交给 AI 决定的工作：

- 生产安全边界。
- 数据删除、迁移和回滚策略。
- 现场网络、GPU、存储和权限基线。
- 重大架构取舍。
- 未验证的性能和稳定性承诺。

## 3. Review 原则

AI 产出进入代码库前必须检查：

- 是否符合当前模块边界。
- 是否改变 API、状态机、表结构或部署约定。
- 是否误删已有功能或用户修改。
- 是否补充了与风险匹配的测试。
- 是否能在本地或目标环境复现验证。

## 4. 验证闭环

最低验证链路：

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

前端变更应额外运行：

```bash
cd crates/media-core/frontend
npm run typecheck
npm run test
```

native 交付变更应额外验证：

```bash
./scripts/build-native-bundle.sh --without-gpu
./scripts/verify-native-bundle-on-target.sh --bundle <bundle> --host <target-host>
./scripts/smoke-codec-matrix.sh
```

## 5. 文档要求

AI 辅助改动完成后，相关文档必须同步：

- API、状态机、RPC、数据库变更同步到 `docs/zh/02-*` 到 `docs/zh/07-*`。
- native 打包、安装、运行时变更同步到 `README.md` 和 `docs/zh/08-native-deployment.md`。
- 测试策略和验收路径变更同步到 `docs/zh/09-testing.md`。
- 重大架构取舍新增或更新 `docs/adr/`。

## 6. 推荐表达

README 或对外说明可以写：

> This is an AI-assisted engineering project. The project owner is responsible for requirements, architecture, code review, testing, deployment validation, and iterative refactoring.

不建议使用不专业或容易误解的表述，例如“vibe coding”。
