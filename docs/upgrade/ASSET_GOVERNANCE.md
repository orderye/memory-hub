# 经验资产治理与 Agent 记忆集合

[![powered by KimiCode](../../.github/assets/kimi-badge.svg)](https://www.kimi.com/code)

> 适用版本：迁移 `20261028000016` / `20261028000017` 之后。
> 本文只描述**已实现**的行为；源码位置逐项标注，未实现项单独放在「边界与未覆盖」。
> 上游任务书：《Memory Hub 认知记忆与知识资产治理升级任务书》P1 / P2。

---

## 1. 定位与不变量

经验资产（Skill）不是新的存储子系统，而是**现有经验记录的治理层**：

1. 一条 `memory_experiences` 记录同时是「经验」与「资产」，字段映射见 §2；
2. 资产**只能通过显式状态流转**获得“已验证”身份，提炼与后台作业都不能；
3. 任何状态流转都写入**该新版本**的快照（每个 version 恰好一行）；
4. 记忆集合只**收窄候选范围**，不授予权限；读取仍由实时 ACL 判定。

不可违反：

- 不新增第二套经验表／状态机；
- 不因语义相似自动合并冲突事实（沿用整合中枢既有门槛，见 `docs/upgrade/CONSOLIDATION_AB.md`）；
- 不引入生成式 LLM 硬依赖：所有规则都是确定性代码。

---

## 2. 字段映射（任务书 4.2）

| 任务书逻辑字段 | 实际落点 | 说明 |
|---|---|---|
| 唯一标识 + 版本 | `memory_experiences.id` + `version` | 内容型/状态型变更 +1 |
| 名称与用途 | `task` | 任务模式描述 |
| 触发条件 | `search_keys` | 词法检索键 |
| 适用环境与限制 | `boundary_envs` + `failure_modes` | 空数组 = 未声明环境限制 |
| 执行步骤 | `solution`（结构化步骤可放 `preconditions` / `validation`） | JSONB 保持向后兼容 |
| 验证方法 | `validation` + `verified_at` / `verified_by` / `verification_note` | 验证门槛要求 `validation` 非空 |
| 来源事实与证据 | `memory_task_experiences` → `memory_tasks.source_evidence_id`；`source_count` | 多对多，天然支持多来源 |
| 当前状态 | `status`（`candidate/active/deprecated/rejected/archived`） | 新增 `rejected` |
| 归属范围 | `namespace`（账户沙箱由 `account_id` 前缀保证） | 不新增团队模型 |
| 创建/更新时间 | `created_at` / `updated_at` | |

源码：`crates/mem-core/src/domain.rs`（`MemoryExperience` / `ExperienceStatus` / `ExperienceTransition` / `ExperienceVersion`）。

---

## 3. 生命周期（任务书 4.3）

```
                verify（需满足门槛）
  candidate ─────────────────────────▶ active
      │                                  │
      │ reject（需理由）                  │ reject（需理由）
      ▼                                  ▼
  rejected ◀───────────────────────────┘
      │                                  │ deprecate（需理由）
      │ restore                          ▼
      └──────────────▶ candidate      deprecated
                                         │ restore
                                         ▼
                                     candidate
```

| 动作 | 合法起点 | 终点 | 强制条件 |
|---|---|---|---|
| `verify` | `candidate` | `active` | ≥ `HUB_EXPERIENCE_VERIFY_MIN_SOURCES`（默认 2）个**不同**的已完成来源任务；关联任务无 `failed`；`validation` 非空 |
| `reject` | `candidate` / `active` / `deprecated` | `rejected` | 必须给出 `reason` |
| `deprecate` | `active` | `deprecated` | 必须给出 `reason` |
| `restore` | `rejected` / `deprecated` / `archived` | `candidate` | 历史版本保留 |

实现：`crates/mem-core/src/working_memory_service.rs`
- `distill_experience` 只产出 `candidate`（**行为变更**，旧版本直接写 `active`）；
- `transition()` 是唯一的状态流转入口；
- `record_feedback` 只调整 `feedback_score`，**不再**因单次正反馈把候选提升为已验证；负反馈跌破 0.3 才转 `deprecated` 并写快照。

非法流转返回 `InvalidArgument`；并发写用乐观锁，版本不一致返回 `Conflict`。

> 例外：负反馈导致的 `deprecated` 是自动衰减，不受显式 `deprecate` 的起点限制——
> 候选经验连续被判定无效（`feedback_score < 0.3`）时也会转 `deprecated`，同样写版本快照。

### 3.1 验证门槛为什么是“两个来源”

单次成功可能是环境、数据或运气的偶然结果。任务书要求“单次成功不代表通用经验已验证”，因此默认要求至少两个**不同的已完成任务**，且不存在失败任务。该门槛只影响 `verify`，不影响提炼与检索。

配置：`HUB_EXPERIENCE_VERIFY_MIN_SOURCES`（1–10，越界启动失败）。

---

## 4. 版本历史（任务书 4.6）

表：`memory_experience_versions`（只追加）

| 列 | 含义 |
|---|---|
| `experience_id` + `version` | 唯一键；同一版本只保留第一条快照 |
| `snapshot` | 该版本的完整经验记录（JSONB） |
| `change_kind` | `created/updated/verified/rejected/deprecated/restored/superseded` |
| `changed_by` / `changed_at` / `note` | 审计信息 |
| `status` | 快照时刻的状态 |

读取：`working_experience_history` MCP 工具 → `WorkingMemoryService::list_versions`。

---

## 5. 检索排序（任务书 4.3.5）

`search_experiences_governed` 的分数：

```
score = 词法命中 × confidence × feedback_score × status_weight
status_weight: active=1.0, candidate=0.6, deprecated=0.3
```

- 默认排除 `rejected` / `archived`；
- `include_statuses` 可显式指定状态集合（审计场景）；
- 环境不匹配仍按既有规则降权 ×0.5 或 `strict_env` 硬拒绝。

源码：`crates/mem-core/src/working_memory_service.rs`、工具 `working_experience_search`。

---

## 6. Agent 记忆集合（任务书 4.4）

### 6.1 数据模型

`memory_collections`（账户级）

| 列 | 说明 |
|---|---|
| `account_id` + `name` | 账户内唯一 |
| `filter` | JSONB，结构见下 |
| `created_by` / `created_at` / `updated_at` | 审计 |

`memory_collection_bindings`：`(collection_id, agent_id)` 主键；集合删除时级联删除绑定。

过滤器只接受显式字段：

```json
{
  "namespaces": ["project-a", "project-b"],
  "scene": "coding",
  "layer": "knowledge",
  "kinds": ["fact", "skill"],
  "tags": ["rust"],
  "statuses": ["active"],
  "include_experiences": true,
  "experience_statuses": ["active"]
}
```

约束：`namespaces` 非空、最多 20 个、禁止 `*`（`CollectionFilter::validate`）。

### 6.2 授权语义（只收窄）

1. 创建/删除/绑定/解绑需要 `memory:admin`；
2. 目标 Agent 必须属于同一账户（`CollectionService::bind` 查 `AgentStore`）；
3. `resolve` 要求调用 Agent **已被绑定**，否则 `InsufficientScope`（fail closed）；
4. 检索时逐命名空间调用原服务，ACL 拒绝的命名空间进入 `skipped_namespaces` 并回传；
5. 解绑立即生效（每次调用实时查库，不缓存）；删除集合/绑定不删除任何记忆。

### 6.3 工具

| 工具 | 作用域 | 说明 |
|---|---|---|
| `memory_collection_upsert` | admin | 新建或按名称更新 |
| `memory_collection_list` | admin | 列出账户集合 |
| `memory_collection_delete` | admin | 删除集合与绑定 |
| `memory_collection_bind` | admin | 绑定 Agent |
| `memory_collection_unbind` | admin | 解绑（立即生效） |

检索侧：`memory_search` 与 `working_experience_search` 接受可选 `collection`（与 `namespace` 互斥）。`memory_search` 在集合启用 `include_experiences` 时额外返回 `experiences` 数组（分数体系不同，单独返回，不跨体系混排）。

响应中的 `scope` 字段始终回传 `namespaces` 与 `skipped_namespaces`，不静默吞掉无权限命名空间。

---

## 7. 本次同时修复的安全缺口

审计发现：`WorkingMemoryService` / `CodingMemoryService` / `ChatCognitiveService` 此前只校验 token scope，**未校验命名空间 ACL**。同一账户内任意持有 `memory:read` 的 Agent 只要知道命名空间名，就能读取其任务、经验、代码修订与认知提案。

修复：三个服务新增 `AccessChecker` 注入（`with_access`），所有公开读写方法先做 `can_read` / `can_write`；生产装配在 `crates/bins/mcp-server/src/main.rs` 注入 `GrantChecker`。未注入时仅用于单元测试与纯库嵌入，属于已知边界（见 §9）。

---

## 8. 兼容性

| 变化 | 影响 | 处理 |
|---|---|---|
| `distill` 返回值状态 `active → candidate` | 依赖“提炼即已验证”的调用方 | 发布说明显式告知；需要立即验证的调用方应改为 `distill` + `transition(verify)` |
| `working_experience_feedback` 移除 `status` 参数 | 旧客户端传 `status` 会收到明确错误 | 改用 `working_experience_transition`；直写状态会绕过门槛，必须关闭 |
| 新增 `rejected` 状态 | 严格解析旧客户端的未知值 | 旧客户端只会读到已有状态；新状态仅由显式 `reject` 产生 |
| 新增 10 个 MCP 工具（28 → 38） | 纯增量 | 旧工具签名保持；`memory_search` / `working_experience_search` 的 `namespace` 改为可选（与 `collection` 二选一） |
| 三个认知服务强制 ACL | 未授权命名空间调用由“成功”变“拒绝” | 这是安全修复；受影响调用方需补齐 grant |

---

## 9. 边界与未覆盖

- **Wiki / CodeGraph**：未实现，任务书允许排除在第一批之外；
- **`AccessChecker` 未注入时的行为**：只校验 scope（测试/嵌入式库场景）。生产二进制始终注入；
- **回滚链**：Coding 回滚通过“新的修订记录 + `commit_sha`/`parent_sha` + 文件路径”重建，不额外引入 `reverts_revision_id` 列（避免为单一用途改表）。读取工具 `coding_revision_list` / `coding_revision_get` 已提供；
- **集合的备份**：集合与绑定是账户级配置，不进入命名空间备份；记忆内容本身仍完整备份。
