# 治理升级发布与回滚指南

[![powered by KimiCode](../../.github/assets/kimi-badge.svg)](https://www.kimi.com/code)

> 对应代码基线：`main` @ `92a809d` 之后的治理升级变更集（未提交）。
> 配套：`GOVERNANCE_AUDIT.md`（审计）、`ASSET_GOVERNANCE.md`（设计）、`DEPLOYMENT_MODES.md`（部署）。

---

## 1. 变更摘要

| 阶段 | 内容 | 行为是否变化 |
|---|---|---|
| P0 | 源码审计、基线口径、ACL 缺口定位 | 否 |
| P1 | 经验资产生命周期（candidate→verify→active）+ 版本历史 + 检索权重 | **是**（提炼不再直接 active） |
| P1 | 认知服务（Working/Coding/Chat）补命名空间 ACL | **是**（未授权命名空间由可用变拒绝） |
| P1 | Coding 只读工具（线程/修订列表与单条读取） | 否（纯新增） |
| P2 | Agent 记忆集合与绑定（5 个管理工具 + 检索可选 `collection`） | 否（纯新增，默认不启用） |
| P2 | 移除 `working_experience_feedback.status` 直写通道 | **是**（旧参数收到明确错误） |
| P3 | 备份负载 v5：经验版本历史纳入快照/恢复 | 否（旧备份仍可恢复） |
| P4 | 部署模式文档、配置项、README 工具清单（28 → 38） | 否 |

---

## 2. 文件级变更清单

### 新增

| 文件 | 作用 |
|---|---|
| `crates/store/migrations/20261028000016_experience_asset_lifecycle.sql` | 经验状态约束、版本列、版本表、来源计数回填 |
| `crates/store/migrations/20261028000017_memory_collections.sql` | 集合与绑定两表 |
| `crates/mem-core/src/collection_service.rs` | 集合 CRUD / 绑定 / 解析 |
| `crates/mem-core/tests/asset_governance.rs` | 生命周期、ACL、集合、Coding 链回归 |
| `crates/store/src/postgres/collections.rs` | 集合 PG 实现 |
| `crates/store/src/in_memory/collections.rs` | 集合内存实现 |
| `crates/mcp/src/tools/collections.rs` | 5 个集合工具 |
| `docs/upgrade/GOVERNANCE_AUDIT.md` / `ASSET_GOVERNANCE.md` / `DEPLOYMENT_MODES.md` / `GOVERNANCE_RELEASE.md` | 审计、设计、部署、发布 |

### 修改（按层）

- **领域**：`mem-core/src/domain.rs`（`Rejected`、`ExperienceTransition`、`ExperienceVersion`、`CollectionFilter`、`MemoryCollection`、`RestoreReport.failed_experience_versions`）
- **存储契约**：`mem-core/src/infra.rs`（`CollectionStore`、`ExperienceStore` 版本化方法、`MemoryHubStore` 组成）
- **服务**：`working_memory_service.rs`（候选提炼、`transition`、版本快照、状态权重、ACL）、`coding_memory_service.rs`（ACL）、`chat_cognitive_service.rs`（ACL）、`backup_service.rs`（v5）、`account_scope.rs`（集合转发与账户校验）
- **存储实现**：`postgres/{mod,working}.rs`、`in_memory/{mod,working}.rs`、`mem-core/testing.rs`
- **协议**：`mcp/src/schema.rs`、`mcp/src/tools/{mod,memory,working,coding}.rs`
- **配置与装配**：`common/src/config.rs`、`bins/mcp-server/src/main.rs`、`.env.example`
- **文档**：`README.md`、`README.en.md`、`docs/README.md`、`docs/upgrade/{WORKING_MEMORY,CODING_MEMORY,EVALUATION,CONSOLIDATION_AB}.md`、新增 `docs/Memory-Hub-认知记忆与知识资产治理升级-任务书与完成情况.md`
- **测试**：`mcp/src/tools/tests.rs`、`mcp/tests/tool_schema_contract.rs`、`mem-core/src/backup_service/tests.rs`

### 未修改（保持原样）

OAuth 2.1 流程、令牌族、账户沙箱前缀规则、整合策略引擎、检索打分公式、Dockerfile / compose 结构。

---

## 3. 数据库迁移

执行顺序：`016` → `017`（按文件名排序，和既有 CI 一致）。

### 016 经验资产生命周期

- 重建 `memory_experiences.status` 约束：新增 `rejected`；
- 新增列：`version`、`superseded_by`、`verified_at`、`verified_by`、`verification_note`、`status_reason`、`source_count`；
- 新建 `memory_experience_versions`（`experience_id + version` 唯一，只追加）；
- 回填 `version = 1`、`source_count = distinct(task_id)`（可重复执行）。

**不可逆点**：无。下行 SQL：

```sql
drop table if exists memory_experience_versions;
alter table memory_experiences drop constraint if exists memory_experiences_status_check;
update memory_experiences set status = 'deprecated' where status = 'rejected';
alter table memory_experiences
  add constraint memory_experiences_status_check
  check (status in ('candidate', 'active', 'deprecated', 'archived'));
alter table memory_experiences
  drop column if exists source_count,
  drop column if exists status_reason,
  drop column if exists verification_note,
  drop column if exists verified_by,
  drop column if exists verified_at,
  drop column if exists superseded_by,
  drop column if exists version;
```

### 017 Agent 记忆集合

新增 `memory_collections` / `memory_collection_bindings`。下行 SQL：

```sql
drop table if exists memory_collection_bindings;
drop table if exists memory_collections;
```

两表都不含记忆内容；删除不触碰 `memories` / `memory_experiences` 等表。

### 迁移失败处理

- CI 以 `psql -v ON_ERROR_STOP=1` 逐文件执行，失败即中断；
- 016 的约束重建放在列变更之前，失败时原约束仍在（要么完整成功，要么保持旧状态）；
- 大规模历史数据回填只有两条 `update`，按主键关联，可重复执行。

---

## 4. 测试报告

命令与环境见下（结果见 §4.1）：

```bash
# 本机：静态 + 单测 + 集成（内存后端）
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace

# 本机无法执行（仓库路径含空格，jemalloc configure 拒绝）：
#   scripts/ci/quality-gate.sh 的 --all-features 口径
# 真实 PostgreSQL（本机无 docker / psql）：
#   CI postgres job：psql 按序执行迁移 + cargo test -- --ignored --test-threads=1
```

### 4.1 本次运行记录

环境：macOS 26.0.1 / arm64，`cargo 1.98.1`；仓库位于含空格路径，`--all-features` 不可用（见 §4 说明）。

| 命令 | 结果 | 备注 |
|---|---|---|
| `cargo test --workspace --no-fail-fast`（首轮） | **4 个目标失败**，其余目标全部通过 | 全量含 doctest；失败项均为本次改动引起的测试期望/测试替身不匹配 |
| `cargo test -p mcp -p store -p worker --lib --no-fail-fast`（修复后） | **通过**：store 56/56、worker 27/27、mcp 全部通过（exit 0） | |
| `cargo test -p mem-core --test asset_governance` | **11/11 通过** | 生命周期、ACL、集合、Coding 链 |
| `cargo test -p mcp --test tool_schema_contract` | **通过**（38 工具黄金快照） | 快照已按新契约刷新并人工核对 |
| `cargo clippy --all-targets -- -D warnings` | 见 §4.2 | |
| `cargo fmt --all -- --check` | 见 §4.2 | |

首轮失败与修复（全部为本次改动的连带影响，非既有缺陷）：

1. `store`：`test_every_migration_file_is_registered` / `test_migrations_registered` —— 016/017 未登记、数量仍为 15；已登记并更新为 17。
2. `worker`：`test_scheduled_backup_is_v4_payload_and_restorable` —— 断言 v4；备份负载已升为 v5，断言同步更新。
3. `mcp`：`test_discover_and_tools_list` —— 工具数断言 28；更新为 38。
4. `mcp`：`test_grant_and_revoke_via_tools` —— 撤销授权后 `memory_search` 必须报错；集合扇出的 `skipped` 语义误伤了单命名空间路径。已改为：**只有集合检索才降级为 skipped，显式单命名空间一律 fail closed**。
5. `mem-core`：`asset_governance` 三项 —— 测试用 `TestHubStore::register_agent` 故意返回 `Unavailable`；测试改为直接种入 `AgentRecord`（验证目标不变）。

### 4.2 静态门禁

| 命令 | 结果 | 耗时 |
|---|---|---|
| `cargo clippy --all-targets -- -D warnings` | **通过（0 warning）** | 12m 09s |
| `cargo fmt --all -- --check` | **通过** | < 3s |

> 两条命令均未加 `--all-features`：本机路径含空格，`tikv-jemalloc-sys` 的 configure 会直接失败（`REPOSITORY_AUDIT.md:20-23` 已记录）。CI 在无空格路径下执行 `--all-features`。
>
> **NOT RUN**：
> - `scripts/ci/quality-gate.sh` 的 `--all-features` 口径（空格路径 + jemalloc configure 拒绝）；
> - 真实 PostgreSQL 迁移与集成测试（本机无 docker/psql）；
> - `docker compose` 启动（本机无 docker）；
> - 真实 embedding 检索效果（需外部服务）；
> - **改造前（HEAD）基线未在本机复跑**：工作区已改动，无法在不干扰改动的情况下取干净基线。本报告给出的是改造后的完整结果与首轮失败→修复→复跑的记录；未把任何未执行项标记为通过。

---

## 5. 兼容性与行为变更

1. **提炼状态**：`working_experience_distill` 返回 `candidate`，不再直接 `active`。需要“立即可用”的调用方应显式调用 `working_experience_transition(action="verify")`。
2. **反馈工具**：`working_experience_feedback` 不再接受 `status`；传了会收到 `InvalidArgument`，提示改用 `transition`。
3. **认知服务 ACL**：Working/Coding/Chat 的读写现在要求命名空间 grant，与 `memory_*` 工具口径一致。
4. **备份格式**：新备份为 v5；新代码可恢复 v1–v5，**旧代码不能恢复 v5**（见 §6.2）。
5. **工具数量**：28 → 38，全部为新增；`memory_search` / `working_experience_search` 的 `namespace` 改为可选（与 `collection` 二选一），旧调用不受影响。

---

## 6. 回滚

### 6.1 代码

按阶段独立提交，可单独 revert。若尚未提交，按文件分组还原即可（新增文件直接删除；修改文件 `git checkout -- <path>`）。

### 6.2 数据与备份

- **先备份再迁移**：`memory_backup_create`（`namespace="*"`）并核对校验和；
- 若已产生 v5 备份，**回滚代码后这些备份无法由旧二进制恢复**（旧代码只认 v1–v4 校验和）。
  需要回滚时，先保留新二进制用于恢复，或在新代码下恢复后再降级；
- 016 下行前必须把 `rejected` 改成 `deprecated`（旧状态机不识别 `rejected`）；
- 集合表可在旧代码下继续存在（旧代码不读），不影响运行。

### 6.3 人工步骤

1. 停止写入（维护窗口）；
2. 全量备份并验证；
3. 执行下行 SQL（§3）；
4. 部署旧二进制；
5. 抽样验证：`memory_stats`、一条 Chat/Working/Coding 读写、一次 `dry_run` 恢复。

---

## 7. 遗留问题与未覆盖

| 项 | 状态 | 说明 / 风险 |
|---|---|---|
| Wiki / CodeGraph | 未实现 | 任务书允许排除；无适配器 |
| 真实 PG 迁移与集成测试 | **NOT RUN (local)** | 本机无 docker/psql；需 CI 执行 |
| `--all-features` | **BLOCKED (local)** | 仓库路径含空格，jemalloc configure 拒绝；CI 无此问题 |
| Docker Compose 启动 | **NOT RUN (local)** | 无 docker；仅静态审查 |
| 真实 embedding 语义效果 | **NOT RUN** | 需外部服务；Mock 不作语义证据 |
| 回滚链显式标记 | 设计取舍 | 用修订链（commit/parent_sha/文件）表达，未加 `reverts_revision_id` 列 |
| `AccessChecker` 未注入 | 已知边界 | 仅测试/纯库嵌入；生产二进制始终注入 |
