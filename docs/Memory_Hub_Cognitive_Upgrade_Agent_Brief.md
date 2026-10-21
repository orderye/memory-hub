# Memory Hub Cognitive Memory Upgrade — Agent Execution Brief

**版本：** 1.0  
**基线日期：** 2026-10-18  
**目标仓库：** https://cnb.cool/toroyxxx/memory-hub  
**执行方式：** 按 P0 → P1 → P2 → P3 → P4 → P5 顺序分阶段实施；每阶段独立提交、独立验收。  
**关键要求：** 先审阅当前工作树和实际源码，再修改。不得假设 README 中的描述完全等于实现，不得虚构文件、trait、API、migration 编号或命令。

---

## 0. 仓库基线与不可违反的约束

### 0.1 已从公开仓库确认的结构

当前 `main` README 显示项目是 Rust workspace，核心 crate/组件包括：

- `crates/common`
- `crates/mem-core`
- `crates/consolidation`
- `crates/store`
- `crates/embed`
- `crates/retrieval`
- `crates/authz`
- `crates/auth-core`
- `crates/auth-web`
- `crates/mcp`
- `crates/worker`
- `bins/mcp-server`
- `bins/auth-server`
- `bins/backup-worker`
- `docs/设计-多智能体长期记忆服务.md`
- 根目录 `Cargo.toml`、`Cargo.lock`、`README.md`、`docker-compose.yml`、`.env.example`

公开 README 描述的能力包括 PostgreSQL 15 + pgvector、内存存储用于开发/测试、FTS + 向量并行召回、五维评分、MMR、18 个 MCP 工具、OAuth 2.1、多账户沙箱、namespace ACL、版本历史、快照备份与三种恢复模式、整合提案与审计。

公开 README 的基线命令：

```bash
cargo test --workspace --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all
cargo test -p store --features pg-integration -- --ignored
```

PostgreSQL 集成测试需要 `DATABASE_URL`。

**注意：** 浏览器可见的目录页面没有完整展开每个 crate 内部的文件清单。因此，下文中只有以上公开确认的路径可视为现存路径。其他文件路径均是“先定位真实实现后再修改”的任务，不得直接创建重复实现。

### 0.2 必须保持的架构不变量

1. 保留现有 Rust workspace 和三个进程：`mcp-server`、`auth-server`、`backup-worker`。除非性能数据证明必要，不增加常驻微服务。
2. 保留现有 MCP 工具及参数兼容性；新增工具必须使用新名称，不改变既有 schema 的必填字段和语义。
3. 保留 OAuth 2.1、PKCE、令牌内省/撤销、`aud` 绑定、账户 claim、scope 检查、账户隔离、namespace ACL 和授权撤销语义。
4. 所有新表都必须具有账户/namespace 隔离方案，并由服务层统一构造授权后的访问上下文；禁止模型层或检索层绕过 ACL。
5. 保留 PostgreSQL 15 + pgvector 和内存 store。新能力不能只在 PostgreSQL 实现而令内存测试失效；若功能确实依赖 PostgreSQL，必须明确标注并提供集成测试。
6. 保留 `memory_versions`、`memory_backups`、恢复前安全备份、校验和验证、dry-run、merge、replace 等既有语义。
7. 现有 `MemoryEntry` 和现有 MCP `memory_put` / `memory_update` / `memory_get` / `memory_search` 等接口必须向后兼容。
8. 任何 LLM 输出都只能先成为候选/提案。LLM 不得直接执行 SQL、直接授予权限、直接删除数据或绕过 `Manual` / `SemiAuto` / `FullAuto` 治理策略。
9. 原始证据与派生认知分离；认知提炼不得覆盖或销毁原始事实。
10. 迁移必须是增量、可重复部署、可审计；严禁未经备份的 destructive migration。
11. 不强制引入图数据库、不强制引入 Hy-Memory Python 运行时、不强制部署 30B 模型。
12. 不得把“认知分层”简单实现成一个无约束 JSON 字段；关键关系、状态和时间字段应可查询、可索引、可验证。

---

## 1. Agent 首轮操作：审阅后再动手

在任何代码修改前执行并记录输出：

```bash
git status --short
git branch --show-current
git log -5 --oneline --decorate
find crates bins docs -maxdepth 4 -type f | sort
find crates/store -maxdepth 5 -type f | sort
rg -n "trait (MemoryStore|BackupStore|IntegrationStore|GrantStore|AgentStore|Embedder|Retriever|Clock)|struct MemoryEntry|struct AuthCtx|memory_versions|memory_backups|integration_proposals" crates bins
rg -n "CREATE TABLE|ALTER TABLE|CREATE INDEX|migration|migrations" crates/store
rg -n "memory_put|memory_update|memory_search|memory_backup_create|memory_restore|memory_consolidate" crates/mcp crates/mem-core
```

然后：

1. 阅读根 `Cargo.toml`、每个相关 crate 的 `Cargo.toml`。
2. 阅读 `crates/mem-core` 领域模型、服务层、账户范围 store 包装、现有 service tests。
3. 阅读 `crates/store` 的 PostgreSQL 和内存实现、migration runner、全部迁移 SQL。
4. 阅读 `crates/retrieval` 的候选召回、打分、MMR、超时降级。
5. 阅读 `crates/consolidation` 的提案状态机、merge 事务与溯源。
6. 阅读 `crates/mcp` 工具注册、JSON schema、错误映射、scope 标注。
7. 阅读 `crates/worker` 调度、幂等、重试与任务记录方式。
8. 阅读 `crates/authz` / `auth-core` / `auth-web` 的 claims、scope、account isolation 与 revoke 测试。
9. 阅读备份序列化格式、checksum、restore 流程和兼容性测试。
10. 将真实路径、现有类型、migration 命名规则、存储 trait 方法签名写入 `docs/upgrade/REPOSITORY_AUDIT.md`（目录不存在时才创建 `docs/upgrade/`）。

如果公开仓库与本地工作树不同，以当前用户工作树为准；先报告差异，不要覆盖用户未提交修改。若 `git status` 非空，除非用户明确授权，否则不重置、不 checkout 覆盖、不 stash 自动处理。

---

## 2. 目标设计概览

增加三条场景管线，但共享现有存储、安全、版本、审计、备份和检索底座：

- **Chat**：原始证据 → 原子事实 → 事件/摘要 → 稳定模式与前瞻意图。
- **Working**：每个已结束任务的事实记录 + 0..N 条经过价值门槛验证的可复用经验。
- **Coding**：Working 双层记忆 + 真实 Git diff/工具变更构成的代码演进链。

建议将认知层先实现为 Rust library modules 和现有 `backup-worker` 的作业类型，而不是新进程。只有在真实负载证明需要独立扩容/隔离后，才考虑拆分服务。

### 2.1 建议的领域类型（须按现有命名和错误类型适配）

以下是目标 API 形状，不代表这些类型当前已存在。实施前检查命名冲突并遵循现有 serde、UUID、时间、错误、审计约定。

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MemoryScene {
    Chat,
    Working,
    Coding,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CognitiveLayer {
    Raw,
    Fact,
    Summary,
    Identity,
    Knowledge,
    Schema,
    Intention,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MemoryRelationKind {
    Supports,
    Contradicts,
    Supersedes,
    DerivedFrom,
    RelatedTo,
    DerivedExperience,
    CodeRevision,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProposalStatus {
    Candidate,
    PendingReview,
    Accepted,
    Rejected,
    Superseded,
}

pub struct MemoryEvidence {
    pub id: Uuid,
    pub account_id: AccountId,       // 使用项目现有 ID 类型
    pub namespace: String,
    pub source_kind: EvidenceSourceKind,
    pub source_ref: Option<String>,  // 会话/任务/工具调用/Git commit 的引用
    pub content_hash: String,
    pub excerpt: Option<String>,     // 经脱敏的最小必要片段，不默认复制完整对话
    pub occurred_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

pub struct MemoryRelation {
    pub from_memory_id: Uuid,
    pub to_memory_id: Uuid,
    pub relation: MemoryRelationKind,
    pub confidence: f32,
    pub source_evidence_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

pub struct MemoryTask {
    pub id: Uuid,
    pub namespace: String,
    pub scene: MemoryScene,
    pub task: String,
    pub status: TaskStatus,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub result_summary: Option<String>,
}

pub struct MemoryExperience {
    pub id: Uuid,
    pub namespace: String,
    pub task: String,
    pub search_keys: Vec<String>,
    pub solution: String,
    pub preconditions: serde_json::Value,
    pub boundary_envs: Vec<String>,
    pub failure_modes: Vec<String>,
    pub validation: serde_json::Value,
    pub confidence: f32,
    pub status: ExperienceStatus,
    pub source_task_ids: Vec<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub struct CodingThread {
    pub id: Uuid,
    pub namespace: String,
    pub repository_id: String,
    pub branch_name: Option<String>,
    pub file_path: String,
    pub symbol_name: Option<String>,
    pub region_fingerprint: Option<String>,
}

pub struct CodingRevision {
    pub id: Uuid,
    pub thread_id: Uuid,
    pub commit_sha: Option<String>,
    pub parent_revision_id: Option<Uuid>,
    pub diff_text: String,
    pub diff_hash: String,
    pub user_intent: Option<String>,
    pub validation: serde_json::Value,
    pub occurred_at: DateTime<Utc>,
}
```

类型字段应遵循实际项目风格；例如 `AccountId` 若不存在，应复用真实 `AuthCtx`/UUID 类型，不要为此复制一套账户 ID。对外响应不得暴露内部账户前缀或跨 namespace 元数据。

### 2.2 目标 trait（不要复制现有 trait）

先检查现有 `MemoryStore`、`BackupStore`、`IntegrationStore`、`GrantStore`、`AgentStore`、`Embedder`、`Retriever`、`Clock`。尽量扩展现有领域服务，只有职责明确且实现可替换时才增加新 trait。

可参考的接口形状：

```rust
#[async_trait]
pub trait EvidenceStore: Send + Sync {
    async fn append_evidence(
        &self,
        ctx: &AuthCtx,
        input: NewEvidence,
    ) -> Result<MemoryEvidence, HubError>;

    async fn get_evidence(
        &self,
        ctx: &AuthCtx,
        id: Uuid,
    ) -> Result<MemoryEvidence, HubError>;
}

#[async_trait]
pub trait RelationStore: Send + Sync {
    async fn list_relations(
        &self,
        ctx: &AuthCtx,
        memory_id: Uuid,
    ) -> Result<Vec<MemoryRelation>, HubError>;

    async fn add_relation(
        &self,
        ctx: &AuthCtx,
        relation: NewMemoryRelation,
    ) -> Result<MemoryRelation, HubError>;
}

#[async_trait]
pub trait CognitiveProcessor: Send + Sync {
    async fn propose_chat_memories(
        &self,
        input: CognitiveInput,
    ) -> Result<Vec<MemoryProposalDraft>, CognitiveError>;

    async fn propose_working_experiences(
        &self,
        input: WorkingTaskInput,
    ) -> Result<Vec<MemoryExperienceDraft>, CognitiveError>;
}
```

约束：

- `CognitiveProcessor` 不得直接写 store；它只产出候选。
- `ctx` 必须由已验证令牌生成，不能由 MCP 参数传入或由模型生成。
- 新增 trait 要同时提供 PostgreSQL 和内存实现，或者明确说明为何不适用并测试相应降级。
- 错误必须映射到项目现有错误类型；不得新增吞错或把权限错误转成空结果的路径。
- 模型调用应通过注入的 provider 接口，避免在领域 crate 内硬编码 HTTP 客户端、密钥或供应商。

---

## 3. 数据库迁移草案

**重要：** 下列 SQL 是迁移设计草案，不可在审阅现有 migration、实际列名、账户/namespace 内部键策略、ID 类型和 migration runner 之前直接执行。Agent 必须根据现有规范新建下一编号迁移，并确保所有外键和唯一约束符合真实 schema。若现有 schema 用 `{account_id}/{namespace}` 内部键，必须沿用该隔离模型；不要引入第二套命名空间编码。

建议迁移目标：新增证据、关系、任务、经验、Coding thread/revision、候选提案表。若当前已有等价表，则复用/增量扩展，不得重复建表。

```sql
-- DRAFT ONLY: adapt identifiers, FK targets, types, schema and migration filename
-- after reading the current crates/store migration history.

CREATE TABLE IF NOT EXISTS memory_evidence (
    id UUID PRIMARY KEY,
    namespace TEXT NOT NULL,
    source_kind TEXT NOT NULL,
    source_ref TEXT,
    content_hash TEXT NOT NULL,
    excerpt TEXT,
    occurred_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_memory_evidence_namespace_created
    ON memory_evidence (namespace, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_memory_evidence_source_ref
    ON memory_evidence (namespace, source_kind, source_ref);

CREATE TABLE IF NOT EXISTS memory_relations (
    from_memory_id UUID NOT NULL,
    to_memory_id UUID NOT NULL,
    relation_kind TEXT NOT NULL,
    confidence REAL NOT NULL DEFAULT 1.0,
    source_evidence_id UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (from_memory_id, to_memory_id, relation_kind),
    CHECK (confidence >= 0.0 AND confidence <= 1.0),
    CHECK (from_memory_id <> to_memory_id)
);

CREATE INDEX IF NOT EXISTS idx_memory_relations_to
    ON memory_relations (to_memory_id, relation_kind);

CREATE TABLE IF NOT EXISTS memory_tasks (
    id UUID PRIMARY KEY,
    namespace TEXT NOT NULL,
    scene TEXT NOT NULL CHECK (scene IN ('working', 'coding')),
    task TEXT NOT NULL,
    status TEXT NOT NULL,
    started_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at TIMESTAMPTZ,
    result_summary TEXT,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb
);

CREATE INDEX IF NOT EXISTS idx_memory_tasks_namespace_started
    ON memory_tasks (namespace, started_at DESC);

CREATE TABLE IF NOT EXISTS memory_experiences (
    id UUID PRIMARY KEY,
    namespace TEXT NOT NULL,
    task TEXT NOT NULL,
    search_keys TEXT[] NOT NULL DEFAULT '{}',
    solution TEXT NOT NULL,
    preconditions JSONB NOT NULL DEFAULT '{}'::jsonb,
    boundary_envs TEXT[] NOT NULL DEFAULT '{}',
    failure_modes TEXT[] NOT NULL DEFAULT '{}',
    validation JSONB NOT NULL DEFAULT '{}'::jsonb,
    confidence REAL NOT NULL DEFAULT 0.5,
    status TEXT NOT NULL DEFAULT 'candidate',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (confidence >= 0.0 AND confidence <= 1.0)
);

CREATE INDEX IF NOT EXISTS idx_memory_experiences_namespace_status
    ON memory_experiences (namespace, status, updated_at DESC);

CREATE TABLE IF NOT EXISTS memory_task_experiences (
    task_id UUID NOT NULL,
    experience_id UUID NOT NULL,
    relation_kind TEXT NOT NULL DEFAULT 'derived',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (task_id, experience_id, relation_kind)
);

CREATE TABLE IF NOT EXISTS coding_threads (
    id UUID PRIMARY KEY,
    namespace TEXT NOT NULL,
    repository_id TEXT NOT NULL,
    branch_name TEXT,
    file_path TEXT NOT NULL,
    symbol_name TEXT,
    region_fingerprint TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_coding_threads_lookup
    ON coding_threads (namespace, repository_id, file_path, branch_name);

CREATE TABLE IF NOT EXISTS coding_revisions (
    id UUID PRIMARY KEY,
    thread_id UUID NOT NULL REFERENCES coding_threads(id) ON DELETE CASCADE,
    commit_sha TEXT,
    parent_revision_id UUID REFERENCES coding_revisions(id),
    diff_text TEXT NOT NULL,
    diff_hash TEXT NOT NULL,
    user_intent TEXT,
    validation JSONB NOT NULL DEFAULT '{}'::jsonb,
    occurred_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (thread_id, diff_hash)
);

CREATE INDEX IF NOT EXISTS idx_coding_revisions_thread_time
    ON coding_revisions (thread_id, occurred_at DESC);
```

### 3.1 SQL 草案必须完成的修正

上面是逻辑结构示例，正式迁移前必须做到：

1. **外键与账户隔离**：给每张新表添加项目实际使用的 namespace/account 约束。不能只凭 `namespace TEXT` 做跨账户隔离。如果真实架构用带账户前缀的内部 namespace key，所有查询必须经现有 `AccountScopedStore` 或等价作用域层。
2. **引用完整性**：为 `memory_relations` 的 memory ID 和 evidence ID 添加与真实主表类型匹配的外键；为 `memory_task_experiences` 添加任务和经验外键。
3. **审计**：新增/修改/裁决记录沿用现有 `events` / `audit_log` 写入方式，不建立绕开现有审计的新通道。
4. **版本和备份**：确定新表是否属于 namespace snapshot payload；必须将它们纳入备份/恢复或明确证明可从已备份数据无损重建。不能让 restore 后出现孤儿关系。
5. **索引**：根据真实查询计划添加必要索引；不要盲目为每个 JSON 字段建索引。
6. **幂等迁移**：遵循项目已有迁移框架；`IF NOT EXISTS` 不能替代迁移版本管理。
7. **锁与停机**：大表回填采用分批策略；新索引在生产环境是否需要 `CONCURRENTLY` 应根据迁移 runner 是否在事务中执行决定。
8. **回滚**：新增表可以通过 down migration 删除，但只有在确认没有新数据或数据已导出后才允许执行；生产默认 forward-fix，不自动 destructive rollback。
9. **内存 store**：补齐相同语义的 in-memory 存储实现，避免只在 PostgreSQL 通过。
10. **数据回填**：先保持现有 `MemoryEntry` 原样；旧条目只在证据可追溯时建立关联，禁止凭空生成 evidence。

---

## 4. P0 — 基线测试与安全回归

### 目标

建立升级前可重复的质量基线，确认安全、备份、授权、检索和现有 API 没有回退。

### 文件级任务清单

仅修改实际存在的文件：

- `Cargo.toml` / `Cargo.lock`：只有确有必要时才改依赖；默认不新增大型依赖。
- 真实存在的 `crates/*/tests/`、`#[cfg(test)]` 测试模块：补充回归用例。
- 真实存在的 `crates/store` migration/测试夹具：加入 schema 快照或约束测试。
- `docs/upgrade/REPOSITORY_AUDIT.md`：记录实际路径、trait、迁移编号、基线命令和现存风险。
- `docs/upgrade/BASELINE.md`：记录测试、clippy、格式、PG 集成、备份恢复和安全测试结果。

### 必测用例

- Account A / Account B 的同名 namespace 隔离。
- 未授权 Agent 读、写、list、search、backup、restore 均按既有合同拒绝。
- namespace grant revoke 后，后续请求不能继续访问。
- token 缺少 account claim、错误 `aud`、无对应 scope 时拒绝。
- `memory_put` upsert、`memory_update` 版本递增、`memory_history` 顺序与幂等。
- 备份 checksum 损坏时 restore 拒绝。
- `replace` 失败后安全备份仍可恢复。
- restore `dry_run` 不产生写入。
- consolidation `dry_run` 不落库；重复 scan 不刷出重复 open proposal。
- embedder 故障时写入/检索降级符合当前约定。
- FTS / vector 任一路超时的降级路径可测试。
- MCP 18 个现有工具 schema、scope、错误码和 `additionalProperties` 行为不变。
- 运行所有现有测试并记录准确数量，不把 README 中的历史测试数量当成当前实测结果。

### 验收命令

```bash
cargo fmt --all -- --check
cargo test --workspace --all-features
cargo clippy --all-targets --all-features -- -D warnings
```

如果本地 PostgreSQL 测试环境已配置：

```bash
cargo test -p store --features pg-integration -- --ignored
```

若测试环境名称/feature 与实际仓库不同，以 Cargo manifest 和 CI 为准，并在 `BASELINE.md` 记录调整理由。

### 失败策略

P0 不得改变生产行为。若任一现有安全、备份或兼容测试失败，先修复基线问题或明确标记为已存在缺陷；禁止在基线不清晰时继续做 schema 变更。

---

## 5. P1 — 证据、时间与关系模型

### 目标

建立可追溯、可演化的认知数据基础，同时不破坏现有 `MemoryEntry` 和 API。

### 文件级改造定位

1. 在 `crates/mem-core` 中找到现有领域模型文件（README 设计文档提及 `mem-core/domain.rs`，但必须以实际文件为准），为 scene/layer/status/relation/evidence 增加兼容类型。
2. 在 `crates/store` 中按实际 migration runner 创建下一编号的增量迁移。
3. 在 `crates/store` 的 PostgreSQL 与内存实现中扩展新表读写。
4. 在 `crates/mem-core` 服务层增加 evidence/relation 的授权后访问方法。
5. 在 `crates/common` 复用现有 `AuthCtx`、ID、错误、Clock 和审计类型。
6. 在 `crates/worker` 中只增加必要的清理/修复作业，不增加新常驻进程。
7. 在 `crates/mcp` 暂不修改旧工具；如确需外部访问证据，新增只读工具并明确 scope 与 schema。
8. 在备份 payload、checksum、restore、prune 相关的真实实现中纳入新实体及引用完整性。
9. 为所有新实体增加 PG 与内存 store tests。

### 领域语义

- 原始证据 append-only；更正通过新证据和关系表达，不原位覆盖。
- 时间字段区分 `occurred_at`（事件发生时间）、`created_at`（系统记录时间）和 `valid_from` / `valid_until`（事实有效期）。
- `confidence` 是模型/规则对抽取的置信度，不是事实客观真实性。
- `supersedes` 表示新事实取代旧事实；旧事实保留并可检索为历史，但默认不作为当前有效事实。
- `contradicts` 表示未裁决的矛盾；不能自动选择较新的事实，除非领域策略明确允许。
- 所有关系两端必须属于当前可访问的 namespace/account scope；拒绝跨 namespace 关系。
- 原文片段默认最小化、脱敏，并遵循现有日志脱敏和保留策略。

### 验收

- migration 可在全新数据库执行。
- migration 可在现有数据库执行，不改写已有记忆正文或版本。
- 备份 → 清空测试库 → restore 后，新旧实体及关系完整。
- 内存 store 与 PG store 的领域语义一致。
- 越权 evidence ID、relation ID 返回与现有服务一致的 NotFound/Permission 错误，不泄露其他账户对象是否存在。
- 旧 MCP 客户端不需要升级即可继续调用旧工具。

---

## 6. P2 — Chat 认知记忆

### 目标

在现有基础存取之上提供 Chat 认知提炼：事实、摘要、稳定模式和前瞻意图。先用简单层级与提案状态机，不先引入图数据库。

### 文件级改造定位

- `crates/mem-core`：实现 Chat scene/layer 领域规则、有效期与 superseded 状态处理。
- `crates/consolidation`：复用现有提案、冲突检测和裁决机制；增加“事实演化”策略，不复制第二套合并引擎。
- `crates/embed`：仅在现有 provider 抽象需要时增加接口，不将 LLM 写入逻辑塞进 embedder。
- `crates/worker`：增加可重试、幂等的认知提炼 job。
- `crates/mcp`：如果要公开功能，新增 `memory_chat_propose` / `memory_cognitive_review` 等新工具前，先确认旧工具的命名和 schema；也可先只用现有 `memory_put` 与 proposal 工具实现内部闭环。
- `docs/upgrade/CHAT_MEMORY.md`：写明层级定义、触发条件、保留策略、失败模式与 API 合同。

### 写入流程

1. 记录来源证据。
2. 规则预处理与敏感信息过滤。
3. 通过注入的 LLM provider 生成结构化候选。
4. 验证 JSON schema、字段长度、枚举、引用有效性和置信度范围。
5. 执行冲突检测并产生 proposal。
6. 由 `Manual` / `SemiAuto` / `FullAuto` 策略决定人工审核或自动接受；不得为认知功能偷偷改变现有默认治理策略。
7. 事务提交 memory/version/relation/audit。
8. 后台更新检索索引。

### 前瞻意图规则

- 必须有明确的有效期或复评策略。
- 到期后转为普通历史事实/过期状态，不能无期限地作为当前计划返回。
- 不从单次含糊表述推断稳定身份或长期偏好。
- 支持用户/Agent 显式修正和删除；删除应遵循现有软删除与备份政策。

### 测试

- 同一事实重复输入：幂等或生成重复提案，不无限复制。
- “过去使用 X，现在决定改用 Y”：产生 supersedes，而非把两个事实当成无法解决的矛盾。
- “可能会考虑 Y”：不得无证据升级为确定意图。
- 到期意图不再出现在当前意图结果中。
- 模型超时、非法 JSON、未知字段、低置信度、空输出都不丢失原始证据。
- 同一 job 重试不重复创建 proposal/version。
- 审核拒绝后，候选不能被当前记忆检索为已接受事实。
- 全流程在 `Manual` 模式下绝不自动合并。

---

## 7. P3 — Working 经验记忆

### 目标

将“本次任务做了什么”与“以后如何复用经验”分开。事实层尽量完整，经验层需要通过价值门槛。

### 文件级改造定位

- `crates/mem-core`：新增任务生命周期与经验领域服务。
- `crates/store`：实现任务、经验、来源链接的 PG/内存持久化。
- `crates/consolidation`：复用候选审核、重复经验检测和 supersede 流程。
- `crates/worker`：任务结束后异步提炼经验；支持重试、幂等和最大重试次数。
- `crates/retrieval`：增加经验召回入口，但先不改变现有 `memory_search` 默认排序。
- `crates/mcp`：新增可选工具 `memory_task_start`、`memory_task_checkpoint`、`memory_task_finish`、`memory_experience_search`、`memory_experience_feedback`。工具名可调整，但不得重用既有名称。
- `docs/upgrade/WORKING_MEMORY.md`：记录任务状态机、经验质量门槛和反馈合同。

### 建议数据合同

`WorkingFact` 至少包含：任务目标、执行摘要、结果、失败原因、涉及文件/资源、来源证据 ID、开始/结束时间、状态。

`Experience` 至少包含：任务模式、搜索关键词、解决方案、前置条件、适用环境、失败模式、验证方法、置信度、来源任务、采用反馈、状态。

经验提取门槛至少检查：

- 是否可复用，而不只是当前任务总结；
- 是否有可验证的结果或明确标记为未经验证；
- 是否说明适用条件；
- 是否存在已知失败模式；
- 是否与现有经验重复或冲突；
- 是否包含密钥、令牌或不应持久化的敏感数据。

### 测试

- 每个成功结束的任务可保存一条任务事实；经验可为 0 条。
- 低价值、纯描述性或无验证的内容不强行生成经验。
- 同一任务重复 finish 幂等。
- 经验引用源任务与证据；源记录归档后仍可追溯。
- 环境不匹配时检索结果标明边界，不宣称适用。
- 反馈“失败”后降低排序或转入复核，不直接删除。
- 不同 namespace 之间不能通过 task ID/experience ID 关联。
- PostgreSQL 和内存实现结果一致。

---

## 8. P4 — Coding 代码演进链

### 目标

把真实代码变更历史作为 Working 记忆的附加证据层，不用模型生成或重写 diff。检索先由现有检索器决定命中哪些事实，再附加相关演进链。

### 文件级改造定位

- 新增 `crates/mem-core` 中的 coding domain/service 模块，或遵循现有 crate 拆分方式新增小型 crate；先用依赖图判断。
- `crates/store`：coding thread/revision 的 PostgreSQL 与内存存储、迁移、索引。
- `crates/retrieval`：对命中的 coding facts 附加 revision context，不让 diff 链取代主召回。
- `crates/mcp`：新增可选 `memory_coding_record_revision` / `memory_coding_history` 工具，或先只通过 Worker 内部服务接入。
- `crates/worker`：可选从用户显式配置的 Git repo 采集变更；默认不扫描任意路径。
- `docs/upgrade/CODING_MEMORY.md`：记录仓库授权、采集模式、隐私过滤、diff 上限和清理策略。

### 采集安全

1. 仅处理用户明确配置并授权的仓库路径；拒绝任意路径、路径穿越和符号链接逃逸。
2. 优先通过 Git plumbing 命令或现有 Rust Git 库获取 commit/diff；先检查依赖策略，不无理由添加大型依赖。
3. 保留真实 diff 的 hash、commit SHA、父版本关系和发生时间。
4. diff 进入数据库前扫描密钥、凭据、私钥、`.env`、二进制与超大文件；敏感内容需拒绝保存或做不可逆脱敏。
5. 对 diff 设置单条和总量限制，支持分页和保留策略。
6. 代码路径/符号可能变更；采用 repository + branch + path + symbol/region fingerprint 作为关联线索，不假定路径永远稳定。
7. 不把工作区未提交 diff 与已提交 commit 混为一类，必须标注来源和确定性。
8. 不自动执行 Git 命令之外的任意 shell 命令；不 checkout、不 reset、不改写用户工作树。
9. `coding_revisions` 备份/恢复必须保留引用完整性；恢复后无孤儿 thread/revision。
10. 不把完整源文件作为默认记忆；优先保存 diff 和坐标。

### 测试

- 纯新增、删除、重命名、同一函数多次修改、分支分叉、merge commit、回滚提交。
- 同一 diff 重放不重复入库。
- diff hash 与原始输入一致；系统不得“让模型补全”不存在的 diff。
- 敏感 token、私钥、`.env` 文件、超大 diff 和二进制文件被正确拒绝/脱敏。
- 不同 namespace 对同一 repo/path 的 revision 互不可见。
- 访问未经授权的路径、路径穿越和符号链接逃逸均拒绝。
- 代码事实被检索到时可附加对应历史；未命中时不扫描全库返回无关 diff。
- restore 和 backup roundtrip 保持 revision 链完整。

---

## 9. P5 — 检索优化与完整评测

### 目标

在现有 FTS + pgvector 并发召回、五维评分、MMR 和软超时降级之上增加 scene-aware 的有效性判断；不能用未经评测的复杂度替代现有算法。

### 文件级改造定位

- `crates/retrieval`：增加场景过滤、经验适用性过滤、时间有效性和关系扩展。
- `crates/mem-core`：提供受 ACL 约束的查询上下文；不让 retrieval 直接构造账户范围。
- `crates/store`：按真实 query plan 添加索引和必要的 PG integration tests。
- `crates/embed`：保持现有 OpenAI-compatible embedding 和 mock 契约。
- `crates/worker`：可选批量 re-embedding/索引修复作业，必须有 checkpoint、幂等和限速。
- 新增 `crates/memory-eval` 或仓库现有测试框架下的 eval 模块（先检查 workspace 结构再决定）。
- `docs/upgrade/EVALUATION.md`：定义数据集版本、指标、基线、运行命令、性能环境和结果。

### 检索阶段

1. 授权与 namespace 过滤先执行。
2. 根据显式 `scene` 选择 Chat / Working / Coding 查询策略；未提供 scene 时保留现有默认行为，避免破坏旧客户端。
3. 并发执行 FTS 与向量召回。
4. 合并候选并按 ID 去重。
5. 过滤已失效的当前事实；若调用方请求历史，则仍可返回 superseded 记录并标明状态。
6. 对 Working 经验检查前置条件、boundary envs 和失败模式。
7. Coding 命中时附加相关 revision chain。
8. 应用现有五维评分和 MMR；新权重必须可配置且通过归一化校验。
9. 可选 reranker 必须受超时和预算限制，失败时回退现有排序。
10. 返回 evidence/source、status、validity 与 revision context 的最小必要字段。

### 评测集与指标

创建固定、可版本控制的合成/人工标注数据集，覆盖：

- 精确事实、语义改写、中文关键词、英文关键词、中英混合；
- 新事实取代旧事实、真实矛盾、过期意图；
- 相似但环境不同的 Working 经验；
- 交错项目的 Coding 任务、文件重命名、回滚与分支差异；
- 同名 namespace 的账户隔离攻击；
- 缺失 embedding、embedding 维度错误、FTS/vector 单路超时；
- 重复、噪声、低置信度候选与冲突提案。

至少报告：

- Recall@K、MRR 或 nDCG@K；
- 事实抽取 precision/recall；
- 冲突裁决准确率、错误覆盖率；
- Working 经验复用成功率；
- Coding revision 还原/关联准确率；
- 跨账户越权读取/写入次数（必须为 0）；
- p50/p95 延迟、每次写入模型调用数、Token/费用（如可得）；
- store/worker 重试导致的重复记录数（必须为 0）。

所有性能比较必须使用同一数据集、硬件、数据库参数和 embedding 模型。先记录 P0 基线，再比较改进；不允许只报告新系统分数。

---

## 10. MCP 与 API 兼容策略

1. 不改变现有 18 个 MCP 工具名称、参数必填项、默认值、错误码和 readOnly/destructive/idempotent 注解。
2. 新工具采用 additive change；每个工具 schema 关闭 `additionalProperties`，声明最小 scope。
3. 新工具只接收 namespace、业务 ID 和业务参数；绝不接收 `account_id`、内部 namespace key、可信 AuthCtx 或数据库字段作为客户端可伪造的权限来源。
4. 读证据、读任务、读经验和读 Coding revision 必须沿用现有 `AuthCtx` + scope + namespace grant 的双重检查。
5. 新增写入、裁决、采集工具必须设置幂等语义与 destructive 标记。
6. MCP 工具的错误映射遵循现有协议合同：工具错误与传输层 JSON-RPC 错误不能混用。
7. 旧客户端没有 scene 参数时继续走旧路径；不得强制要求升级客户端。

---

## 11. 备份与恢复合同

升级后，备份和恢复必须将新实体纳入同一个一致性快照或在文档中明确其可重建规则。

- `memory_evidence`、`memory_relations`、tasks、experiences、coding threads/revisions、proposals 需要纳入快照 payload 或由已备份的不可变数据确定性重建。
- checksum 必须覆盖新数据和关系。
- restore dry-run 要展示新增/更新/冲突/孤儿引用的差异。
- replace 模式继续先创建安全备份，再做事务性替换。
- 失败后安全备份可恢复；恢复不允许留下指向不存在 memory/task/thread 的关系。
- 旧格式备份必须仍可恢复；缺失新字段采用明确、可测试的默认值。
- 增加 backup format version；不要仅依赖 Rust struct 的隐式 serde 默认。
- 在每个阶段运行备份/恢复 roundtrip；不得等到 P5 才验证。

---

## 12. 全阶段验收矩阵

每个阶段完成后都运行：

```bash
cargo fmt --all -- --check
cargo test --workspace --all-features
cargo clippy --all-targets --all-features -- -D warnings
```

如果实际 CI 使用其他命令或 features，以仓库现有 CI 为准并记录。

有 PostgreSQL 环境时：

```bash
cargo test -p store --features pg-integration -- --ignored
```

每阶段还需：

- `git diff --check`
- 审阅 `git diff --stat` 与完整 diff
- 检查是否意外改变旧 MCP schema
- 检查 migration 在空库和升级库都能执行
- 检查内存 store 测试
- 检查备份/恢复 roundtrip
- 检查跨账户、跨 namespace 和 revoke 后访问
- 检查新增依赖、许可证、MSRV 与 `Cargo.lock`
- 更新 `README.md` 和 `docs/设计-多智能体长期记忆服务.md`，只记录已经实现并通过验收的功能

最终发布前应提供：

1. `docs/upgrade/REPOSITORY_AUDIT.md`
2. `docs/upgrade/BASELINE.md`
3. `docs/upgrade/CHAT_MEMORY.md`
4. `docs/upgrade/WORKING_MEMORY.md`
5. `docs/upgrade/CODING_MEMORY.md`
6. `docs/upgrade/EVALUATION.md`
7. migration 文件及迁移说明
8. 每阶段测试报告、变更摘要和回滚操作
9. 旧 MCP 工具兼容性报告
10. 备份/恢复兼容性报告

---

## 13. 回滚与故障处理

### 代码回滚

- 每阶段单独 commit；不要把 P0–P5 放在一个大提交中。
- 新功能先以配置开关关闭，默认行为保持与旧版相同。
- 出现回归时先关闭新 scene/worker job/reranker，不先删除数据。
- 禁止执行 `git reset --hard`、`git clean -fd` 或覆盖用户未提交改动。
- 失败修复优先使用新提交或 revert 对应阶段提交。

### 数据库回滚

- 生产升级前执行并验证一次可恢复备份。
- additive migration 优先采用 forward-fix。
- 新表尚未写入数据时可按明确的 down migration 回退；一旦写入生产数据，必须先导出/备份新增实体并确认数据归属。
- 不得为了回滚代码自动删除认知数据。
- 任何 destructive migration 都要求显式批准、备份 ID、checksum 验证和恢复演练。
- 若 restore 失败，保留原始库和 safety backup，停止后续迁移，不自动尝试清空重建。

### 模型故障

- 超时、限流、非法结构化输出：保留原始证据，候选标记失败，可重试。
- 超过重试次数进入 dead-letter/人工诊断状态，不能无限循环。
- 不能因模型不可用阻断现有 `memory_put` / `memory_search`。
- 模型响应内容视为不可信输入，必须校验长度、枚举、引用 ID、权限范围与敏感数据。

---

## 14. Agent 执行顺序与交付格式

严格按顺序执行，不可并行推进会冲突的 migration：

1. **Audit-only**：完成仓库审阅，写 `REPOSITORY_AUDIT.md`，报告实际路径、现有 trait、migration runner、现有备份 payload、测试入口与任何差异；不改业务代码。
2. **P0**：记录基线，补安全回归。
3. **P1**：新增证据/时间/关系 schema 和领域 API，验证备份兼容。
4. **P2**：实现 Chat 提案管线。
5. **P3**：实现 Working 事实与经验双层。
6. **P4**：实现 Coding diff 链。
7. **P5**：增加 scene-aware 检索和固定评测。
8. 每阶段通过验收后才开始下一阶段。

每阶段完成时，Agent 必须输出：

- 实际修改文件列表（不列未修改的建议文件）
- 每个 migration 的编号、SQL 内容和升级/回滚说明
- 新增/修改的 Rust public types、traits、methods 与 API schema
- 测试命令及真实通过/失败结果（不得声称未运行的测试通过）
- 安全、OAuth、ACL、MCP、备份/恢复兼容性结论
- 性能/成本数据（若未测量，明确标注未测量）
- 已知问题与回滚命令
- commit SHA

**第一条回复只做 Audit-only。** 在输出 `REPOSITORY_AUDIT.md` 并给出真实源码依据前，不允许编写 migration，不允许添加 crate，不允许猜测现有文件名或接口。

[![powered by KimiCode](../.github/assets/kimi-badge.svg)](https://www.kimi.com/code)
