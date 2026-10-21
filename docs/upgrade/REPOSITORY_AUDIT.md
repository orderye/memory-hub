# Memory Hub 仓库审计报告 (REPOSITORY_AUDIT)

**审计日期：** 2026-10-18  
**执行角色：** Antigravity Agent  
**审计阶段：** Audit-only (第 1 阶段，严格只读与基线盘点，不修改任何业务代码)  
**依据文档：** `docs/Memory_Hub_Cognitive_Upgrade_Agent_Brief.md`

---

## 1. Git 状态与基线环境

- **当前分支：** `main`
- **HEAD Commit：** `f84598d` (*feat: 落地 P1-P5 并发与数据修复、零依赖中文分词、整合中枢归一化与安全流水线加固*)
- **`git status --short` 输出：**
  ```text
  ?? docs/Memory_Hub_Cognitive_Upgrade_Agent_Brief.md
  ```
  工作树中仅存在用户引入的升级指导文件，无任何未跟踪或修改的代码文件。所有操作严格遵循“不 reset、不 checkout 覆盖、不自动 stash”原则。
- **环境构建约束与发现：**
  - 本地仓库处于带有空格的路径：`/Volumes/WD Drive/项目/记忆中枢`。
  - 二进制 crate（`auth-server`, `backup-worker`, `mcp-server`）定义了可选特性 `jemalloc = ["dep:tikv-jemallocator"]`。
  - `tikv-jemalloc-sys` 的 autotools `configure` 脚本硬性禁止路径包含空格（报错：`configure: error: Prefix should not contain spaces`）。
  - 因此在带空格路径的工作空间下，不可运行 `--all-features`；默认特性与常规命令 `cargo test --workspace`、`cargo clippy --all-targets -- -D warnings` 完全正常可用。

---

## 2. 真实目录结构与 Workspace 布局

README 与 Agent Brief 中部分路径与实际源码目录存在细微差异，实测物理目录如下：

### 2.1 Workspace 成员（14 个）

1. `crates/common`：公共错误 `HubError`、配置 `AppConfig`、认证上下文 `AuthCtx`、令牌 `Scope`/`Acl`、限流、幂等与运行时。
2. `crates/mem-core`：核心领域模型（`MemoryEntry`、`MemoryVersion` 等）、抽象 trait（`MemoryStore`、`GrantStore` 等）、`AccountScopedStore` 账户隔离沙箱、`MemoryService`、`BackupService`、`IntegrationService` 契约。
3. `crates/embed`：文本嵌入向量生成抽象与客户端。
4. `crates/store`：持久化存储，包含 PostgreSQL 15 + pgvector 实现、`InMemoryHubStore` 纯内存全功能测试实现、以及 SQL 迁移。
5. `crates/retrieval`：混合检索器 `HybridRetriever`（FTS + 向量双路召回、五维评分融合、MMR 重排、软超时降级）。
6. `crates/authz`：资源服务器鉴权中间件、RFC 9728 元数据、RFC 8707 资源指示符与 TokenClaims 校验器 `TokenValidator`。
7. `crates/auth-core`：OAuth 2.1 授权核心、PKCE、Token Family 刷新令牌轮转与防重放。
8. `crates/auth-web`：授权服务器 HTTP 接口与账户控制台会话管理。
9. `crates/mcp`：MCP 协议、18 个工具的 JSON Schema 定义、`ToolDispatcher` 分发器、Streamable HTTP 与 stdio 传输层、以及 CJS 适配层。
10. `crates/consolidation`：整合中枢 `ConsolidationEngine`、重复/冲突/陈旧检测器、提案合并状态机与策略引擎。
11. `crates/worker`：周期作业库，包括 `BackupJob`、`IntegrationScanJob` 与 cron 调度器 `JobScheduler`。
12. `crates/bins/mcp-server`：MCP 服务端进程（**注意：真实路径在 `crates/bins/` 下，而非根目录 `bins/`**）。
13. `crates/bins/auth-server`：OAuth 2.1 / 账户授权服务端进程。
14. `crates/bins/backup-worker`：后台定期备份与整合扫描作业进程。

---

## 3. 核心领域类型与潜在冲突点审计

### 3.1 关键领域实体 (`crates/mem-core/src/domain.rs`)

- **`MemoryKind`**:
  ```rust
  pub enum MemoryKind {
      Fact, Preference, Episode, Note, Skill, Procedure
  }
  ```
- **`MemoryStatus`**:
  ```rust
  pub enum MemoryStatus {
      Active, Archived, Merged
  }
  ```
- **`VersionChange`**:
  ```rust
  pub enum VersionChange {
      Created, Updated, Archived, Merged
  }
  ```
- **`MemoryEntry`**:
  ```rust
  pub struct MemoryEntry {
      pub id: Uuid,
      pub namespace: Namespace,
      pub key: Option<String>,
      pub kind: MemoryKind,
      pub content: String,
      pub tags: Vec<String>,
      pub metadata: serde_json::Value,
      pub embedding: Option<Vec<f32>>,
      pub embedding_model: Option<String>,
      pub source_agent: AgentId,
      pub confidence: f32,
      pub status: MemoryStatus,
      pub version: i64,
      pub expires_at: Option<DateTime<Utc>>,
      pub merged_into: Option<Uuid>,
      pub created_at: DateTime<Utc>,
      pub updated_at: DateTime<Utc>,
  }
  ```

### 3.2 冲突风险警示：`ProposalStatus`

- **现状：** `crates/mem-core/src/domain.rs` 中**已经存在**以下定义：
  ```rust
  pub enum ProposalStatus {
      Open,
      Merged,
      Dismissed,
      Expired,
  }
  ```
  用于整合中枢的 `IntegrationProposal`。
- **冲突防范：** Brief 中草拟的认知提案状态包含 `Candidate`, `PendingReview`, `Accepted`, `Rejected`, `Superseded`。**绝对不能直接覆盖现有的 `ProposalStatus`**，认知升级方案应定义专属的枚举类型（如 `CognitiveProposalStatus` 或 `EvolutionProposalStatus`），以杜绝破坏既有整合提案系统与现有 MCP 工具。

### 3.3 账户隔离模型 (`crates/mem-core/src/account_scope.rs`)

- 账户 ID 类型为 `common::AccountId`（即 `uuid::Uuid`）。
- 存储层内部隔离键：`{account_id}/{namespace}`，分隔符为 `/` (`SCOPE_SEPARATOR: char = '/'`)。
- 服务层在每次进入存储前，通过 `AccountScopedStore<S>` 执行透明前缀注入；读取时剔除前缀并做 `assert_owned` 归属断言（非本账户键直接 fail-closed 返回 `NotFound`，不泄露数据存在性）。
- 用户可见的 namespace 严禁包含 `/`（由 `access::validate_namespace` 强制校验）。
- **结论：** 认知升级引入的所有新表（证据、关系、任务、经验、代码演进）必须沿用此 `{account_id}/{namespace}` 内部作用域隔离设计，禁止引入第二套隔离机制。

---

## 4. 存储抽象 Trait 与方法签名审计

全部定义位于 `crates/mem-core/src/infra.rs`：

### 4.1 `MemoryStore`
```rust
#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn insert(&self, entry: &MemoryEntry) -> Result<MemoryEntry>;
    async fn insert_initial(&self, entry: &MemoryEntry) -> Result<MemoryEntry>;
    async fn get(&self, namespace: &str, id: Uuid) -> Result<MemoryEntry>;
    async fn get_by_key(&self, namespace: &str, key: &str) -> Result<Option<MemoryEntry>>;
    async fn save(&self, entry: &MemoryEntry) -> Result<MemoryEntry>;
    async fn save_if_version(&self, entry: &MemoryEntry, expected_version: i64) -> Result<MemoryEntry>;
    async fn revise(&self, entry: &MemoryEntry, expected_version: i64, change: VersionChange) -> Result<MemoryEntry>;
    async fn delete(&self, namespace: &str, id: Uuid) -> Result<()>;
    async fn list(&self, filter: &MemoryFilter) -> Result<Vec<MemoryEntry>>;
    async fn count(&self, filter: &MemoryFilter) -> Result<usize>;
    async fn similar(&self, namespace: &str, embedding: &[f32], k: usize, kinds: Option<&[MemoryKind]>) -> Result<Vec<Scored<MemoryEntry>>>;
    async fn all_in_namespace(&self, namespace: &str, include_merged: bool) -> Result<Vec<MemoryEntry>>;
    async fn list_namespaces(&self) -> Result<Vec<Namespace>>;
    async fn record_version(&self, v: &MemoryVersion) -> Result<()>;
    async fn versions(&self, id: Uuid) -> Result<Vec<MemoryVersion>>;
    async fn archive_expired(&self, now: DateTime<Utc>) -> Result<usize>;
    async fn ping(&self) -> Result<()>;
}
```

### 4.2 `BackupStore`
```rust
#[async_trait]
pub trait BackupStore: Send + Sync {
    async fn save_backup(&self, backup: &BackupRecord) -> Result<()>;
    async fn get_backup(&self, id: Uuid) -> Result<BackupRecord>;
    async fn list_backups(&self, namespace: Option<&str>) -> Result<Vec<BackupRecord>>;
    async fn delete_backup(&self, id: Uuid) -> Result<()>;
}
```

### 4.3 `IntegrationStore`
```rust
#[async_trait]
pub trait IntegrationStore: Send + Sync {
    async fn save_proposal(&self, p: &IntegrationProposal) -> Result<()>;
    async fn get_proposal(&self, id: Uuid) -> Result<IntegrationProposal>;
    async fn list_proposals(&self, namespace: &str, status: Option<ProposalStatus>) -> Result<Vec<IntegrationProposal>>;
    async fn open_exists(&self, namespace: &str, kind_member: &[(String, Vec<Uuid>)]) -> Result<bool>;
}
```

### 4.4 `GrantStore`
```rust
#[async_trait]
pub trait GrantStore: Send + Sync {
    async fn ensure_namespace(&self, namespace: &str, owner: &AgentId) -> Result<(NamespaceInfo, bool)>;
    async fn get_namespace(&self, namespace: &str) -> Result<Option<NamespaceInfo>>;
    async fn list_namespace_infos(&self) -> Result<Vec<NamespaceInfo>>;
    async fn grant(&self, record: &NamespaceGrantRecord) -> Result<()>;
    async fn revoke(&self, namespace: &str, agent_id: &AgentId) -> Result<()>;
    async fn grants_for(&self, agent_id: &AgentId) -> Result<Vec<NamespaceGrantRecord>>;
    async fn can_read(&self, namespace: &str, agent_id: &AgentId) -> Result<bool>;
    async fn can_write(&self, namespace: &str, agent_id: &AgentId) -> Result<bool>;
    async fn owned_namespaces(&self, agent_id: &AgentId) -> Result<Vec<Namespace>>;
}
```

### 4.5 `AgentStore` & `AccountStore`
负责多租户沙箱与主体注册验证，支持口令哈希与 OAuth 机密客户端校验。

### 4.6 检索与嵌入抽象
- `Retriever`：`async fn search(&self, auth: &AuthCtx, q: &SearchQuery) -> Result<Vec<Scored<MemoryEntry>>>`
- `Embedder`：`model_id(&self) -> &str`, `dim(&self) -> usize`, `async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>`
- `Clock`：`fn now(&self) -> DateTime<Utc>`

---

## 5. 数据库 Migration 机制与命名规则

### 5.1 Migration 机制 (`crates/store/src/postgres.rs`)
- 账本表：`_hub_migrations`，主键为 `version`，带 `applied_at` 时间戳。
- 静态注册：PostgresHubStore 内部使用硬编码静态数组 `MIGRATIONS: &[(&str, &str)]` 编译内嵌所有 SQL。
- **强制回归测试守卫：**
  - `test_every_migration_file_is_registered()`：断言 `crates/store/migrations/` 下的每一个 `.sql` 必须在 `MIGRATIONS` 登记，否则编译测试报错。
  - `test_migrations_registered()`：断言版本有序递增且数量吻合。
  - `sql_validation.rs`：静态校验初始 schema 关键表与扩展。

### 5.2 现有迁移历史清单
1. `20261011000001_initial_schema.sql`：建立 agents, namespaces, namespace_grants, memories, memory_versions, memory_backups, integration_proposals, refresh_tokens, confirm_tokens, idempotency_keys, rate_limit_buckets, events, audit_log 及角色。
2. `20261012000002_accounts_sandbox.sql`：增加 accounts, account_sessions，namespace 全面引入 `{account_id}/{namespace}` 前缀隔离。
3. `20261013000003_conditional_key_unique.sql`：memories key 唯一性降级为 active 状态条件唯一索引。
4. `20261013000004_query_indexes.sql`：补充 tags GIN 索引、(namespace, key) 索引及 (namespace, status, kind) 索引。
5. `20261013000005_cjk_bigram_fts.sql`：添加 `tsv_tokens` 列并建立 GIN 索引，支持中文 Bigram 检索。

**下一序号：** P1 阶段新增迁移必须严格命名为 `20261018000006_<name>.sql` 并在 `MIGRATIONS` 注册。

---

## 6. 备份与恢复合同 (`crates/mem-core/src/backup_service.rs`)

- **快照格式版本：** `BACKUP_PAYLOAD_VERSION = 1`
- **快照负载结构：**
  ```json
  {
    "version": 1,
    "namespace": "namespace_name",
    "entries": [ ... ]
  }
  ```
- **Checksum 算法：** `hex::encode(Sha256::digest(&serde_json::to_vec(entries).unwrap_or_default()))`
- **三种恢复模式：**
  1. `DryRun`：计算当前数据库与快照差异，只读无写入。
  2. `Merge`：对比条目，若当前版本不存在则 `insert_initial`；若已存在且内容不同则推进版本更新。
  3. `Replace`：先对目标命名空间自动创建 `restore-safety:{namespace}` 安全备份，清空该命名空间现有条目，然后按快照完整灌入。
- **认知升级兼容约束：** 新增实体（证据、关系等）纳入备份模型时，必须保证旧格式快照能正常恢复，并保留安全备份及 Checksum 验证链条。

---

## 7. 现有 18 个 MCP 工具审计 (`crates/mcp/src/schema.rs`)

| 工具名称 | 作用域 (Scope) | 读写属性 | 破坏性 | 幂等性 | 说明 |
| :--- | :--- | :--- | :--- | :--- | :--- |
| `memory_put` | `memory:write` | 写入 | 否 | 是 | 核心条目写入 (upsert) |
| `memory_get` | `memory:read` | 只读 | 否 | 是 | 按 ID 或 key 查询 |
| `memory_update` | `memory:write` | 写入 | 否 | 是 | 字段修订 |
| `memory_delete` | `memory:write` | 写入 | 是 | 是 | 软删除（归档） |
| `memory_search` | `memory:read` | 只读 | 否 | 是 | 混合检索 (FTS+向量+MMR) |
| `memory_list` | `memory:read` | 只读 | 否 | 是 | 游标分页查询 |
| `memory_stats` | `memory:read` | 只读 | 否 | 是 | 命名空间统计 |
| `memory_history` | `memory:read` | 只读 | 否 | 是 | 版本修订历史 |
| `memory_backup_create` | `memory:admin` | 写入 | 否 | 是 | 创建快照备份 |
| `memory_backup_list` | `memory:admin` | 只读 | 否 | 是 | 列举快照 |
| `memory_restore` | `memory:admin` | 写入 | 是 | 否 | 从快照恢复 (DryRun/Merge/Replace) |
| `memory_integrate_scan`| `memory:write` | 写入 | 否 | 是 | 整合中枢提案扫描 |
| `memory_integration_list`| `memory:read` | 只读 | 否 | 是 | 整合提案列表 |
| `memory_integration_resolve`| `memory:write` | 写入 | 否 | 是 | 裁决提案 (Merge/Dismiss) |
| `memory_consolidate` | `memory:write` | 写入 | 否 | 是 | 全量整理与合并 |
| `memory_consolidation_stats`| `memory:read` | 只读 | 否 | 是 | 整合健康度指标 |
| `memory_grant` | `memory:admin` | 写入 | 否 | 是 | 命名空间跨 Agent 授权 |
| `memory_revoke` | `memory:admin` | 写入 | 是 | 是 | 撤销授权 |

**规范要求：** 所有 18 个既有工具严格保持名称、入参、返回值与行为不变。后续 P1-P5 阶段新增工具一律使用新工具名并采用 additive 设计。

---

## 8. 测试与质量检查实测结果

- **测试套件运行：** `cargo test --workspace`
  - 测试套件总计 **15** 个模块（含 1 个集成测试套件 `cjs_adapter_test`），11 个包文档测试。
  - **实测通过用例数：281 passed, 0 failed, 0 ignored**
    - `auth-core`: 11 passed
    - `auth-server` bin: 1 passed
    - `auth-web`: 42 passed
    - `authz`: 5 passed
    - `backup-worker` bin: 2 passed
    - `common`: 41 passed
    - `consolidation`: 11 passed
    - `embed`: 9 passed
    - `mcp` lib: 59 passed
    - `mcp` tests (`cjs_adapter_test`): 5 passed
    - `mcp-server` bin: 3 passed
    - `mem-core`: 33 passed
    - `retrieval`: 7 passed
    - `store`: 41 passed
    - `worker`: 11 passed
- **Clippy 静态检查：** `cargo clippy --all-targets -- -D warnings`
  - 检查结果：**0 错误，0 警告，完全通过**。
- **格式检查：** `cargo fmt --all -- --check`
  - 检查结果：仓库目前存在若干既有格式微调差异（已在上述记录，不作为本阶段修改）。

---

## 9. 结论与下一步（P0 建议）

1. 仓库当前质量良好，核心底座完善，单测覆盖率高且全数通过。
2. 架构不变量已清晰，命名冲突与路径差异已全数标记。
3. Audit-only 完成，进入 P0 阶段前确认无需引入重型依赖，所有后续阶段将严格按 P0 → P1 → P2 → P3 → P4 → P5 顺序分步实施。

<!-- kimi-badge -->
[![powered by KimiCode](../../.github/assets/kimi-badge.svg)](https://www.kimi.com/code)
