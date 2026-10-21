# Memory Hub 认知记忆系统：Chat 认知记忆流规范 (P2)

## 1. 概述与核心目标

Chat 认知记忆流（Chat Cognitive Memory Pipeline）为 Memory Hub 提供面向对话与会话场景的认知提炼与生命周期管理机制。
其设计遵循系统总体核心原则：
1. **真实世界对齐**：区分事实发生时刻（`occurred_at`）、系统记录时刻（`created_at`）与事实有效期（`valid_from` / `valid_until`）。
2. **证据可溯源（Append-only Evidence）**：每次提炼均在模型抽取前完成原始会话片段证据存根的持久化。即使模型超时或输出非法 JSON，原始证据绝不丢失。
3. **事实演化优于冲突僵局**：自动检测“过去使用 X，现在决定改用 Y”等事实变迁，通过 `MemoryRelationKind::Supersedes` 建立关联并闭合旧事实有效期，避免形成无法裁决的伪冲突。
4. **严格的治理纪律**：支持 `Manual`（全人工审核）、`SemiAuto`（半自动审查）与 `FullAuto`（全自动入库）。在 `Manual` 模式下，系统绝不自动合并或修改活跃记忆，必须经显式提案审查流程。
5. **前瞻意图防含糊**：前瞻意图必须具备有效期限或周期复评，到期意图自动从活跃检索中过滤；对含糊/推测性表述（“可能会考虑”、“或许”）严禁无证据升级为确定意图。
6. **幂等执行与安全重入**：无论直接服务调用或后台 Job 异步重试，同一会话和内容哈希绝不重复生成多份条目或版本。

---

## 2. 认知层级定义 (Cognitive Layers)

| 层级 (Layer) | 含义与定位 | 典型示例 | 存储形式与关联 |
| :--- | :--- | :--- | :--- |
| **Fact (事实)** | 确定的客观事实、系统配置、技术选型或环境信息 | “数据库已升级为 PostgreSQL 16” | `MemoryKind::Fact`，`scene: Chat`，挂载证据引用 |
| **Summary (摘要)** | 会话阶段性或全量讨论共识提炼，用于快速恢复上下文 | “讨论了认知升级架构，决定按 P0-P5 阶段执行” | `MemoryKind::Episode` 或 `Fact`，标记 `layer: Summary` |
| **Pattern (模式)** | 长期稳定的编码偏好、行为习惯或协作约定 | “用户习惯在每个函数上方增加用途与设计决策说明” | `MemoryKind::Preference`，`scene: Chat`，高置信度要求 |
| **Intention (意图)** | 前瞻性计划、待办事项、演进里程碑 | “计划在 2026-11 月完成 Working 经验记忆模块” | `MemoryKind::Fact`，必须具备 `valid_until`，到期自动失效 |

---

## 3. 提炼触发条件与流程

### 3.1 触发时机
1. **会话结束或阶段 Checkpoint**：调用方 Agent 或后台 Worker 提交 `ChatCognitiveInput`；
2. **显式提炼指令**：调用方 Agent 显式请求从当前对话提炼事实与偏好；
3. **异步重试扫描**：针对因瞬态故障未完成提炼的会话，后台 `ChatCognitiveJob` 周期重试。

### 3.2 写入与提炼流程图

```
ChatCognitiveInput (messages, session_id, namespace)
          │
          ▼
[Step 1: 计算 SHA-256 消息哈希与脱敏 Excerpt]
          │
          ▼
[Step 2: 追加写入 MemoryEvidence 证据存根 (Append-Only)]
  (若已存在同哈希证据，复用证据 ID 并进行幂等检查)
          │
          ▼
[Step 3: 调用 ChatCognitiveExtractor 生成 CognitiveCandidate 列表]
  ┌───────┴────────────────────────┐
  │ 异常 (Timeout / Invalid JSON)  │ 正常抽取候选
  ▼                                │
[保留证据并返回错误，支持安全重试]   │
                                   ▼
          [Step 4: 规则引擎校验 validate_cognitive_candidate]
             - 过滤含糊推测 ("可能会考虑...")
             - 校验置信度区间 [0.0, 1.0]
             - 校验 Intention 有效期完整性
                                   │
                                   ▼
          [Step 5: 事实演化检测 & 冲突检测]
             - 存在 Supersedes 目标 -> 建立 Supersedes 关系，限制旧记忆 valid_until
             - 发现内容/Key 严重冲突 -> 标记冲突并生成待裁决提案
                                   │
                                   ▼
          [Step 6: 治理策略执行 (CognitiveGovernanceMode)]
             - Manual: 全部转入 CognitiveProposalRecord (PendingReview)
             - SemiAuto: 高置信且无冲突自动落库，冲突与低置信生成提案
             - FullAuto: 自动提交记忆与关联关系
                                   │
                                   ▼
          [Step 7: 审计与检索索引更新 (版本递增 & 关系持久化)]
```

---

## 4. 保留策略与生命周期管理

### 4.1 证据存根 (MemoryEvidence)

#### 边界定义：「不可变」指写入之后，不是写入之前

| 说法 | 含义 | 是否成立 |
| :--- | :--- | :--- |
| 证据行写入后不可悄悄改写 | 只增不改（Append-only），禁止 UPDATE / 静默 DELETE | ✅ 成立 |
| 证据行写入前不能做任何处理 | 原始对话原封不动入库 | ❌ **不成立** |

**必须写清楚这一点**：不可变表一旦写入，就无法追回。Token、密码、私钥、个人信息
一旦落进 `memory_evidence`，之后再"修订"或"删除"也补不回来——原始副本可能已随备份扩散。
因此敏感内容的处理必须发生在**持久化之前**，而不是寄望于写入之后再补救。

#### 写入前的三道处理（`crates/mem-core/src/redaction.rs`）

1. **凭证扫描**（Credential）：私钥块（`-----BEGIN … PRIVATE KEY-----`）、
   API Token（`sk-` / `ghp_` / `github_pat_` / `glpat-` / `xox*` / `AKIA` / `AIza`）、
   JWT、`Bearer` / `Basic` 令牌、带口令的连接串（`scheme://user:pass@host`）、
   赋值型密钥（`password=` / `api_key:` / `client_secret=` …）——全部替换为 `[已脱敏:凭证]`。
   赋值型规则只替换**值**，保留键名，便于事后审计"这里曾经有过一个口令"而不泄露口令本身。
2. **敏感片段处理**（PersonalData）：邮箱、手机号、身份证号做掩码，替换为 `[已脱敏:个人信息]`。
3. **最小化保存**：证据行只保存定长摘要（`EVIDENCE_EXCERPT_MAX_CHARS`，默认 500 字符），
   **完整对话永不落库**。顺序不可颠倒——先截断再脱敏会漏掉被截断掉的尾部凭证。

> 词元与文档侧同源：脱敏在 CJK 文本里同样生效（"我的密钥是sk-…"这类中英混排不能漏检）。
> 实现上不用 `` 词边界（CJK 属 Unicode 词字符，边界在中文里不可靠），改用"上下文在捕获组之外"。

#### 保留期限（第二道防线）

- 每条证据带 `expires_at`（`HUB_EVIDENCE_RETENTION_DAYS`，默认 **90 天**；0 = 永久保留）；
- 到期由 `EvidenceRetentionJob` **物理删除**（`backup-worker` 周期执行），不让不可变副本无限期留存；
- 历史数据 `expires_at` 为 NULL，不做隐式删除。

#### 对照：Coding 路径的既有做法

Coding 修订链对 diff 采取的是**拒绝入库**策略（命中敏感凭证直接报错，不持久化）。
Chat 证据路径不能这么做——证据的用途就是"模型抽取失败时原始材料不丢失"，
所以采用**脱敏后入库**而不是拒绝。这两条路径的差异是有意为之，不是遗漏。

#### 独立安全测试

证据表有**独立**的安全测试（不依赖其它模块的测试通过）：

- `crates/mem-core/src/redaction.rs`：各类凭证形态、私钥块、个人信息、中英混排、
  纯数字不误伤、统计信息不泄露原文；
- `crates/mem-core/src/chat_cognitive_service.rs`：入库前的脱敏与保留期限；
- `crates/worker/src/evidence_job.rs`：到期清理只删已到期证据。

### 4.2 事实演化 (Supersedes)
- 当新事实明确声明取代（`relation_kind: Supersedes`）或与旧版本事实存在演化关系时：
  1. 写入新条目 `new_entry`；
  2. 封堵旧条目有效期：`old_entry.valid_until = Some(new_entry.occurred_at.unwrap_or(now))`；
  3. 在 `memory_relations` 中建立 `from: new_entry.id, to: old_entry.id, relation: Supersedes`；
  4. 旧条目保留于存储中以供审计与历史版本回溯，但在活跃查询时默认被排除。

### 4.3 前瞻意图 (Intention)
- 所有 `Intention` 候选必须具备 `valid_until`。
- `list_active_intentions` 查询严格应用 `valid_until > as_of` 过滤器，已过期的计划不再作为当前有效意图返回给 Agent。
- 含糊/推测表述（如包含“可能会考虑”、“或许”、“待定”、“视情况而定”）在校验阶段即被拒绝，绝不升级为确定意图。

---

## 5. 治理模式与提案状态机

系统支持以下三种治理模式（`CognitiveGovernanceMode`）：
1. **`Manual`**：
   - 绝不自动合并或直接落库为活跃记忆。
   - 所有抽取候选均转化为 `CognitiveProposalRecord`，初始状态为 `PendingReview`。
   - 只有通过 `review_proposal(..., ReviewDecision::Approve)` 显式批准后，方才提交至 `MemoryEntry` 并链接关系。
2. **`SemiAuto`**：
   - 若候选置信度 `>= auto_accept_threshold` 且不存在未解决冲突，自动入库。
   - 低置信度或存在冲突时，生成 `PendingReview` 提案。
3. **`FullAuto`**：
   - 合法且无严重冲突的候选自动提交入库；冲突候选生成提案供后续裁决。

### 提案状态机：
- `PendingReview` -> `Approved`（自动生成/激活 MemoryEntry，建立关系）。
- `PendingReview` -> `Rejected`（记录拒绝原因，候选条目绝不被当前记忆检索）。

---

## 6. 异常与失败安全保障 (Failure Modes)

| 异常场景 | 系统行为 | 安全保证 |
| :--- | :--- | :--- |
| **LLM 超时 / 网络中断** | `ChatCognitiveJob` 执行指数退避重试（默认 3 次） | 原始证据已在 Step 2 写入，绝不因网络中断丢失会话存根 |
| **LLM 输出非法 JSON / 乱码** | Extractor 报错并返回 `InvalidArgument`，终止当前抽取 | 证据完好保存，不产生破坏性脏数据或残缺记忆条目 |
| **低置信度输出** | 校验器过滤或根据治理模式降级为提案待审 | 不会污染生产环境高置信事实库 |
| **同一输入重复调用** | 消息哈希查重命中已有证据，直接返回已有结果 | 幂等重入，不产生重复条目或版本暴增 |
| **多账户越权访问** | `AccountScopedStore` 在服务入口及底层查询强校验作用域 | 跨账户完全隔离，Fail-closed，不泄露任何跨账户元数据 |

---

## 7. 接口与代码契约

### 7.1 抽取器契约 (`ChatCognitiveExtractor`)
```rust
#[async_trait]
pub trait ChatCognitiveExtractor: Send + Sync {
    async fn extract_candidates(
        &self,
        input: &ChatCognitiveInput,
    ) -> Result<Vec<CognitiveCandidate>>;
}
```

### 7.2 核心服务契约 (`ChatCognitiveService`)
```rust
pub struct ChatCognitiveService {
    // 内部封装 store, extractor, policy
}

impl ChatCognitiveService {
    /// 执行 Chat 会话认知提炼主流程
    pub async fn process_chat_session(
        &self,
        auth: &AuthCtx,
        input: ChatCognitiveInput,
    ) -> Result<ChatCognitiveResult>;

    /// 审核候选提案（批准入库或拒绝）
    pub async fn review_proposal(
        &self,
        auth: &AuthCtx,
        proposal_id: Uuid,
        decision: ReviewDecision,
        reviewer_agent: &str,
    ) -> Result<CognitiveProposalRecord>;

    /// 查询当前有效意图（自动过滤过期项）
    pub async fn list_active_intentions(
        &self,
        auth: &AuthCtx,
        namespace: &str,
        as_of: DateTime<Utc>,
    ) -> Result<Vec<MemoryEntry>>;
}
```

### 7.3 后台作业契约 (`ChatCognitiveJob`)
```rust
pub struct ChatCognitiveJob {
    // 封装 service, worker_agent, max_retries, initial_backoff_ms
}

impl ChatCognitiveJob {
    /// 执行会话认知提炼（带瞬态重试与幂等控制）
    pub async fn run_session(
        &self,
        auth: &AuthCtx,
        input: ChatCognitiveInput,
    ) -> Result<ChatCognitiveJobReport>;
}
```

<!-- kimi-badge -->
[![powered by KimiCode](../../.github/assets/kimi-badge.svg)](https://www.kimi.com/code)
