# 记忆整合 A/B 双模式升级（CONSOLIDATION AB）

> 任务书：`docs/Memory-Hub-AB-Consolidation-Upgrade-Taskbook.md`
> 实施基线：`main` @ `9d7fae270ec000cb0f900bb7084553b97127ed19`
> 说明：任务书编写时记录的 HEAD（`f367c78d`）已过期，本文档以实施时的真实 HEAD 为准。

---

## 1. 一句话结论

整合中枢现在支持两种**候选发现**模式，二者共用同一套确定性决策层：

| 模式 | 候选来源 | 嵌入调用 | 记忆内容是否出站 |
|---|---|---|---|
| **B `rules`（默认）** | 规范化指纹、结构化字段、本地词法索引 | **恒为 0** | **否** |
| **A `hybrid`** | B 的候选 ∪ 嵌入向量召回 | 按需、有界、可计数 | 视嵌入端点而定 |

**A 是"向量辅助发现候选"，不是"AI 自动改写记忆"；B 是"完全不调用向量模型的本地规则整合"。**
两种模式共享同一套证据约束、安全边界和确定性合并决策。

---

## 2. 现状映射（实施前的真实实现）

以下全部指向 `9d7fae2` 的真实符号，不是任务书的转述。

### 2.1 调用链

```
MCP memory_consolidate        crates/mcp/src/tools.rs
  └─ ConsolidationEngine::new(scoped_store, sim_threshold, max_proposals)
     .with_embedder(...) / .with_policy(AutoPolicy::new(PolicyMode::SemiAuto))
  └─ engine.consolidate_namespace(ns, dry_run, sim_threshold, agent_id)
MCP memory_integrate_scan     tools.rs → consolidation::service::DefaultIntegrationService::scan
cron IntegrationScanJob       crates/worker/src/integration_job.rs → engine.rs
```

### 2.2 实施前的候选发现（**全部依赖向量**）

- `DuplicateDetector`：按 `MemoryKind` 分组 → O(n²) 全量比对 → `cosine_similarity ≥ 0.92` → 并查集聚类。
  **无向量的条目被 `e.embedding.is_some()` 直接过滤掉，完全不可见。**
- `ConflictDetector`：
  - key 冲突（纯本地，但 PG 下 `idx_memories_ns_key_active` 唯一索引使活跃条目同 key 不可能共存）；
  - 语义冲突：候选对生成（小规模穷举 / 大规模固定种子 LSH）→ 余弦 ∈ [0.82, 0.92) → Jaccard 分歧度 ≥ 0.45。

### 2.3 实施前的自动合并门槛（只看相似度）

`AutoPolicy::should_auto_merge(similarity, member_count)`：

| 模式 | 判定 | 默认策略下的实际情况 |
|---|---|---|
| `Manual`（默认） | 永不合并 | `DefaultIntegrationService`（scan 路径）与 cron 作业都用它 |
| `SemiAuto` | 相似度 ≥ 0.98 且成员 ≤ 3 | 只有显式 `memory_consolidate{auto_merge:true}` 会切到 |
| `FullAuto` | 相似度 ≥ 0.98 | 无调用方 |

阈值梯度：`语义冲突 0.82` < `去重/整合 0.92` < `自动合并 0.98`。

**风险**：判定只看相似度，语义近重复（余弦 0.99）也能触发自动合并。

### 2.4 其他确认事实

| 事实 | 位置 |
|---|---|
| 合并后重嵌入是整合链路唯一的嵌入调用 | `engine.rs`（合并分支） |
| 提案表无溯源字段，`kind` 有 `CHECK (kind in ('duplicate','conflict'))` | `integration_proposals` DDL |
| `integration_proposals` **不在任何备份快照载荷内** | 当时快照 v1~v4；现为 v6（含 `memory_versions` 与 `experience_versions`），仍不含提案表 |
| `open_exists` 的 PG 与内存实现**语义一致** | PG 用 `cardinality` 相等 + `@>`，等价于集合相等 |
| `embedding_model` 全仓从未被读回比较 | 检测器会跨模型比较向量（本次已补守卫） |
| `MemoryStore::similar`（账户隔离的 ANN 召回）现成可用 | `infra.rs` / `account_scope.rs` |
| `events` / `audit_log` 表存在但**全仓无写入代码** | 不能当作现成审计通道 |

---

## 3. 实施后的架构

### 3.1 分层

```
请求 / 配置
  └─ 解析有效模式（requested → effective，含降级）
       └─ 按账户 / 命名空间 / 权限 / 有效期 / 状态读取候选空间
            ├─ B: RulesCandidateFinder    —— 结构上不持有 Embedder
            └─ A: HybridCandidateFinder   —— Rules ∪ Vector
                 └─ 去重候选并规范化证据
                      └─ 共享决策层：关系分类 → 冲突检查 → 提案治理 → 自动合并门槛
                           └─ 保留证据、审计、统计与版本历史
```

**模式差异只存在于「候选从哪来」。** 分类、冲突检查、提案治理与自动合并门槛逐字共用同一份实现。

### 3.2 新增类型

| 类型 | 位置 | 作用 |
|---|---|---|
| `ConsolidationMode` / `ConsolidationFallback` / `ConsolidationModeStatus` | `crates/common/src/consolidation.rs` | 模式枚举、降级策略、运行级状态 |
| `CandidateRelation` | `crates/consolidation/src/candidates.rs` | 8 类关系（精确重复 / 规范化重复 / 语义近重复 / 词法近匹配 / 版本更新 / 冲突 / 仅相关 / 不确定） |
| `CandidateSource` / `CandidatePair` / `CandidateFinder` / `CandidateContext` | 同上 | 候选抽象与上下文 |
| `RulesCandidateFinder` | `crates/consolidation/src/candidates/rules.rs` | 纯本地候选（无 embedder 字段） |
| `HybridCandidateFinder` | `crates/consolidation/src/candidates/hybrid.rs` | 规则 ∪ 向量候选 |
| `canonical_form` / `content_fingerprint` | `crates/mem-core/src/fingerprint.rs` | 保守规范化与内容指纹 |
| `CountingEmbedder` | `crates/mem-core/src/testing.rs` | 嵌入调用计数（用于证明 B 模式 0 调用） |

### 3.3 B 模式规则集

规范化：`NFKC` → 换行归一 → 连续 Unicode 空白折叠为单空格 → 去首尾空白。
**不做**大小写折叠、**不删**否定词、不归一数字/单位/日期/版本号/路径/URL/代码标识符。

| 规则 | 条件 | 关系 | 自动合并 |
|---|---|---|---|
| R1 精确重复 | 同 kind 且原文逐字节相同 | `ExactDuplicate` | 可 |
| R2 规范化指纹 | 同 kind 且指纹相同 | `CanonicalDuplicate` | 可 |
| R3/R4 业务键 | 同 key 且正文不同 | `Conflict` / `SupersedingVersion` | 否 |
| R5 词法+结构 | 同 kind、场景一致、标签/键有交集、词法高度重叠 | `Uncertain` | 否（只出待审提案） |
| R6 词法相关 | 同 kind 且词法高度重叠但缺结构佐证 | `RelatedOnly` | 否（不提案） |
| R7 环境冲突 | 词法高度重叠且环境标签不相交 | `Conflict` | 否 |

### 3.4 共享决策层

- **自动合并门槛升级为「关系类型 + 相似度 + 成员数」**：
  只有 `ExactDuplicate` / `CanonicalDuplicate` 具备自动合并资格；
  语义近重复、词法近匹配、版本更新与冲突**一律降级为待审提案**。
- 其余共享逻辑全部沿用：冲突优先于重复的成员剔除、`open_exists` 幂等查重、
  `suggested_survivor` 取最新、`execute_merge` 软删除 + `Supersedes` 关系 + 版本历史、
  `dry_run` 只读、`AccountScopedStore` 账户/命名空间授权。
- **每次扫描只算一次候选**：原实现会跑三遍检测器（统计一遍、冲突一遍、重复一遍），
  拆分后统一为一次，避免向量模式下 embedding 调用数变成三倍且无法解释。

---

## 4. 行为变更（发布必读）

| 变更 | 说明 |
|---|---|
| **整合默认不再使用向量** | 默认 `HUB_CONSOLIDATION_MODE=rules`。即使配置了 `HUB_EMBED_PROVIDER=http`，整合也不会自动调用向量服务。配置了检索 embedding **不会**自动启用向量整合。 |
| **自动合并门槛收紧** | 语义近重复即使余弦 0.99 也不再自动合并，只生成待审提案。默认 `Manual` 策略不受影响；受影响的只有显式 `memory_consolidate{auto_merge:true}` 的 SemiAuto。 |
| **B 模式下合并可能清空幸存者向量** | 合并后正文若变化，B 模式不会为了补齐向量调用嵌入服务，改为清空该条目向量（与既有"重嵌入失败"分支一致），该条目退化为词法可检索，直到被再次写入重建。合并后正文未变时保留原向量。 |
| **提案新增溯源字段** | 提案新增 `metadata` 字段（关系类型、候选来源、原因码、模式、策略版本）。旧提案读作 `{}`。 |
| **提案分组口径微调** | 词法近匹配与证据不足的候选按**两成员**提案给出（不再并查集聚类）；只有精确/规范化/语义近重复参与聚类。 |
| **整合工具新增可选 `mode` 参数** | 默认禁止逐请求覆盖（`HUB_CONSOLIDATION_ALLOW_MODE_OVERRIDE=false`）。旧客户端不传该参数，行为不变。 |

---

## 5. 数据库迁移

新增迁移 `crates/store/migrations/20261021000011_consolidation_proposal_metadata.sql`：

```sql
alter table integration_proposals
  add column if not exists metadata jsonb not null default '{}'::jsonb;
comment on column integration_proposals.metadata is '...';
```

- **向后兼容**：新增列带默认值，旧行读作 `{}`；不修改 `kind` 的 CHECK 约束。
- **不需要升快照版本**：`integration_proposals` 不在任何备份快照载荷内，旧快照恢复不受影响。
- **幂等**：可在已含该列的库上重复执行；同时兼容 Rust 迁移运行器与 `docker-compose` 的 initdb 路径。
- 迁移数量守卫测试（`MIGRATIONS.len()`）已同步更新为 11。

---

## 6. 配置

| 环境变量 | 默认 | 说明 |
|---|---|---|
| `HUB_CONSOLIDATION_MODE` | `rules` | `rules` \| `hybrid`；未知值启动即失败 |
| `HUB_CONSOLIDATION_FALLBACK` | `rules` | `rules`（降级）\| `error`（报错） |
| `HUB_CONSOLIDATION_ALLOW_MODE_OVERRIDE` | `false` | 是否允许 MCP 逐请求覆盖模式 |
| `HUB_CONSOLIDATION_EMBED_TOP_K` | `20` | hybrid 单次扫描最多新计算向量的条目数（硬上限 200） |
| `HUB_CONSOLIDATION_EMBED_MIN_SIMILARITY` | 未设置 | hybrid 语义候选相似度下限；**未设置 = 未经真实模型标定**，回落到 `INTEGRATE_SIM_THRESHOLD` 并标记 `similarity_threshold_calibrated=false` |
| `HUB_CONSOLIDATION_LEXICAL_MIN_SIMILARITY` | `0.80` | 纯规则模式的词法候选下限 |

既有 `INTEGRATE_SIM_THRESHOLD` / `INTEGRATE_MAX_PROPOSALS` 保持不变（无前缀，属历史命名）。

**检索与整合解耦**：`HUB_EMBED_PROVIDER`、`HUB_RETRIEVAL_MIN_SCORE`、MMR 与 `memory_search` 的行为完全未改。
整合模式**不**影响检索模式，反之亦然。

---

## 7. 健康与能力上报

`GET /readyz` 与 `GET /capabilities` 新增 `consolidation` 段，**与检索的 `semantic.ready` 明确区分**：

```json
"consolidation": {
  "mode": "rules",
  "fallback": "rules",
  "allow_mode_override": false,
  "vector_ready": false,
  "threshold_calibrated": false
}
```

每次整合运行的响应新增 `mode` 状态段：

```json
"mode": {
  "requested_mode": "hybrid",
  "effective_mode": "rules",
  "candidate_strategy": "local_rules",
  "embedding_used": false,
  "embedding_calls": 0,
  "embedded_entries": 0,
  "fallback_applied": true,
  "fallback_reason": "embedding_provider_not_ready",
  "strategy_version": "consolidation-v1",
  "similarity_threshold_calibrated": false
}
```

**降级必须显式可见**：`requested_mode` 保持 `hybrid`、`effective_mode` 为 `rules`，
不得把降级结果伪装成成功的向量整合。

---

## 8. 评测

新增 `crates/consolidation/tests/consolidation_eval.rs`：

```bash
cargo test --test consolidation_eval -- --nocapture
```

与检索评测（`retrieval::eval_bench`）是**两套不同的东西**：检索评测衡量排序质量，
本套件衡量整合质量（候选召回、候选精确率、**错误合并数**、冲突识别召回、硬约束违规数）。

数据集覆盖任务书 §10.1 的 15 类样本，程序化确定性生成，固定时钟，SHA-256 指纹。

### 8.1 实测结果（本次实施）

| 指标 | 模式 B（rules） | 模式 A（hybrid） |
|---|---|---|
| 数据集指纹 | `be5aaac5319944e3f8bd504d77c59078bd49686dbd30e3ebb73af5c0bbd2c933` | 同左 |
| 样本数 | 16 | 16 |
| 重复候选召回 | 1.000 | 1.000 |
| 候选召回（含语义改写） | 0.667 | 0.667（**未验收**：无真实嵌入端点） |
| 冲突候选召回 | 1.000 | 1.000 |
| 候选精确率 | 1.000 | 1.000 |
| **错误合并数** | **0** | **0** |
| **硬约束违规数** | **0** | **0** |
| **embedding 调用次数** | **0** | 0（未配置真实端点时按 fallback 降级为规则模式） |
| 语义改写类召回 | 0.000 | 0.000（**未验收**） |

> 候选召回 0.667 来自 1/3 个"候选类"标注对未被召回——即语义改写样本：
> 纯规则模式按设计抓不到同义改写（词法 Jaccard 仅 0.071，远低于 0.80 门槛），
> 这正是模式 A 存在的意义。**该指标在接入真实嵌入端点后才有验收意义。**

**强制门槛**（全部写成断言）：

- 错误合并数 = 0；
- 硬约束违规数 = 0（时间 / 版本 / 否定 / 数值 / 环境 / 保持分立）；
- 跨账户越权 = 0；
- B 模式 embedding 调用数 = 0；
- A 模式生成式 LLM 调用数 = 0（全程只用 embeddings 协议）。

**语义增益的验收条件**：只有在配置真实嵌入端点
（`HUB_CONSOLIDATION_EVAL_EMBED_ENDPOINT` 等）时，才断言 A 在语义改写类样本上的召回高于 B。
未配置真实端点时，该项标记为**未验收**——不以伪向量的结果冒充语义质量。

---

## 9. 验收记录

| 项目 | 结果 |
| :--- | :--- |
| 实施基线 HEAD | `9d7fae270ec000cb0f900bb7084553b97127ed19`（`main`） |
| 变更前基线测试 | `cargo test --workspace`：**396 通过 / 0 失败**（另 4 个 ignored），**无预存失败** |
| 变更后测试 | `cargo test --workspace`：**442 通过 / 0 失败**（另 4 个 ignored），新增 46 个用例 |
| 格式检查 | `cargo fmt --all -- --check` → 退出码 0 |
| 静态检查 | `cargo clippy --workspace --all-targets -- -D warnings` → 退出码 0，零警告 |
| 文档构建 | `cargo doc --no-deps --workspace` → 退出码 0 |
| 检索基线回归 | `cargo test --test eval_bench -- --nocapture` → 门禁通过，全部指标 Δ = 0.0000（数据集指纹 `9f5b3b075a558371`） |
| 整合评测 | `cargo test --test consolidation_eval -- --nocapture` → 9 通过 / 0 失败 |
| 数据库迁移 | 新增 1 条（`20261021000011_consolidation_proposal_metadata`）；守卫测试 `test_migrations_registered` / `test_every_migration_file_is_registered` 均通过 |
| PostgreSQL 集成测试 | **本机未执行**：无 Docker / psql / 本地 PostgreSQL；由 CI `postgres` job 覆盖 |
| 真实嵌入端点验收 | **未执行**：本机无可达的真实 embeddings 端点；A 模式语义增益标记为"未验收" |

> 本地按 `GEMINI.md` 禁用 `--all-features`（路径含空格会导致 `tikv-jemalloc-sys` 构建失败）；
> CI 侧路径无空格，仍按 `.github/workflows/ci.yml` 跑 `--all-features`。

---

## 10. 已知限制与遗留

| 限制 | 说明 |
|---|---|
| 跨进程并发扫描仍可能重复产出 open 提案 | 本次用**进程内按命名空间互斥**覆盖了同进程并发（MCP 请求与 cron 同进程）。跨进程（MCP 进程与 backup-worker 进程同时扫同一命名空间）需数据库侧 `status='open'` 的部分唯一索引才能彻底消除；该方案需要评估迁移期去重对生产数据的影响，**本次未实施**。 |
| A 模式语义质量需真实模型验收 | 无真实嵌入端点时，语义增益标记为"未验收"。 |
| 纯规则模式召回低于向量模式 | 这是设计意图：规则模式只做保守、可解释的判定。语义改写类样本主要靠 A 模式覆盖。 |
| 词法候选的倒排索引规模 | 词元文档频超过 64 的常见词不参与候选生成；每条目最多取 24 个最稀有词元配对。 |
| 环境标签提取依赖约定 | 环境标签取自 `metadata.envs` / `metadata.boundary_envs` / `metadata.environments` 与 `env:` 前缀标签。 |
| 整合模式不覆盖认知提炼 | 本次只改**记忆整合链路**。`memory_search`、Chat 认知提炼等是否调用模型需另行说明——整合模式 B **不等于**全系统离线。 |
| 规则模式不识别否定极性 | "不是 X" 与 "X" 规范化后指纹不同（不会误判为重复），但二者词法高度重叠，会作为**待审候选**提出来——不会自动合并，但也不会被自动判成"冲突"。识别否定极性需要额外的语义分析，规则模式暂不做。 |
| 词法候选偏保守 | 词法近匹配只在"同 kind + 场景一致 + 标签或业务键有交集"时才升级为待审提案；缺少结构化佐证时只计入"仅相关"并保持分立。这是有意的保守取舍：宁可少提案，不要错提案。 |

---

## 11. 发布与回滚

### 默认 B，先发布规则模式

不设置 `HUB_CONSOLIDATION_MODE` 即为 `rules`：整合不会发出任何嵌入请求。

### 显式启用 A

```bash
HUB_CONSOLIDATION_MODE=hybrid
HUB_EMBED_PROVIDER=http
HUB_EMBED_ENDPOINT=https://embed.example.com/v1/embeddings
HUB_EMBED_MODEL=bge-m3
HUB_EMBED_DIM=1024
# 建议先留空，确认候选质量后再按评测标定
# HUB_CONSOLIDATION_EMBED_MIN_SIMILARITY=0.90
```

首次启用 A 时优先用 `dry_run: true` 观察候选质量，确认自动合并安全门槛后再允许自动合并。

### 切回 B

```bash
HUB_CONSOLIDATION_MODE=rules
```

无需改动 OAuth、检索或数据库架构。也可只把 `HUB_CONSOLIDATION_FALLBACK` 设为 `rules`，
让 A 在不可用时自动降级。

### 恢复验证

```bash
cargo test --workspace
cargo test --test eval_bench -- --nocapture
cargo test --test consolidation_eval -- --nocapture
```

数据库侧：迁移是**加列**，旧版本程序仍可读写该表；回滚应用版本前无需改 schema。
如需回滚 schema，`alter table integration_proposals drop column metadata;` 即可（会丢失溯源信息）。

[![powered by KimiCode](../../.github/assets/kimi-badge.svg)](https://www.kimi.com/code)
