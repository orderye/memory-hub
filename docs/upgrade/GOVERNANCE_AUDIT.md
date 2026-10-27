# 治理升级源码审计（P0 基线）

[![powered by KimiCode](../../.github/assets/kimi-badge.svg)](https://www.kimi.com/code)

> 审计对象：`toroyxxx/memory-hub`，`main` @ `92a809d`（工作区干净）。
> 方法：只读核对真实文件与符号；不根据其他项目结构推测。
> 配套文档：`REPOSITORY_AUDIT.md`（旧快照）、`LIGHTWEIGHT_AUDIT.md`（定基于 `3048efc`）。

---

## 1. 工程拓扑

| 层 | crate | 关键内容 |
|---|---|---|
| 领域/服务 | `crates/mem-core` | `domain.rs`（全部领域类型）、`memory_service.rs`、`backup_service.rs`、`chat_cognitive_service.rs`、`working_memory_service.rs`、`coding_memory_service.rs`、`collection_service.rs`（本次新增）、`access.rs`、`account_scope.rs`、`redaction.rs` |
| 持久化 | `crates/store` | `postgres/`（memory/working/coding/collections/backup/integration/access）、`in_memory/`、15 + 2 个迁移 |
| 检索 | `crates/retrieval` | 混合召回、MMR、场景权重、评测 Bench v2 |
| 整合 | `crates/consolidation` | 规则/向量候选、8 类关系、策略引擎 |
| 身份 | `crates/auth-core` / `authz` / `auth-web` | OAuth 2.1、PKCE、令牌族、ACL 校验 |
| 协议 | `crates/mcp` | 38 个工具的 schema 与执行器 |
| 进程 | `crates/bins/{auth-server,mcp-server,backup-worker}` | 三进程 + 内嵌 worker 模式 |
| 后台 | `crates/worker` | 备份、证据清理、认知、经验、整合作业与租约 |

编译特性：workspace 无全局 features；仅 `mcp`（`adapter-cjs`）、`store`（`pg-integration`）
与三个 bin 有局部 features。

## 2. 数据模型与迁移

| 迁移 | 内容 |
|---|---|
| 001 | 初始 schema（memories / versions / namespaces / grants / backups / proposals） |
| 002 | 账户沙箱 |
| 003 | 条件键唯一 |
| 004 | 查询索引 |
| 005 | CJK bigram FTS |
| 006 | 证据与关系（`memory_evidence` / `memory_relations`） |
| 007 | Working 任务 / 经验 / 关联 |
| 008 | Coding 线程 / 修订 |
| 009 | 复合命名空间外键加固 |
| 010 | 证据保留期 |
| 011 | 整合提案元数据 |
| 012 | 认知提案 |
| 013 | FTS 重新分词标记 |
| 014 | `memories(namespace, updated_at)` 索引 |
| 015 | halfvec 部分向量索引 |
| **016** | **经验资产生命周期 + 版本表**（本次） |
| **017** | **Agent 记忆集合 + 绑定**（本次） |

执行机制：无迁移框架；CI `postgres` job 用 `psql -v ON_ERROR_STOP=1` 按文件名顺序执行。
`PostgresHubStore` 启动时也有版本化清单与 `_hub_migrations` 账本（见 `store/src/postgres/mod.rs` 文档注释）。

## 3. 能力盘点

### 3.1 已实现（本次不重做）

| 能力 | 证据 |
|---|---|
| 不可变证据 + 脱敏 + 保留期 | `domain.rs::MemoryEvidence`、`redaction.rs`、迁移 006/010 |
| 时间有效性 / supersedes / 冲突 | `MemoryRelationKind`、`valid_from/valid_until`、`algorithm::is_superseded_at` |
| Chat 七层认知 + 三档治理 | `CognitiveLayer`、`CognitivePolicy`、`cognitive_*` 工具 |
| Working 任务/经验解耦 + 环境边界 | `memory_tasks`、`memory_experiences`、`boundary_envs` |
| Coding 真实 diff + 幂等 + 安全过滤 | `coding_memory_service.rs`、`validate_coding_revision` |
| 纯规则整合 + 相似度不自动合并 | `consolidation/src/policy.rs`、`candidates.rs` |
| 多租户沙箱 + OAuth 2.1 | `account_scope.rs`、`auth-core`/`authz` |
| 备份/恢复（认知表全覆盖） | `backup_service.rs` payload |
| 双部署 profile | `common/src/profile.rs`、`docker-compose.lite.yml` |
| 评测门禁 | `retrieval/tests/eval_bench.rs`、`consolidation/tests/consolidation_eval.rs` |

### 3.2 缺失（本次补齐）

| 编号 | 缺口 | 证据 | 处置 |
|---|---|---|---|
| G1 | 提炼即 `active`，单次正反馈即升级 | `working_memory_service.rs:216`（旧行号）、`:257` | 改为候选 + 显式 `verify` 门槛 |
| G2 | 无 `rejected` 状态与转换条件 | `domain.rs::ExperienceStatus` | 新增状态与 `ExperienceTransition` |
| G3 | 经验无版本历史 | `ExperienceStore::update_experience` | 新增 `memory_experience_versions` |
| G4 | 候选/已验证同权重 | `search_experiences_with` 仅排除 archived | 状态权重 1.0/0.6/0.3 |
| G5 | 无 Agent 记忆集合 | 全仓无 collection 模型 | 新增表、服务、5 个工具 |
| G6 | Coding 无读取面 | schema 仅 create/record | 新增 3 个只读工具 |
| G7 | 验证不强制来源 | 经验仅间接溯源 | `verify` 校验来源任务与 `validation` |
| G8 | **认知服务缺命名空间 ACL** | `WorkingMemoryService`/`CodingMemoryService`/`ChatCognitiveService` 只查 scope | 注入 `AccessChecker` 并逐方法校验 |
| G9 | 部署文档口径过期 | 轻量化报告定基于 `3048efc` | 新增 `DEPLOYMENT_MODES.md` |

### 3.3 实现不确定项

| 项 | 现状 | 说明 |
|---|---|---|
| 真实 PG 迁移 | 本地无法执行 | 本机无 docker/psql；以 CI 为唯一验证口径 |
| Docker Compose 启动 | 未实测 | 无 docker；仅静态审查 |
| 真实嵌入检索 | 未实测 | 需外部服务；Mock 结果不作语义证据 |
| Chat 抽取器 | `MockChatCognitiveExtractor` | 既有实现，不引入 LLM；本次不改 |

## 4. 必须保持不变的契约

1. `memory_*` 18 个基础工具的请求/响应字段（本次仅新增可选字段）；
2. 账户沙箱前缀 `{account_id}/{namespace}` 与 `GrantChecker` 的判定语义；
3. 备份 payload v1–v4 的可读性（本次新增 v5 字段，旧备份继续可恢复）；
4. 迁移不可重命名/删除既有列；新列必须可空或带默认值；
5. 纯规则整合默认值（`HUB_CONSOLIDATION_MODE=rules`）与“仅精确/规范化重复可自动合并”；
6. `production` 为默认 profile，Lite 必须显式选择。

## 5. 测试基线

命令（唯一事实来源）：`scripts/ci/quality-gate.sh`（fmt + clippy --all-features + test --workspace --all-features）。

> **本机环境约束（既有，见 `REPOSITORY_AUDIT.md:20-23`）**：仓库位于含空格路径
> `/Volumes/WD Drive/项目/记忆中枢`，`tikv-jemalloc-sys` 的 autotools `configure`
> 硬性拒绝含空格的前缀，因此本机**无法**执行 `--all-features`。实际执行口径为
> `cargo test --workspace` 与 `cargo clippy --all-targets -- -D warnings`（文档记录的可用命令）。
> CI 在无空格路径下执行 `--all-features`，这是两套环境唯一的口径差异。

| 环境 | 结果 |
|---|---|
| 本机 `cargo test --workspace --no-fail-fast`（首轮） | 4 个目标失败，其余通过（失败均为本次改动的连带期望，见发布文档 §4.1） |
| 本机修复后复跑 | store 56/56、worker 27/27、mcp 全部通过、`asset_governance` 11/11、工具契约快照通过 |
| 本机 `cargo clippy --all-targets -- -D warnings` | 通过，0 warning |
| 本机 `cargo fmt --all -- --check` | 通过 |
| 本机真实 PG | **NOT RUN**（无 docker/psql） |
| CI `postgres` job | 唯一可执行路径：按序 `psql` 执行迁移 + `cargo test -- --ignored --test-threads=1` |

> 本审计不把未执行/跳过的测试标记为通过。
