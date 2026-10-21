# Memory Hub 认知记忆系统：Working 经验记忆规范 (P3)

## 1. 概述与核心目标

Working 经验记忆系统（Working Memory System）专为智能体长期多任务执行、问题解决与故障排查设计。
针对以往 Agent 系统中“把单次执行过程当经验塞满记忆库”、“经验无法跨环境复用或产生灾难性误导”的痛点，P3 严格遵循以下原则：

1. **任务事实层（WorkingFact）与经验层（Experience）严格解耦**：
   - 任务事实记录“本次任务做了什么、输入、产出、耗时、状态”，单次任务事实完整归档；
   - 经验层提炼“通用的排查策略、故障处理模式、设计套路”，必须满足严格的可复用与可验证门槛。
2. **严格的质量门槛与安全防御**：
   - 严禁任何硬编码凭证（Token、API Key、密码、私钥）泄露；
   - 拒绝纯局部上下文叙述（如“本次任务我修改了第 12 行代码”），必须抽象为通用任务模式；
   - 必须包含非空可验证方法与环境边界条件；
   - 置信度门槛严格限制（$\ge 0.5$）。
3. **反馈闭环机制（不硬删除）**：
   - 采用后正向反馈提升经验权重与置信度；
   - 负向反馈降低权重与置信度，当衰减至临界值（$< 0.3$）时降级为 `Deprecated`，绝不物理硬删除，保留完整历史审计轨迹。
4. **环境边界感知（Boundary-Aware Recall）**：
   - 经验严格限定适用环境（如 `["linux", "k8s"]`）；
   - 执行检索时，若当前运行环境与经验环境不匹配，检索结果显式标记 `env_mismatch = true` 并附带明确警告，同时实施分数惩罚，严禁伪称适用；
   - **两种模式，按后果严重程度选**：
     - **降权模式（默认，`strict_env = false`）**：分数 ×0.5 + 显式告警，
       结果仍然返回，交由 Agent 判别；
     - **拒绝模式（`strict_env = true`）**：环境不匹配的经验**直接不返回**。
       降权只是**排序提示**——Agent 仍可能误取；而把"只适用于 kafka 的处置步骤"
       用在 JVM 上这类**有安全后果**的场景，需要的是硬规则而不是排序。
       检索工具 `working_experience_search` 暴露 `strict_env` 参数，
       语义与测试见 `crates/mem-core/src/working_memory_service.rs`。
5. **账户沙箱强隔离**：
   - 账户沙箱体系下，跨命名空间任务与经验关联直接阻断（Fail-Closed）。

---

## 2. 状态机与生命周期设计

### 2.1 任务生命周期状态机 (`TaskStatus`)

```
   [start_task]
        │
        ▼
     Running ───(finish_task)───► Success
        │
        ├───(finish_task)───────► Failed
        │
        └───(finish_task)───────► Cancelled
```

- **幂等性保障**：对已完成（Success / Failed / Cancelled）的任务再次调用 `finish_task`，保持幂等返回，不覆盖已有的结果事实与完成时间。

### 2.2 经验生命周期状态机 (`ExperienceStatus`)

```
   [distill_experience]
        │
        ├── (通过质量门槛且直接激活) ──► Active ◄──┐
        │                                 │       │ (正反馈回升)
        │ (需进一步试用)                   │       │
        └───► Candidate ──────────────────┘       │
                  │ (负反馈衰减)                  │
                  ▼                               │
              Deprecated ─────────────────────────┘
                  │ (人工或周期策略归档)
                  ▼
               Archived
```

- **Candidate**：提炼出的候选经验，需在后续任务中验证。
- **Active**：经过验证或高置信度直接激活的可靠经验。
- **Deprecated**：因多次负反馈或环境过时导致评分低于 0.3 的废弃经验，检索时默认降权。
- **Archived**：显式归档状态，检索默认完全排除。

---

## 3. 质量与安全门槛规范 (`validate_experience_candidate`)

提炼入库前必须通过四道规则检查：

| 门槛项 | 检验规则 | 违例处理 |
| :--- | :--- | :--- |
| **凭证扫描** | 正则与特征检测 `sk-[a-zA-Z0-9]`, `ghp_`, `AKIA`, `PRIVATE KEY`, `Bearer ` 等敏感字符串 | 立即抛出 `HubError::InvalidArgument("经验候选包含敏感凭证...")`，直接阻断 |
| **通用性检验** | 拒绝包含“本次任务”、“我刚才”、“我修复了”、“临时测试”等强局部会话叙述 | 抛出 `HubError::InvalidArgument("经验描述过于针对单次任务局部...")` |
| **可验证性检验** | 校验 `validation` 字段非空且有效长度 $\ge 5$ 字符 | 拒绝无明确验收方式的经验伪装 |
| **边界条件检验** | `boundary_envs` 或 `preconditions` 至少一项非空 | 严禁无边界的泛化主张 |
| **置信度底线** | `confidence` 必须落在 $[0.5, 1.0]$ 区间内 | 过滤低质量推测 |

---

## 4. 存储架构与 Migration 7

通过 PostgreSQL Migration `20261018000007_working_memory_tasks_and_experiences.sql` 建立核心数据表：

```sql
-- 1. 任务事实表
CREATE TABLE memory_tasks (
    id UUID PRIMARY KEY,
    namespace VARCHAR(128) NOT NULL,
    task TEXT NOT NULL,
    scene VARCHAR(32) NOT NULL DEFAULT 'Working',
    status VARCHAR(32) NOT NULL DEFAULT 'Running',
    summary TEXT,
    artifacts JSONB NOT NULL DEFAULT '[]'::jsonb,
    validation_steps JSONB NOT NULL DEFAULT '[]'::jsonb,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    started_at TIMESTAMPTZ NOT NULL,
    completed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL
);

-- 2. 经验记忆表
CREATE TABLE memory_experiences (
    id UUID PRIMARY KEY,
    namespace VARCHAR(128) NOT NULL,
    task TEXT NOT NULL,
    search_keys JSONB NOT NULL DEFAULT '[]'::jsonb,
    solution TEXT NOT NULL,
    preconditions JSONB NOT NULL DEFAULT '[]'::jsonb,
    boundary_envs JSONB NOT NULL DEFAULT '[]'::jsonb,
    failure_modes JSONB NOT NULL DEFAULT '[]'::jsonb,
    validation TEXT NOT NULL,
    confidence REAL NOT NULL DEFAULT 1.0,
    status VARCHAR(32) NOT NULL DEFAULT 'Active',
    feedback_score REAL NOT NULL DEFAULT 1.0,
    created_at TIMESTAMPTZ NOT NULL
);

-- 3. 任务与经验多对多关联映射表
CREATE TABLE memory_task_experiences (
    task_id UUID NOT NULL REFERENCES memory_tasks(id) ON DELETE CASCADE,
    experience_id UUID NOT NULL REFERENCES memory_experiences(id) ON DELETE CASCADE,
    relation_kind VARCHAR(32) NOT NULL DEFAULT 'derived',
    created_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (task_id, experience_id)
);
```

---

## 5. 异步提炼作业与重试机制 (`ExperienceDistillJob`)

在 `crates/worker/src/experience_job.rs` 中提供异步提炼执行器：
- **瞬态错误重试**：遇到数据库连接异常、锁等待等瞬态可重试错误，以指数退避策略执行重试（默认 3 次）；
- **门槛拦截不重试**：当候选因敏感凭证泄露或通用性不足被拒时，直接记录拒绝报告，不进行无意义重试；
- **幂等查重**：若同一命名空间已存在相同任务模式和解决方案的经验，作业自动复用已有经验并补充源任务关联。

---

## 6. 备份与还原 (Backup Payload v3)

- **版本升级**：`BACKUP_PAYLOAD_VERSION` 升级至 `3`；
- **新增内容**：快照包含 `tasks`, `experiences`, `task_experiences` 全量数据；
- **向前兼容**：校验和验证支持 `checksum_v3` -> `checksum_v2` -> `checksum_v1` 渐进式回退兼容，保障历史 v1/v2 快照可无缝还原。

[![powered by KimiCode](../../.github/assets/kimi-badge.svg)](https://www.kimi.com/code)
