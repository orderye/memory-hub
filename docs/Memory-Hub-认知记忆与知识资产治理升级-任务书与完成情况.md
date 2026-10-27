# Memory Hub 认知记忆与知识资产治理升级 · 任务书与完成情况

> **文档性质**：任务书原文（保留章节结构与强制性要求）＋ 执行回执（逐章标注完成情况与证据）。
> **代码基线**：`main` @ `92a809d`（开工时工作区干净）；本次变更集未提交。
> **配套文档**：[`GOVERNANCE_AUDIT.md`](./upgrade/GOVERNANCE_AUDIT.md)、[`ASSET_GOVERNANCE.md`](./upgrade/ASSET_GOVERNANCE.md)、[`DEPLOYMENT_MODES.md`](./upgrade/DEPLOYMENT_MODES.md)、[`GOVERNANCE_RELEASE.md`](./upgrade/GOVERNANCE_RELEASE.md)。

---

## 0. 执行摘要

### 0.1 一句话结论

任务书要求的 **P0、P1、P3 大部分已在仓库中实现**；本轮真正补齐的是 **P2 经验资产治理与 Agent 记忆集合**，并修复了一个审计中发现的**真实安全缺口**（Working/Coding/Chat 认知服务缺少命名空间 ACL）。P4 的部署文档、评测与发布材料按实际可验证范围交付，未执行项全部显式标注。

### 0.2 完成度总览

| 章节 | 要求 | 状态 |
|---|---|---|
| 一 | 目标与不可违反约束 | ✅ 全部保留，未删除安全机制/字段/接口 |
| 二 P0 | 审计与安全基线 | ✅ 审计文档已产出；ACL 缺口已修复；真实 PG 基线 **NOT RUN（本机无环境）** |
| 三 P1 | Chat / Working / Coding | ✅ Chat/Coding 既有实现；Working 生命周期与版本历史本轮补齐 |
| 四 P2 | 经验资产、Skill、Agent 集合 | ✅ 生命周期、版本、集合与 ACL 全部落地；Wiki/CodeGraph 未做（任务书允许排除） |
| 五 P3 | 双部署 | ✅ 运行期 profile 已具备；部署文档补齐；compose 启动 **NOT RUN** |
| 六 | 检索、整合与评测 | ⚠️ 既有 Bench v2/整合评测通过；**未新增治理专属评测集**与真实向量对比 |
| 七 | 迁移与兼容 | ✅ 迁移 016/017；备份 v5；下行 SQL 与兼容性已记录 |
| 八 | 测试与 CI | ✅ 单测/集成/回归执行并记录；`--all-features` 本机 **BLOCKED（空格路径）** |
| 九 | 实施顺序与交付物 | ✅ 按阶段交付，映射见 §9 |
| 十 | Git 与数据保护 | ✅ 未 reset/未清理/未覆盖用户文件 |
| 十一 | 回滚 | ✅ 代码/迁移/备份回滚步骤已写入发布文档 |
| 十二 | 最终交付清单 | ✅ 10 项逐项对应到文档位置 |

### 0.3 验证结果（真实执行）

| 命令 | 结果 |
|---|---|
| `cargo test --workspace --no-fail-fast`（首轮） | 4 个目标失败，其余通过 |
| 修复后复跑 `-p mcp -p store -p worker --lib` | store **56/56**、worker **27/27**、mcp **全部通过** |
| `cargo test -p mem-core --test asset_governance` | **11/11 通过** |
| `cargo test -p mcp --test tool_schema_contract` | 通过（38 工具黄金快照） |
| `cargo clippy --all-targets -- -D warnings` | **通过，0 warning** |
| `cargo fmt --all -- --check` | **通过** |

未执行项：`--all-features`（空格路径 + jemalloc）、真实 PostgreSQL 迁移/集成、`docker compose` 启动、真实 embedding 语义效果、改造前 HEAD 基线复跑。

---

## 一、项目目标与不可违反的约束

### 1.1 总体目标（任务书原文要点）

将 Memory Hub 完善为可信、可追溯、可恢复、可扩展的 AI 长期记忆基础设施，同时支持个人轻量与完整生产部署。升级聚焦：

1. 完善 Chat / Working / Coding 三类记忆的处理边界；
2. 建立事实、证据、工作经验之间可追溯的关联；
3. 完善基于真实 Git 变更记录的 Coding 演化链；
4. 引入 Skill 式经验资产、Agent 记忆集合绑定与可选知识源集成；
5. 实现双部署，控制运行成本与维护复杂度。

明确**不是**重新开发 Agent 平台，也不是完整移植参考项目。

**完成情况**：✅ 方向一致。未新增 Agent 平台能力；未引入生成式 LLM 依赖；未替换存储引擎。

### 1.2 必须保留的现有能力（不可违反）

- Rust 现有架构及核心领域逻辑；
- PostgreSQL 与 pgvector 集成；
- OAuth 2.1 与现有授权流程；
- ACL、用户隔离、命名空间隔离及权限语义；
- MCP 服务及现有工具协议；
- 已有记忆读写/更新/删除/检索接口；
- 备份、恢复、迁移与数据兼容；
- 配置、部署入口、CI 与自动化测试约定。

**完成情况**：✅ 逐项保留。

| 约束 | 证据 |
|---|---|
| Rust 架构 | 未新增 crate 外的运行时；新增 `collection_service.rs` 仍在 `mem-core` 内 |
| PG + pgvector | 未改存储引擎；新增迁移只做 additive 变更 |
| OAuth 2.1 | `auth-core`/`authz`/`auth-web` 未改流程 |
| ACL/隔离 | 账户前缀与 `GrantChecker` 语义不变；认知服务补齐 ACL 校验 |
| MCP 协议 | 28 → 38 全为新增工具；既有工具签名兼容 |
| 备份/恢复 | 沿用 `BackupService`；新增 v5 字段，旧备份仍可恢复 |
| CI/测试约定 | 沿用 `scripts/ci/quality-gate.sh` 与既有测试组织方式 |

### 1.3 记忆整合的强制原则

**模式 B（默认）**：纯本地规则，不依赖向量/LLM；规范化、指纹、结构化字段、词法检索；不确定时保守处理；保留证据、冲突与历史。

**模式 A（可选）**：在 B 之上增加语义候选；向量相似度**不得**单独作为自动合并依据；不强制 LLM；不默认调用 LLM 改写；向量不可用时按配置回退或返回可诊断错误。

**任何模式都不得因语义相似自动合并存在事实冲突的记录。**

**完成情况**：✅ 既有实现已满足，本轮未削弱。

- `HUB_CONSOLIDATION_MODE=rules` 仍为默认；`hybrid` 需显式开启；
- 仅 `ExactDuplicate` / `CanonicalDuplicate` 可自动合并；语义近重复、冲突、版本更新一律只出提案（`consolidation/src/policy.rs`）；
- 适用环境不同会被判为冲突而非重复（`candidates.rs` 的强度排序注释）。

---

## 二、P0：现有仓库审计与安全基线

### 2.1 执行前必须完成的源码盘点

任务书要求至少检查：Git 状态、Cargo/features、源码目录与入口、schema 与迁移、OAuth/ACL、记忆模型与证据/时间、备份恢复、测试/CI/部署、文档/API/许可证。

**完成情况**：✅ 已核对真实文件，产出 [`upgrade/GOVERNANCE_AUDIT.md`](./upgrade/GOVERNANCE_AUDIT.md)。

| 盘点项 | 结论 |
|---|---|
| Git | `main`，开工时工作区干净，HEAD `92a809d` |
| Workspace | 14 个成员 crate；无全局 features；`mcp`/`store`/3 个 bin 有局部 features |
| 迁移 | 001–015 原有；本轮新增 016/017（共 17） |
| MCP | 28 个工具（原有）；本轮 38 个 |
| 测试 | 开工时 521 个测试属性、4 个 `#[ignore]`（真实 PG） |
| 许可证 | MIT OR Apache-2.0；依赖审计沿用 `deny.toml` |
| 文档 | 已存在 `docs/upgrade/` 历史快照；本轮新增四份现行文档 |

### 2.2 必须提交的审计产物

要求：架构图/模块关系、模型/表/索引/接口清单、features 与构建模式、迁移顺序与风险、测试命令与真实结果、功能三分类、必须保持的接口与数据约束；每条结论关联真实文件/符号/迁移。

**完成情况**：✅ `upgrade/GOVERNANCE_AUDIT.md` 已覆盖上述 7 项，逐条标注文件与迁移编号；未根据其他项目结构推测实现。

### 2.3 安全基线测试

要求重点检查：未授权访问与跨用户读取、跨命名空间读写、OAuth 令牌、MCP 工具权限、记忆更新/删除/历史、迁移与空库安装、备份校验与恢复、并发与重复请求、配置与部署兼容。

**完成情况**：⚠️ 部分完成。

| 检查项 | 结果 |
|---|---|
| 未授权/跨用户 | ✅ 账户前缀 + `GrantChecker`；`account_scope_tests` 全部通过 |
| 跨命名空间 | ❌→✅ **发现缺口**：Working/Coding/Chat 只查 scope；已注入 `AccessChecker` 并补测试 |
| OAuth 令牌 | ✅ 既有测试通过；本轮未改流程 |
| MCP 工具权限 | ✅ scope 声明 + 分发层；新增工具均声明 `memory:read/write/admin` |
| 更新/删除/历史 | ✅ 既有测试通过 |
| 迁移与空库 | ⚠️ **NOT RUN（本机无 PG）**；CI `postgres` job 为唯一口径 |
| 备份校验/恢复 | ✅ 既有测试 + 本轮 v5 恢复测试通过 |
| 并发/重复请求 | ✅ 既有幂等与乐观锁测试通过；新增经验乐观并发 |
| 配置/部署兼容 | ✅ profile 默认值不变；Lite 需显式选择 |

### 2.4 P0 验收标准

要求：架构与接口形成可核对基线；测试结果真实可追溯；保护用户数据与工作区；识别影响 OAuth/ACL/迁移/恢复的改动点；未通过基线审计不得大规模改造。

**完成情况**：✅ 满足，并据此把改造范围收敛到 P2 + ACL 修复，而非重写 P1。

---

## 三、P1：完善 Chat / Working / Coding 认知记忆

### 3.1 目标

参考 Hy-Memory 的三类组织方式，但不照搬 Python 实现、数据库设计或认知层级；保留 Rust 领域模型，在真实源码上补齐写入、检索、演化与关联规则。

**完成情况**：✅ 复用既有 `MemoryScene` / `CognitiveLayer` / `MemoryKind`，未引入新认知层级。

### 3.2 Chat：长期事实与对话记忆

要求：稳定事实与偏好；原始证据与来源；时间有效性；新旧替代；不确定/冲突；历史可追溯。原始证据与提炼结果分开；新事实不无条件覆盖；无法确认冲突时保留两条并标记待核验；过期/撤销/替代保留历史；不要求八层体系。

**完成情况**：✅ 既有能力满足，本轮未改动语义。

| 要求 | 证据 |
|---|---|
| 证据与提炼分离 | `memory_evidence`（append-only）+ `CognitiveCandidate` |
| 时间有效性 | `valid_from` / `valid_until`；意图过期过滤 |
| 替代关系 | `MemoryRelationKind::Supersedes` + `contradicts` |
| 冲突保留 | 认知提案与整合提案均保留双方，不自动覆盖 |
| 历史可追溯 | `memory_versions` + 关系网 |

### 3.3 Working：工作事实与可复用经验

要求：区分工作事实与可复用经验；一次事实关联零或多条经验；一条经验引用多条事实；经验可追溯来源；出现次数不等于正确；适用环境、验证状态、失效条件可表达。

**完成情况**：✅ 本轮补齐「验证状态」与「版本」。

| 要求 | 落点 |
|---|---|
| 事实/经验解耦 | `memory_tasks` / `memory_experiences` |
| 多对多 | `memory_task_experiences` |
| 来源可追溯 | 经验 → 关联任务 → `source_evidence_id`；`source_count` |
| 出现次数≠正确 | `verify` 要求 ≥2 个不同已完成来源、无失败任务、`validation` 非空 |
| 适用环境 | `boundary_envs`；不匹配降权或 `strict_env` 硬拒绝 |
| 验证状态 | `candidate/active/deprecated/rejected/archived` |
| 失效条件 | `failure_modes` + 负反馈衰减 + `deprecate` 理由 |

**新增行为**：`distill` 只产出 `candidate`；`verify` 是唯一升级入口；`record_feedback` 不再因单次成功升级。

### 3.4 Coding：代码变更与演化链

要求：关联仓库/分支/提交/变更对象；定位文件/符号/范围；保存真实变更证据；保留多次修改、回滚与后续演化；区分当前状态与历史版本；不把「发生过修改」当「修改成功」；不把提交信息当修改原因；不把原始 diff 拼接进所有检索结果；来源必须是可验证 Git diff 或明确记录；不能安全采集时先做只读查询与导入；大 diff 要有大小限制、敏感过滤、访问控制、按需读取。

**完成情况**：✅ 既有导入与安全过滤保留；本轮补齐**读取面**；采集仍为调用方导入（任务书允许）。

| 要求 | 证据 |
|---|---|
| 仓库/分支/提交/文件/符号 | `coding_threads` + `coding_revisions`（`commit_sha`/`parent_sha`/`symbol_fingerprint`） |
| 真实 diff 证据 | 调用方提交原始 diff；SHA-256 `diff_hash` 幂等 |
| 多次修改/回滚 | 修订按 `occurred_at` 排序；回滚作为新修订，`parent_sha` 指向前次 |
| 当前 vs 历史 | `coding_revision_list` / `coding_revision_get`（本轮新增） |
| 不误判成功 | 修订只记录变更，不推断结果；修复/测试结果通过 `source_task_id` 关联 |
| 安全过滤 | 512KB 上限、敏感凭证扫描、`.env`/路径穿越/二进制拒绝 |
| 不拼接进检索 | 修订内容不进入 `memory_search` 结果 |

### 3.5 三类记忆之间的关联

允许：Chat 事实关联 Working 任务；Working 经验关联 Coding 变更；Coding 变更关联修复/测试/回滚；一个项目可有多个 Agent 记忆但遵守隔离。

**完成情况**：⚠️ 部分覆盖。

- ✅ Working 事实 ↔ 经验（多对多）；Coding 修订 → 来源任务与证据；
- ✅ 修复/测试结果通过 `memory_tasks` 关联；
- ⚠️ 未新增跨三类记忆的**统一关联表**（任务书为「允许」而非强制）；Coding ↔ 经验目前需经任务间接关联。

### 3.6 P1 验收标准

要求测试覆盖：新事实替代旧事实、冲突与人工核验、工作事实关联经验、经验引用多来源、Coding 多次修改与回滚、不同分支/文件/用户隔离、历史检索、证据缺失或引用无效的错误处理；所有新增逻辑在纯规则模式工作，无 LLM 硬依赖。

**完成情况**：✅ 新增/既有测试覆盖：

- `asset_governance.rs`：多来源验证、失败来源阻断、拒绝与恢复、候选/已验证排序、Coding 多次修改与回滚链、跨命名空间隔离、ACL fail closed；
- 既有 `chat_cognitive_service` / `coding_memory_service` / `account_scope_tests` 覆盖替代、冲突、跨账户隔离；
- 全部逻辑为确定性规则，无 LLM 依赖。

---

## 四、P2：经验资产、Skill 与 Agent 记忆集合

### 4.1 目标

参考 TencentDB Agent Memory 的资产治理思路，让经验成为可审核、可复用、可授权的知识资产；不复制 Web 面板、Proxy、完整团队系统或代码图谱。

**完成情况**：✅ 只做逻辑资产层与授权边界，未复制 UI/Proxy/团队系统。

### 4.2 经验升级为 Skill

流程：工作事实 → 候选经验 → 验证 → 可复用 Skill → 更新或废弃。

Skill 需表达：唯一标识与版本、名称与用途、触发条件、适用环境与限制、执行步骤、验证方法、来源事实与证据、当前状态、所属范围、创建/更新时间。

**完成情况**：✅ 复用 `memory_experiences`，未新增第二套资产表。

| 逻辑字段 | 落点 |
|---|---|
| 标识+版本 | `id` + `version`（迁移 016） |
| 名称与用途 | `task` |
| 触发条件 | `search_keys` |
| 环境与限制 | `boundary_envs` + `failure_modes` |
| 执行步骤 | `solution`（结构化细节放 `preconditions`/`validation`） |
| 验证方法 | `validation` + `verified_at/by/note` |
| 来源事实与证据 | `memory_task_experiences` → `memory_tasks.source_evidence_id`；`source_count` |
| 状态 | `candidate/active/deprecated/rejected/archived` |
| 归属范围 | `namespace`（账户沙箱由 `account_id` 保证） |
| 时间 | `created_at`/`updated_at` |

### 4.3 Skill 生命周期

要求：Draft / Verified / Deprecated / Rejected；复用现有状态机；转换有明确条件；单次成功不代表已验证；只在声明范围推荐；环境变化可致失效；废弃保留来源；未验证不得与已验证同优先级；自动整合不得直接升级。

**完成情况**：✅ 全部落地。

| 要求 | 实现 |
|---|---|
| 四态 | `candidate`≈Draft、`active`≈Verified、`deprecated`、`rejected` |
| 转换条件 | `ExperienceTransition`：`verify/reject/deprecate/restore`，非法转换报 `InvalidArgument` |
| 单次成功不验证 | 提炼只出 `candidate`；正反馈不改状态 |
| 适用范围 | `boundary_envs` 匹配/硬拒绝 |
| 环境变化失效 | 显式 `deprecate`（需理由）或负反馈衰减 |
| 废弃保留来源 | 软状态 + 版本快照，不物理删除 |
| 检索优先级 | 权重 1.0 / 0.6 / 0.3；`rejected/archived` 默认不召回 |
| 自动整合不升级 | 后台作业与整合流程均不调用 `verify` |

### 4.4 Agent 记忆集合绑定

要求：不同 Agent 使用不同可访问记忆；复用用户/Agent/命名空间/ACL；绑定只决定候选范围、不绕过权限；删除绑定不删记忆；解绑/撤销/废弃有明确生效语义；不得成为跨用户旁路；初期只支持明确配置。

**完成情况**：✅ 新表 `memory_collections` + `memory_collection_bindings`；5 个管理工具；检索可选 `collection`。

- 创建/删除/绑定/解绑需 `memory:admin`；目标 Agent 必须属于同一账户；
- 未绑定调用 `resolve` 直接 `InsufficientScope`（fail closed）；
- 命名空间必须显式列出、禁止 `*`、最多 20 个；
- 检索逐命名空间调用原服务，ACL 拒绝的进入 `skipped_namespaces` 并回传；
- 解绑实时生效；删除集合级联删绑定，不触碰记忆；
- 单命名空间请求仍严格执行 ACL（不会因集合逻辑被降级为空结果）。

### 4.5 Wiki 与 CodeGraph 的可选集成

要求：Wiki（文档来源/版本/链接/按需读取）与 CodeGraph（文件/符号/调用关系/影响范围/与演化链引用）作为后续可选；区分代码图谱、演化链、工作经验；优先外部工具/适配器。

**完成情况**：❌ 未实现（任务书明确「不属于第一批必须实现的核心功能」）。未引入占位实现，避免伪能力。

### 4.6 P2 验收标准

要求：候选经验可审核/验证/废弃；Skill 可追溯；绑定不绕过 ACL；集合可配置与撤销；版本变化不丢历史；Wiki/CodeGraph 未启用不影响核心；资产操作有授权与错误处理。

**完成情况**：✅ 除 Wiki/CodeGraph 外全部满足；版本历史由迁移 016 的只追加表承载，并纳入备份 v5。

---

## 五、P3：个人轻量版与完整生产版双部署

### 5.1 目标

两种部署共用核心逻辑、存储模型与业务规则。

- 轻量：默认纯规则、无外部 embedding、无 LLM、减少后台依赖、保留读写/检索/证据/备份；
- 生产：保留 PG + pgvector、OAuth 2.1、完整 ACL、MCP、备份恢复、可选向量整合、多用户/命名空间/集合、资产治理与可观测性。

**完成情况**：✅ 由 `DeploymentProfile::{Production,Lite}` 统一实现；默认 Production。

### 5.2 实现要求

要求先检查 features/配置/启动流程；优先用现有构建特性、配置项、部署文件切换；不得复制业务逻辑；不得产生不兼容数据格式；明确编译期/运行期可选、两模式必需/仅生产、数据互通、升级降级与备份恢复；不得擅自把 PG 换成 SQLite。

**完成情况**：✅ 采用**运行期 profile**，不引入 Cargo features。

| 项 | 结论 |
|---|---|
| 编译期可选 | 无新增；现有组件均可运行期关闭 |
| 运行期可选 | 嵌入器 `off`、清扫间隔 `0`、内嵌 worker 开关 |
| 轻量必需 | PG + pgvector、auth-server、mcp-server、全部 MCP 工具、备份 |
| 仅生产启用 | 真实 embedding、`hybrid` 整合、多副本 worker/租约 |
| 数据互通 | 同一 schema、同一备份格式；双向切换无需迁移 |
| 升级/降级 | 迁移 additive；降级前需处理 `rejected` 状态与 v5 备份 |
| 存储引擎 | 未替换（Lite 仍用 PG） |

### 5.3 P3 验收标准

要求：两模式有可复现启动步骤；默认轻量不需向量/LLM；生产安全未削弱；核心逻辑无重复；迁移与备份兼容；未启用组件不阻断核心；配置缺失/服务不可用有清晰错误。

**完成情况**：⚠️ 文档与配置满足；**compose 实际启动 NOT RUN（本机无 docker）**。启动步骤见 [`upgrade/DEPLOYMENT_MODES.md`](./upgrade/DEPLOYMENT_MODES.md)。

---

## 六、检索、整合与记忆质量评测

### 6.1 整合规则

要求至少区分：完全相同、规范化等价、相似但信息不同、明确冲突、时间替代、环境不同、来源不足。

**完成情况**：✅ `CandidateRelation` 已覆盖 `ExactDuplicate/CanonicalDuplicate/SemanticNearDuplicate/LexicalNearMatch/SupersedingVersion/Conflict/RelatedOnly/Uncertain`；自动合并仅限前两类。

### 6.2 评测数据集

要求覆盖：精确事实、同义/中文改写、时间变化与历史、矛盾事实、环境差异、跨用户/命名空间隔离、代码变更链与回滚、经验推荐与负样本、域外查询、向量不可用、空库与大量重复。

**完成情况**：⚠️ 部分覆盖。

- ✅ 既有 Bench v2（480 文档 / 600 查询）覆盖精确、同义、时间、冲突、环境、域外、空库/重复；
- ✅ 本轮回归覆盖经验推荐正负样本、候选/已验证排序、Coding 回滚链、跨命名空间隔离、集合 fail closed；
- ❌ **未新增独立的治理评测数据集文件**，也未做真实 embedding 与纯规则的对比报告；
- ❌ 「向量服务不可用」仅由既有整合降级测试覆盖，未纳入统一评测集。

### 6.3 指标

要求：Recall@1/5/10、MRR、nDCG、错误合并率、冲突保留率、Top-K 硬负样本违规率、域外误召回率、旧事实处理率、越权率、备份恢复成功率、轻量资源/延迟；区分真实 embedding / 纯规则 / 测试替身。

**完成情况**：⚠️ 沿用既有指标与门禁；本轮未新增治理专属指标报告。Mock/伪向量结果未作为语义证据；真实 embedding 效果 **NOT RUN**。

---

## 七、数据库迁移与兼容性要求

任务书 10 条要求：先确认现有表；不得重命名/删除字段而不兼容；不得重建库；不得覆盖/清空用户记忆；新增索引评估成本；新字段有旧记录兼容；大回填可重复可恢复；失败有明确错误与保护；备份恢复覆盖升级前后 schema；API/MCP 变更提供兼容或版本策略。

**完成情况**：✅

| 迁移 | 内容 | 兼容性 |
|---|---|---|
| `20261028000016_experience_asset_lifecycle` | 状态约束 + 版本列 + 版本表 + 回填 | additive；`IF NOT EXISTS`；回填可重复 |
| `20261028000017_memory_collections` | 集合与绑定 | additive；`IF NOT EXISTS` |

- 未重命名/删除既有列；未重建库；未覆盖用户数据；
- 备份 v5 覆盖新表；旧备份 v1–v4 仍可恢复；
- 新备份**不能被旧代码恢复**（旧代码只认 v1–v4 校验和），已在发布文档显式警示；
- MCP 工具纯增量；`namespace` 改为可选但保留原语义，旧客户端不受影响。

---

## 八、测试与 CI 要求

任务书要求每阶段测试：单元（规则、冲突、时间、关联、状态、演化链、集合过滤、配置错误）、集成（迁移、多用户 ACL、MCP 授权、写入查询、更新历史、备份恢复、双模式兼容、向量降级）、回归（每次改动执行对应既有测试，发布前完整流程）。

**完成情况**：✅ 已执行并记录。

- 单元：`mem-core` / `store` / `mcp` / `worker` 全部通过；
- 集成：内存后端全通过；真实 PG **NOT RUN**；
- 回归：`asset_governance`、`tool_schema_contract`、`account_scope_tests`、`backup_service` 全通过；
- 静态：clippy `-D warnings` 0 warning；fmt 通过；
- 命令从仓库脚本与 CI 获取，未凭空编造。

> 本机 `--all-features` 被空格路径阻断（`REPOSITORY_AUDIT.md:20-23` 已记录），实际口径为 `cargo test --workspace` 与 `cargo clippy --all-targets -- -D warnings`；CI 在无空格路径执行 `--all-features`。

---

## 九、建议的实施顺序与交付物（对照）

| 阶段 | 任务书交付 | 实际交付 |
|---|---|---|
| 0 | 架构/文件/API/features/迁移/许可证/测试报告/改造计划 | `upgrade/GOVERNANCE_AUDIT.md`；本轮文件级清单见 `GOVERNANCE_RELEASE.md` §2 |
| 1 | Chat/Working/Coding 规范、事实与经验关联、时间/冲突测试、Coding 最小实现或差距报告、兼容迁移 | 既有规范 + 本轮生命周期与版本；迁移 016；差距见 §3.5 |
| 2 | Skill 生命周期、来源关联、Agent 集合、ACL 回归、Wiki/CodeGraph 评估 | 迁移 016/017；`collection_service`；`asset_governance` 11 项；Wiki/CodeGraph 明确排除 |
| 3 | 轻量/生产配置、部署升级文档、切换与兼容测试 | `DEPLOYMENT_MODES.md`；profile 既有；compose 未实测 |
| 4 | 固定评测集、规则/真实向量对比、安全回归、性能基线、备份验收、发布清单与回滚 | 发布清单/回滚完成；评测集与真实向量对比**未新增**（见 §6.2/6.3） |

---

## 十、Git、工作区和数据保护

任务书要求：检查工作区；保护未提交改动；不 reset/清理/覆盖用户文件；未确认备份前不执行破坏性迁移；不删除旧 API/数据/历史；遵循现有格式/错误/日志；每阶段独立变更集；不覆盖他人提交或重写历史。

**完成情况**：✅

- 开工时 `git status` 干净；全程未 `reset`、未 `clean`、未删除用户文件；
- 迁移全部为 additive，未执行任何破坏性迁移；
- 每阶段按文件分组，可独立审查（未提交）；
- **注意**：工作区出现的 `web/` 目录与 `.gitignore` 两行改动**不是本次任务产生**，已原样保留、未触碰。

---

## 十一、回滚与失败处理

任务书要求：代码可独立撤销；迁移区分可逆/不可逆；不可逆前有验证备份；新增数据有向后兼容读取；功能开关关闭后旧能力仍可运行；向量不可用不丢证据；撤销 Skill/绑定不删记忆；恢复失败不得误报成功。

**完成情况**：✅ 步骤见 [`upgrade/GOVERNANCE_RELEASE.md`](./upgrade/GOVERNANCE_RELEASE.md) §6。

关键点：
- 016 下行前把 `rejected` 改为 `deprecated`；
- 017 直接 drop 两张配置表，不触碰记忆；
- v5 备份需要新二进制恢复（降级前保留或先恢复）；
- 恢复失败按 `RestoreReport` 逐项报告，不报成功。

---

## 十二、最终交付清单（10 项对照）

| # | 要求 | 位置 |
|---|---|---|
| 1 | 源码审计报告 | `docs/upgrade/GOVERNANCE_AUDIT.md` |
| 2 | 设计决策记录 | `docs/upgrade/ASSET_GOVERNANCE.md`、`GOVERNANCE_RELEASE.md` §1 |
| 3 | 文件级变更清单 | `docs/upgrade/GOVERNANCE_RELEASE.md` §2 |
| 4 | 数据库迁移说明 | `GOVERNANCE_RELEASE.md` §3 |
| 5 | 测试报告 | `GOVERNANCE_RELEASE.md` §4 |
| 6 | 性能及检索评测 | §6 说明；本轮**未新增**独立评测与真实向量对比 |
| 7 | 安全回归报告 | `GOVERNANCE_AUDIT.md` §2.3、`GOVERNANCE_RELEASE.md` §1/§4 |
| 8 | 部署文档 | `docs/upgrade/DEPLOYMENT_MODES.md` |
| 9 | 发布与回滚指南 | `docs/upgrade/GOVERNANCE_RELEASE.md` |
| 10 | 遗留问题清单 | `GOVERNANCE_RELEASE.md` §7、本文 §13 |

### 最终验收原则

只有在真实源码修改完成、测试结果可复现、迁移与兼容性得到验证后，才能声称对应阶段完成；不得把设计文档完成、编译通过或 Mock 测试通过等同于完整验收。

**本回执的自我约束**：未执行的 PG 迁移/集成、compose、真实向量效果、`--all-features` 均标注为 `NOT RUN/BLOCKED`；未把首轮 4 个失败隐藏为“全绿”，而是记录了失败、修复与复跑。

---

## 13. 遗留问题清单

| 项 | 状态 | 影响 / 建议 |
|---|---|---|
| Wiki / CodeGraph | 未实现 | 任务书允许排除；需要时以独立适配器接入 |
| 治理专属评测集 | 未新增 | 经验推荐负样本、集合越权、回滚链目前由回归测试覆盖；建议后续固化为独立 Bench |
| 真实向量 vs 纯规则对比 | 未执行 | 需外部 embedding；不得用 Mock 结果替代 |
| 真实 PG 迁移/集成 | NOT RUN（本机） | 必须由 CI `postgres` job 验证后再发布 |
| `--all-features` 构建 | BLOCKED（空格路径） | 在无空格路径或 CI 执行 |
| Docker Compose 启动 | NOT RUN | 需在具备 docker 的环境做一次冒烟 |
| 跨三类记忆统一关联 | 部分 | Coding ↔ 经验目前经任务间接关联；如需直接引用应新增关系类型并评估迁移 |
| Coding 采集方式 | 调用方导入 | 未集成 git CLI 采集；任务书允许先做导入，后续可加只读采集器 |
| `rejected` 与旧代码 | 降级需转换 | 旧状态机不识别 `rejected`；回滚步骤已写明 |
| v5 备份与旧代码 | 不兼容 | 旧二进制无法恢复 v5 备份；发布文档已警示 |

<!-- kimi-badge -->
[![powered by KimiCode](../.github/assets/kimi-badge.svg)](https://www.kimi.com/code)
