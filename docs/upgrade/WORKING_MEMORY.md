# Memory Hub 认知记忆系统：Working 经验记忆规范 (P3)

> **口径说明（2026-10 治理升级后）**
> 经验资产的**生命周期、验证门槛与版本历史**以 [`ASSET_GOVERNANCE.md`](./ASSET_GOVERNANCE.md) 为准；
> 本文覆盖 Working 场景的任务事实、提炼门槛、环境边界与异步作业。两者冲突时以 `ASSET_GOVERNANCE.md` 与源码为准。

## 1. 概述与核心目标

Working 经验记忆系统（Working Memory System）专为智能体长期多任务执行、问题解决与故障排查设计。
针对以往 Agent 系统中“把单次执行过程当经验塞满记忆库”、“经验无法跨环境复用或产生灾难性误导”的痛点，遵循以下原则：

1. **任务事实层（WorkingFact）与经验层（Experience）严格解耦**：
   - 任务事实记录“本次任务做了什么、输入、产出、状态”，单次任务事实完整归档；
   - 经验层提炼“通用的排查策略、故障处理模式、设计套路”，必须满足严格的可复用与可验证门槛。
2. **严格的质量门槛与安全防御**：
   - 严禁任何硬编码凭证（Token、API Key、密码、私钥）泄露；
   - 拒绝纯局部上下文叙述（如“本次任务我修改了第 12 行代码”），必须抽象为通用任务模式；
   - 必须包含非空可验证方法与环境边界条件；
   - 置信度门槛严格限制（$\ge 0.5$）。
3. **反馈闭环机制（不硬删除）**：
   - 正向反馈只提升 `feedback_score`，**不改变状态**（单次成功不足以证明通用有效）；
   - 负向反馈降低 `feedback_score`，衰减至临界值（$< 0.3$）时标记 `deprecated`，
     绝不物理硬删除，并写入版本快照保留审计轨迹。
4. **环境边界感知（Boundary-Aware Recall）**：
   - 经验严格限定适用环境（如 `["linux", "k8s"]`）；
   - 执行检索时，若当前运行环境与经验环境不匹配，检索结果显式标记 `env_mismatch = true` 并附带明确警告，同时实施分数惩罚；
   - **两种模式，按后果严重程度选**：
     - **降权模式（默认，`strict_env = false`）**：分数 ×0.5 + 显式告警，结果仍然返回，交由 Agent 判别；
     - **拒绝模式（`strict_env = true`）**：环境不匹配的经验**直接不返回**。
       降权只是**排序提示**——Agent 仍可能误取；把“只适用于 kafka 的处置步骤”用在 JVM 上这类有安全后果的场景，
       需要的是硬规则而不是排序。
5. **账户沙箱与命名空间强隔离**：
   - 账户沙箱下跨命名空间任务/经验关联直接阻断（Fail-Closed）；
   - 服务注入 `AccessChecker` 后，读、写、提炼、检索、状态流转都先校验命名空间 ACL。

---

## 2. 状态机与生命周期设计

### 2.1 任务生命周期状态机 (`TaskStatus`)

```
   [start_task]
        │
        ▼
     Running ──[finish_task: completed]──► Completed
        │
        ├──[finish_task: failed]─────────► Failed
        └──[finish_task: cancelled]──────► Cancelled
```

- **幂等性保障**：对已完成（Completed / Failed / Cancelled）的任务再次调用 `finish_task`，
  保持幂等返回，不覆盖已有的结果事实与完成时间。
- 任务可关联 `source_evidence_id`；Coding 修订可反过来通过 `source_task_id` 关联任务。

### 2.2 经验生命周期状态机 (`ExperienceStatus`)

```
   [distill_experience] ──► Candidate
                               │
                               │ verify（≥2 个不同已完成来源
                               │         + 无失败来源 + validation 非空）
                               ▼
                             Active
                               │
        reject（需理由）────────┼──────── deprecate（需理由）
                               ▼
                       Rejected / Deprecated
                               │
                               │ restore
                               ▼
                           Candidate
```

- **Candidate**：提炼出的候选经验；默认参与检索但排序权重为 0.6。
- **Active**：已显式验证、可在声明范围内复用的经验；权重 1.0。
- **Rejected**：审核未通过；必须给出理由；默认不参与检索。
- **Deprecated**：显式弃用，或负反馈使反馈分低于 0.3；权重 0.3，保留来源与历史。
- **Archived**：显式归档；默认不参与检索，随备份保留。

非法流转返回 `InvalidArgument`；并发流转使用乐观锁，版本不一致返回 `Conflict`。
每次状态流转都会在 `memory_experience_versions` 写入**该版本**的快照（每个 version 一行）。

---

## 3. 质量与安全门槛规范 (`validate_experience_candidate`)

提炼入库前必须通过四道规则检查：

| 门槛项 | 检验规则 | 违例处理 |
| :--- | :--- | :--- |
| **凭证扫描** | 正则与特征检测 `sk-`、`ghp_`、`AKIA`、`PRIVATE KEY`、`Bearer ` 等敏感字符串 | 直接阻断，返回 `InvalidArgument` |
| **通用性检验** | 拒绝包含“本次任务 / 刚才我 / 今天我完成了”等局部叙述 | 拒绝伪经验 |
| **可验证性检验** | 校验 `validation` 非空且有效 | 拒绝无明确验收方式的经验伪装 |
| **边界条件检验** | `boundary_envs` 或 `preconditions` 至少一项非空 | 严禁无边界的泛化主张 |
| **置信度底线** | `confidence` 必须落在 $[0.5, 1.0]$ 区间内 | 过滤低质量推测 |

> 注意：通过上述门槛只代表“可以成为候选”。升级为 `active` 还需要第 2.2 节的验证条件，
> 由 `working_experience_transition(action="verify")` 显式触发。

---

## 4. 存储架构

实际 DDL 以迁移文件为唯一事实来源，本文不再复制表结构，避免与源码漂移：

| 表 | 迁移 | 作用 |
| :--- | :--- | :--- |
| `memory_tasks` | `20261018000007_working_memory_tasks_and_experiences.sql` | 任务事实（状态、结果摘要、失败原因、证据） |
| `memory_experiences` | 007 + `20261028000016_experience_asset_lifecycle.sql` | 经验内容、状态、置信度、反馈分、版本与验证元数据 |
| `memory_task_experiences` | 007 | 任务与经验多对多关联（`derived` / `reused`） |
| `memory_experience_versions` | 016 | 经验版本快照（只追加） |

命名空间外键与复合唯一约束见迁移 `20261019000009_composite_namespace_fk_hardening.sql`。

---

## 5. 异步提炼作业与重试机制 (`ExperienceDistillJob`)

在 `crates/worker/src/experience_job.rs` 中提供异步提炼执行器：

- **瞬态错误重试**：遇到数据库连接异常、锁等待等瞬态可重试错误，以指数退避策略执行重试（默认 3 次）；
- **门槛拦截不重试**：当候选因敏感凭证泄露或通用性不足被拒时，直接记录拒绝报告，不进行无意义重试；
- **幂等查重**：若同一命名空间已存在相同任务模式和解决方案的经验，作业自动复用已有经验并补充源任务关联；
- **不得自动升级**：作业只做候选生成与衰减，**不会**把候选提升为 `active`——验证必须由显式调用触发。

> 当前该作业尚未接入任何二进制的调度；真实 LLM 抽取器落地后再装配。

---

## 6. 备份与还原

- 备份负载已升级到 **v5**：包含 `tasks`、`experiences`、`experience_versions`、`task_experiences` 等全量数据；
- 恢复路径兼容 v1–v5；**旧二进制不能恢复 v5 备份**（旧代码只认 v1–v4 校验和）；
- 校验和不匹配一律拒绝恢复，不把部分恢复报成成功。

细节见 [`GOVERNANCE_RELEASE.md`](./GOVERNANCE_RELEASE.md) §3 与 [`DEPLOYMENT_MODES.md`](./DEPLOYMENT_MODES.md) §5。

[![powered by KimiCode](../../.github/assets/kimi-badge.svg)](https://www.kimi.com/code)
