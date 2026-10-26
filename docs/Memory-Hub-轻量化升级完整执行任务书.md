# Memory Hub 轻量化升级完整执行任务书

> **文档性质：归档任务书 (Reference)** —— 本文件是 2026-10-21 轻量化升级的原始执行任务书，
> 用于追溯当时的约束、阶段划分与验收口径，不再是当前开发指令。实际交付与最新口径以
> [`upgrade/LIGHTWEIGHT_AUDIT.md`](./upgrade/LIGHTWEIGHT_AUDIT.md)、
> [`upgrade/LIGHTWEIGHT_BASELINE.md`](./upgrade/LIGHTWEIGHT_BASELINE.md)、
> [`upgrade/LIGHTWEIGHT_EXECUTION_REPORT.md`](./upgrade/LIGHTWEIGHT_EXECUTION_REPORT.md)
> 与根目录 README 为准。
>
> 文中命令按当时的仓库布局书写；评测 target 实际位于子 crate，下文已用
> `-p retrieval` / `-p consolidation` 标注。

| 项目 | 内容 |
| :--- | :--- |
| 项目地址 | `https://cnb.cool/toroyxxx/memory-hub` |
| 实施方式 | 基于现有 Rust Workspace 原地升级 |
| 目标版本 | Lightweight Deployment & Runtime Optimization V1 |
| 执行原则 | 先审计、后修改；小步提交；先保证兼容，再做性能优化 |

---

## 0. 给编程 Agent 的总指令

你是本项目的 Rust 主程、数据库工程师、安全审计工程师和测试负责人。

请在当前实际检出的 Memory Hub 仓库中，执行一轮完整的轻量化升级。不要仅编写建议或更新文档，必须完成源码审计、必要代码修改、自动化测试、性能评测、文档更新和验收报告。

### 0.1 目标部署形态

提供两种部署配置，但只维护一套核心业务实现。

**A. Personal Lite：个人轻量部署**

- 面向单机、个人使用、少量 AI Agent。
- 默认使用本地规则整合，不调用嵌入模型进行整合。
- 支持基础关键词检索。
- 允许显式启用可选向量检索。
- 保留 Chat、Working、Coding 三类认知记忆的存取和 MCP 工具。
- 允许将后台整合、统计和维护任务改为按需执行。
- 默认关闭非必要的周期性扫描和额外计算。
- 保留 PostgreSQL/pgvector 正式存储能力。
- `memory://local` 只能作为明确标注的临时开发模式，不得冒充持久化轻量生产模式。

**B. Production Full：完整生产部署**

- 保留现有 `mcp-server`、`auth-server`、`backup-worker` 三个进程边界。
- 保留 PostgreSQL/pgvector、多账户隔离、OAuth 2.1、ACL、备份和恢复。
- 保留完整检索、向量辅助候选发现、认知记忆和后台维护能力。
- 支持独立部署、水平扩展、监控、限流和任务调度。
- 不允许因为个人版的简化而削弱生产版安全能力。

两种模式必须复用现有领域逻辑、数据模型、数据库访问层和授权机制。禁止复制一套个人版业务实现。

### 0.2 不可违反的约束

1. 不重写项目架构，不无故合并 crate。
2. 不破坏既有 MCP 工具、参数、返回结构和权限语义。
3. 不破坏 OAuth 2.1、PKCE S256、刷新令牌轮换、账户隔离、namespace ACL 和撤销权限的即时生效能力。
4. 不破坏 PostgreSQL/pgvector、现有数据库迁移链和备份恢复兼容性。
5. 不删除证据链、历史版本、时间有效性、冲突记录和整合提案。
6. 不引入默认必需的生成式 LLM。
7. 不使用生成式 LLM 默认改写、总结或合并记忆。
8. 不得将 Mock/Pseudo embedding 的结果作为真实语义能力的验收结果。
9. 不得通过降低测试覆盖率、跳过测试或放宽安全断言来获得通过。
10. 不得在没有实测数据的情况下声称降低了内存、延迟或依赖体积。
11. 不得未经核查修改历史 migration、备份快照格式或现有数据语义。
12. 不得臆造文件、类型、接口、Cargo feature 或数据库字段。

---

# P0：真实源码审计与性能基线

## P0.1 检查当前工作区

开始执行前：

```bash
git status --short
git branch --show-current
git rev-parse HEAD
git log -10 --oneline --decorate
git diff --stat
```

要求：

- 记录真实 HEAD 和工作区是否干净。
- 不覆盖用户尚未提交的修改。
- 若工作区存在未提交变更，先区分用户修改与本任务修改。
- 创建独立工作分支；如果当前环境不允许创建分支，至少保留明确的基线提交和补丁。
- 不得把历史实施文档中的提交号直接当作当前 HEAD。

## P0.2 核对 Cargo Workspace 和 Features

重点检查：

- `Cargo.toml`
- `Cargo.lock`
- `crates/` 下实际存在的所有 `Cargo.toml`
- `.github/workflows/` 下真实 CI 配置
- `deny.toml`
- `Dockerfile`
- `docker-compose.yml`
- `.env.example`
- `GEMINI.md`

执行：

```bash
cargo metadata --format-version 1 --no-deps
cargo tree --workspace
cargo tree --workspace -e features
cargo tree --workspace -d
cargo tree --workspace -e features -i tikv-jemallocator
```

最后一条如果依赖或 feature 不存在，记录实际情况，不得为使命令通过而擅自添加依赖。

形成审计清单：

| 项目 | 必须查明的内容 |
|---|---|
| Workspace | 实际 crate、依赖方向、循环依赖风险 |
| Features | 默认 feature、可选 feature、各 feature 的依赖闭包 |
| Embedding | Provider 创建位置、实际调用链、缓存与失败行为 |
| Retrieval | 关键词、向量、排序、MMR、过滤的真实执行顺序 |
| Consolidation | 候选发现、关系判定、自动合并、提案和审计 |
| Cognitive | Chat / Working / Coding 实际实现、共享代码和重复逻辑 |
| Storage | MemoryStore、AccountScopedStore、PostgreSQL 与内存实现 |
| Worker | 任务注册、调度、并发、重复执行和失败恢复 |
| MCP | 真实工具注册、输入输出 schema、权限检查 |
| Backup | 真实快照版本、校验、恢复、失败回滚 |
| CI | 当前实际执行的测试和 feature 组合 |

**验收：** 提交 `docs/upgrade/LIGHTWEIGHT_AUDIT.md`，记录每项结论对应的实际文件和符号。该文档必须反映本次检出的真实源码，而不是复制 README 的架构描述。

## P0.3 核对数据库迁移

检查 `crates/store/migrations/` 下全部实际 migration 文件、迁移注册表和迁移执行器。

重点验证：

- migration 的真实数量、顺序和幂等性。
- `MIGRATIONS` 注册与文件是否一一对应。
- Migration 9 的复合外键及账户/namespace 一致性约束是否仍然存在。
- 最新 A/B 整合实现中 `integration_proposals.metadata` 的迁移是否已登记。
- 新增的认知记忆字段和关系是否有真实 schema 支撑。
- Docker 初始化路径与 Rust migration runner 的执行顺序是否一致。
- 历史数据库从最早受支持版本升级到当前版本是否可连续完成。

禁止仅根据 migration 编号推断 schema 正确。

## P0.4 记录性能基线

至少记录：

- Workspace 构建耗时。
- 默认 feature 和实际生产 feature 的构建耗时。
- 三个二进制文件的大小。
- 个人模式启动时间。
- 进程启动后的空闲 RSS。
- 代表性读写的 p50 / p95 延迟。
- 关键词检索与混合检索延迟。
- 每次整合处理的记忆数量、候选数量和嵌入调用数。
- 数据库往返次数、批处理规模和超时率。
- 备份和恢复耗时。
- Chat / Working / Coding 的基本操作延迟。

如果缺少真实 PostgreSQL、真实嵌入端点或可复现的数据集，标记为 `NOT MEASURED`，不能用内存存储或伪向量数据替代生产结论。

输出：

`docs/upgrade/LIGHTWEIGHT_BASELINE.md`

---

# P1：双部署模式与运行时轻量化

## P1.1 部署模式设计

优先复用现有配置体系，不增加无必要的配置框架。

建议的外部配置语义：

```text
HUB_DEPLOYMENT_PROFILE=lite|production
HUB_EMBED_PROVIDER=mock|http|<现有实际支持的 provider>
HUB_CONSOLIDATION_MODE=rules|hybrid
```

以上配置项必须先检查现有实现：

- 如果已存在，沿用原有名称和默认值。
- 如果不存在，先确定是否确实需要新增。
- 不得同时保留两个表达相同含义的配置变量。
- 不得让 `HUB_DEPLOYMENT_PROFILE` 取代已有授权或安全配置。

### Lite 模式

1. 默认关闭非必要后台周期任务。
2. 允许用户手动触发整合、统计和清理。
3. 默认采用关键词检索。
4. 若启用真实向量检索，必须显式配置 Provider。
5. 纯规则整合不得触发 embedding 请求。
6. 保留现有三类认知记忆接口。
7. 保留所有持久化、备份、恢复和权限约束。
8. 必须在健康状态中明确说明向量能力是否可用。

### Production 模式

1. 保留现有三个进程及其职责。
2. 保留后台任务的生产调度能力。
3. 保留向量召回和完整检索流程。
4. 保留完整备份、恢复、权限与监控能力。
5. 不得依赖只有 Lite 模式才有的内存状态。
6. 确保多实例部署时任务幂等和并发安全。

## P1.2 后台任务按需执行

审计 `crates/worker/` 下实际任务注册和调度代码。

优先考虑：

- 将可手动触发的统计计算从高频周期任务中分离。
- 让整合任务使用有界批次。
- 对过期清理和历史扫描使用分页。
- 避免每次 API 调用重复执行全库统计。
- 在没有待处理数据时快速退出。
- 明确任务失败后的重试、超时和恢复行为。
- 防止同一 namespace 在多个进程中被重复整合。

不得将备份、数据完整性检查或安全维护直接删除。

## P1.3 Cargo 依赖与构建优化

在审计结果基础上逐项处理：

- 不被生产代码使用的传递依赖。
- 可合理拆分的可选模型客户端依赖。
- 只用于测试的依赖是否泄漏到生产依赖图。
- 重复的序列化、HTTP 客户端和错误处理实现。
- 不必要的全 feature 构建成本。
- 已存在的分配器或编译优化是否真正有收益。

禁止为了减少 crate 数量而合并独立职责。

禁止未经实测就删除 jemalloc、TLS、数据库驱动或其他安全相关依赖。

### P1 验收

- Lite 与 Production 都能编译。
- 原有部署方式继续可用。
- Lite 默认不启动非必要的周期性工作。
- Production 原有任务与安全能力保持完整。
- 不新增必需的生成式 LLM 依赖。
- 输出修改前后依赖树、二进制大小、启动时间和 RSS 对比。

---

# P2：检索流程按需执行

## P2.1 保留现有检索契约

现有公开设计包括：

- 关键词检索。
- pgvector 语义召回。
- 数据库层过滤。
- 五维评分。
- MMR 去重。
- 可靠性阈值 `min_score`。
- 无可靠匹配时返回明确的空结果。

不得通过删除这些能力的对外契约来实现轻量化。

## P2.2 引入按需检索策略

建议的逻辑：

```text
请求
  ├─ 验证账户、namespace、权限和查询参数
  ├─ 精确业务键 / 高确定性关键词路径
  ├─ 判断是否需要语义召回
  │    ├─ 不需要：关键词路径
  │    └─ 需要且 Provider 可用：混合召回
  ├─ 数据库过滤
  ├─ 统一评分
  ├─ 必要时执行 MMR
  ├─ min_score 与域外拒绝
  └─ 返回结果与能力状态
```

这是目标执行流程，不是要求照搬成新的代码结构。必须先审计当前调用链，尽量复用现有实现。

### 优化要求

1. 精确 key、路径、版本号等查询优先利用确定性检索。
2. 不得在尚未执行权限过滤时向模型发送记忆内容。
3. 不得为了加速而取消时间有效性、账户隔离、scene、tag、layer 和环境过滤。
4. 关键词路径与向量路径共用后续评分和结果契约。
5. 仅当 MMR 对当前候选集有实际价值时才执行。
6. 低分结果不能为了凑满 `top_k` 而强行返回。
7. 向量服务不可用时按现有降级策略执行，并明确报告语义能力不可用。
8. 伪向量不能被当作真实语义相似度参与生产结论。

## P2.3 基准与校准

沿用仓库现有 Bench v2：

```bash
cargo test -p retrieval --test eval_bench -- --nocapture

HUB_EVAL_ABLATIONS=1 \
  cargo test -p retrieval --test eval_bench -- --ignored --nocapture

HUB_EVAL_CALIBRATE=1 \
  cargo test -p retrieval --test eval_bench -- --ignored --nocapture
```

真实嵌入端点评测必须使用仓库当前支持的配置变量，并启用真实语义强制验收。

不能为了让测试通过而自动更新 baseline。

### P2 验收

- 检索质量不能发生未解释的显著回归。
- 时间、namespace、scene、环境等硬约束违规数为零。
- 域外误召回率、首位误命中率满足项目已有绝对门槛。
- 报告 p50 / p95、Recall@K、MRR、nDCG、调用量及各类查询的分层结果。
- 明确区分伪向量和真实嵌入模型的结果。

---

# P3：统一 A/B 整合中枢

## P3.1 目标

保留现有两种模式：

- B：`rules`，默认模式。
- A：`hybrid`，向量辅助候选发现。

根据公开实施文档，现有相关代码包括：

- `crates/common/src/consolidation.rs`
- `crates/consolidation/src/candidates.rs`
- `crates/consolidation/src/candidates/rules.rs`
- `crates/consolidation/src/candidates/hybrid.rs`
- `crates/mem-core/src/fingerprint.rs`
- `crates/mem-core/src/testing.rs`

这些路径是现有实施文档记录的文件；Agent 必须在当前检出中逐一确认，文件若有变动，应以实际代码为准。

## P3.2 统一处理链路

目标：

```text
配置与请求
    ↓
确定 requested_mode / effective_mode
    ↓
账户与 namespace 授权过滤
    ↓
候选发现
    ├─ RulesCandidateFinder
    └─ HybridCandidateFinder
           └─ 规则候选 ∪ 向量候选
    ↓
候选去重与证据规范化
    ↓
共享决策层
    ├─ 关系分类
    ├─ 冲突判断
    ├─ 证据与历史检查
    ├─ 自动合并资格判断
    └─ 提案治理
    ↓
合并 / 提案 / 保持分立
```

禁止为 A、B 各自维护不同的最终合并规则。

## P3.3 B 模式强约束

`HUB_CONSOLIDATION_MODE=rules` 为默认值。

即使配置了真实 embedding Provider：

- B 也不得调用 embedding 服务。
- B 不得为了合并后重新生成向量而发起 embedding 请求。
- B 的调用计数必须严格为零。
- 不能访问外部生成式 LLM。
- 不得上传记忆正文到外部模型服务。

B 模式仅使用现有本地规则、指纹、结构化字段与词法候选发现能力。

## P3.4 A 模式强约束

A 模式只允许使用 embedding 协议进行候选发现。

禁止：

- 调用生成式 LLM 重写记忆。
- 调用生成式 LLM 总结记忆。
- 调用生成式 LLM 直接决定是否合并。
- 把向量相似度直接视为事实相同。
- 将不同版本、不同数字、否定关系、不同路径或不同环境的记忆自动合并。

A 模式必须具备：

- 明确的嵌入调用计数。
- 每次扫描的候选数量上限。
- 超时与失败降级策略。
- requested/effective 模式状态。
- dry-run。
- 真实嵌入模型评测。
- 语义阈值未标定时的明确告警。

## P3.5 自动合并安全规则

只允许符合严格重复条件的候选自动合并。

至少包括：

- 原文逐字节相同。
- 经保守规范化后完全相同。

语义近重复、版本更新、冲突、词法近匹配或不确定候选必须保持独立或生成待审提案。

保守规范化不能随意改变：

- 大小写。
- 否定词。
- 数字和单位。
- 日期和时间。
- 版本号。
- 文件路径与 URL。
- 代码标识符。
- 环境标签。

合并操作必须保留：

- 证据来源。
- 版本历史。
- Supersedes 关系。
- 必要的提案元数据。
- 审计记录。
- 账户和 namespace 边界。

## P3.6 整合效率

检查现有整合是否重复扫描同一候选集、重复查询数据库或多次计算相同指标。

优先复用一次候选扫描结果完成：

- 重复检测。
- 冲突检测。
- 提案统计。
- 候选来源统计。

所有批处理必须有上限，避免全库 O(n²) 处理成为默认运行方式。

若使用数据库部分唯一索引解决跨进程重复提案，必须先审计现有重复数据并设计可回滚迁移，不得直接在生产库创建可能失败的唯一索引。

## P3.7 整合验收

运行：

```bash
cargo test -p consolidation --test consolidation_eval -- --nocapture
```

测试必须断言：

- B 模式 embedding 调用数为零。
- A 模式生成式 LLM 调用数为零。
- 错误自动合并数为零。
- 版本、否定、数字、环境等硬约束违规数为零。
- 跨账户越权数为零。
- 重复扫描具有可解释的调用计数。
- 整合失败不会造成部分数据被静默破坏。
- dry-run 不产生写入。
- 重复执行具备幂等性。

只有配置真实嵌入端点时，才能验收 A 在语义改写样本上的召回提升。

---

# P4：保留 Chat / Working / Coding 并简化实现

## P4.1 Chat 认知记忆

保留现有：

- Fact。
- Summary。
- Pattern。
- Intention。
- 时间有效性。
- 证据关联。
- 提案审核。
- 认知记忆 MCP 工具。

重点审计重复的输入校验、证据转换、时间处理和提案状态转换。

不得删除历史证据，也不得把未经确认的意图自动变成确定事实。

默认不增加生成式 LLM。需要模型参与的现有流程必须保持显式可配置，并明确报告其是否实际调用模型。

## P4.2 Working 记忆

保留：

- WorkingFact。
- Experience。
- 任务开始与完成。
- 经验适用条件。
- 环境边界。
- 经验反馈。
- 失效与归档。

优化方向：

- 统一重复的生命周期转换逻辑。
- 对任务与经验列表使用分页。
- 避免每次经验搜索都加载无关历史。
- 保持环境不匹配的警告与降权行为。
- 保持归档而非物理删除的历史语义。

## P4.3 Coding 记忆

保留：

- CodingThread。
- CodingRevision。
- 真实 Git diff 来源。
- 幂等记录。
- 安全相对路径校验。
- 敏感信息扫描与脱敏。

禁止通过模型自行描述来代替真实 Git diff。

禁止为了减少存储而删除仍被历史链引用的版本记录。

## P4.4 MCP 兼容性

公开 README 记录了 28 个 MCP 工具，包括 18 个基础工具和 10 个认知相关工具。

Agent 必须从真实注册表重新枚举工具，不得直接将 README 数量作为正确性依据。

对比升级前后的：

- 工具名称。
- 参数名称与类型。
- 必填参数。
- 默认值。
- 返回结构。
- 错误类型。
- 权限 scope。
- 账户与 namespace 过滤。
- 旧客户端不传新增可选参数时的行为。

禁止在轻量模式中删除认知工具。对于不适用的后台能力，应返回明确的状态或受控错误，而不是让工具消失。

## P4 验收

- Chat / Working / Coding 基本读写与搜索回归通过。
- 所有历史 MCP 工具保持兼容。
- 新增或调整的功能有独立测试。
- 跨账户、跨 namespace、权限撤销与历史证据测试全部通过。
- 不允许将认知记忆简化为普通文本拼接后失去原有结构语义。

---

# P5：备份、恢复、迁移、性能和发布验收

## P5.1 数据库迁移

默认原则：本轮轻量化不应为了部署模式本身新增数据库迁移。

只有确实需要持久化新状态、约束或索引时才增加 migration。

若新增 migration：

1. 使用当前项目的真实命名和注册方式。
2. 保持历史迁移不可变。
3. 尽可能向后兼容。
4. 对现有数据做迁移前检查。
5. 提供 SQL 回滚或明确不可逆说明。
6. 覆盖空库、旧库、重复执行和失败恢复。
7. 同步验证 PostgreSQL 与项目支持的其他存储实现。
8. 确认 Docker 初始化路径与 Rust migration runner 一致。

重点复测：

- Migration 9 的复合外键与同 namespace / 账户约束。
- 最新整合提案元数据 migration。
- 所有认知记忆表的关系约束。
- 历史 schema 的连续迁移。
- 迁移注册守卫。

## P5.2 备份与恢复

必须验证：

- SHA-256 校验失败时拒绝恢复。
- `dry_run` 完全只读。
- `merge` 不覆盖不相关的数据。
- `replace` 执行前创建安全备份。
- 恢复中途失败时回滚到一致状态。
- 备份中的账户与 namespace 不得越权导入。
- 历史快照版本仍可读取。
- 提案、认知记忆、关系和证据的恢复行为有明确测试。
- 恢复后索引、外键和引用关系保持一致。

检查现有 v1–v4 备份格式兼容性，不得因为本轮优化直接改变快照版本或移除旧格式解析器。

如要更改备份格式，必须单独提供格式迁移方案、旧版样本测试和恢复回滚路径。

## P5.3 PostgreSQL 集成测试

必须在真实 PostgreSQL/pgvector 环境中执行，而不只依赖内存实现。

验证：

- 全量 migration。
- 多账户隔离。
- 同名 namespace 的隔离。
- 复合外键。
- 向量索引与查询。
- ACL 授权和撤销。
- 并发整合。
- 重复提案。
- 备份校验与恢复失败回滚。
- 认知记忆写入和历史查询。

如果本机无法运行 PostgreSQL，必须通过 CI 或可用的真实数据库环境执行，并在报告中提供实际日志。无法执行时标记 `BLOCKED`，不得标记通过。

## P5.4 统一验收命令

至少执行：

```bash
cargo fmt --all -- --check

cargo test --workspace

cargo clippy --workspace --all-targets -- -D warnings

cargo doc --no-deps --workspace

cargo test -p retrieval --test eval_bench -- --nocapture

cargo test -p consolidation --test consolidation_eval -- --nocapture
```

再按实际仓库情况执行：

- 真实 PostgreSQL/pgvector 集成测试。
- 迁移连续升级测试。
- v1–v4 备份恢复测试。
- 真实 embedding 评测。
- Lite 部署启动与功能测试。
- Production Compose 部署测试。
- 所有项目现有 CI feature 组合。

如果某个测试目标名称已经变化，应先核对真实 Cargo target，再执行实际存在的命令。不能创建空测试来伪装验收。

## P5.5 发布文档

根据实际修改更新：

- `README.md`
- `README.en.md`
- `.env.example`
- `docker-compose.yml`
- `docs/upgrade/EVALUATION.md`
- `docs/upgrade/CONSOLIDATION_AB.md`
- `docs/upgrade/LIGHTWEIGHT_AUDIT.md`
- `docs/upgrade/LIGHTWEIGHT_BASELINE.md`

最后两个文件若不存在，应按实际仓库规范新增。

文档必须说明：

- Lite 与 Production 的区别。
- 向量模型如何启用和关闭。
- B 模式为什么保证零 embedding 调用。
- A 模式的数据出站风险。
- 认知记忆的模型依赖和运行边界。
- 后台任务如何手动触发或调度。
- 如何执行测试。
- 如何回滚应用版本。
- 如何验证备份和恢复。

---

# 6. 建议的文件级改造清单

以下是候选检查范围，不代表这些文件都必须修改。

| 文件或目录 | 处理要求 |
|---|---|
| `Cargo.toml` | 审计 workspace 与 feature；仅在证实有收益时修改 |
| `Cargo.lock` | 仅随真实依赖变更更新 |
| `.env.example` | 增加必要的部署模式说明，避免重复配置 |
| `docker-compose.yml` | 增加个人与生产部署组合；保留原服务和数据卷兼容 |
| `Dockerfile` | 仅在实测证明有构建或镜像收益时调整 |
| `README.md` / `README.en.md` | 同步部署方式与模型行为 |
| `crates/common/src/consolidation.rs` | 核查模式配置与状态定义是否可复用 |
| `crates/consolidation/src/candidates.rs` | 核查候选抽象是否存在重复实现 |
| `crates/consolidation/src/candidates/rules.rs` | 核查纯规则模式的调用路径 |
| `crates/consolidation/src/candidates/hybrid.rs` | 核查向量候选是否有界、可计数 |
| `crates/mem-core/src/fingerprint.rs` | 确保保守规范化与事实安全边界 |
| `crates/mem-core/src/testing.rs` | 核查 embedding 计数器与测试设施 |
| `crates/store/migrations/` | 核对全部迁移；无必要不新增 schema |
| `crates/worker/` | 核查后台任务、批次、重复执行和失败恢复 |
| `crates/mcp/` | 核查 MCP 工具兼容性与权限 |
| `docs/upgrade/` | 增加审计、基线和执行结果记录 |

所有新增路径和具体 Rust 符号必须由 Agent 在真实仓库中确认。若目录布局已变化，应更新本清单并在实施报告中说明对应关系。

---

# 7. 性能优化目标与量化要求

先测量，后设定最终目标。禁止用未经验证的数字作为承诺。

初步工程目标：

| 指标 | 目标 |
|---|---|
| B 模式 embedding 调用 | 严格为 0 |
| A 模式生成式 LLM 调用 | 严格为 0 |
| 自动错误合并 | 0 |
| 跨账户越权 | 0 |
| 时间、版本、否定等硬约束违规 | 0 |
| Lite 默认后台周期扫描 | 不启动非必要任务 |
| 检索硬约束 | 不得退化 |
| 旧 MCP 工具兼容 | 100% |
| 历史备份格式兼容 | 全部受支持版本通过 |
| 构建、启动、RSS、延迟 | 必须给出前后实测数据 |

对构建耗时、二进制大小、空闲 RSS、检索 p95 和整合处理吞吐量，先以 P0 基线为准。建议把可复现的改进目标设为：对明确的热点指标至少改善 10%，同时所有质量和安全门禁保持通过。

如果没有达到 10%，不应为了完成数字而牺牲正确性；应报告真实结果、瓶颈与下一步建议。

---

# 8. 回滚方案

## 8.1 代码回滚

- 每个阶段独立提交。
- 不覆盖用户原有修改。
- 记录基线 HEAD 和每阶段提交。
- 如某阶段测试失败，优先回滚该阶段，不连带撤销已验收的阶段。
- 不通过修改历史 migration 的方式回滚。

## 8.2 部署回滚

- 保留上一版本镜像或二进制。
- 记录启动环境变量和部署配置。
- 保留现有数据库卷。
- Lite 与 Production 使用兼容的数据模型，不建立互不兼容的独立 schema。
- 先回滚应用版本，再根据 migration 是否可逆决定是否需要数据库回滚。

## 8.3 数据库回滚

如果没有新增 migration，不应执行任何破坏性 schema 回滚。

如果新增了 migration：

- 提供回滚 SQL。
- 回滚前生成并验证安全备份。
- 对不可逆变更明确说明。
- 不得自动删除历史数据、证据或提案。
- 回滚后执行完整迁移、读写、授权与恢复测试。

## 8.4 功能降级

- 向量服务不可用：按配置降级为关键词检索或返回明确错误。
- A 模式不可用：按配置切换到 B 或停止整合。
- 后台 Worker 未启动：明确标识待执行任务，不得静默丢弃。
- 认知功能出错：不得破坏基础记忆读写和历史数据。

---

# 9. 最终交付物

Agent 必须提供：

1. `docs/upgrade/LIGHTWEIGHT_AUDIT.md`：实际源码、Cargo features、迁移和测试审计。
2. `docs/upgrade/LIGHTWEIGHT_BASELINE.md`：优化前后性能基线。
3. `docs/upgrade/LIGHTWEIGHT_EXECUTION_REPORT.md`：逐阶段实施记录。
4. 必要的源码修改及测试。
5. 如确实需要，附数据库 migration 和回滚 SQL。
6. Lite / Production 两种部署配置。
7. 更新后的中文与英文部署文档。
8. 实际测试日志摘要。
9. 已知限制、未完成项目和风险清单。
10. 可复现的验收命令。

最终报告至少包含：

```text
[ ] 源码审计完成
[ ] Cargo Workspace / Features 审计完成
[ ] 数据库迁移审计完成
[ ] Lite 部署通过
[ ] Production 部署通过
[ ] OAuth 2.1 / ACL 回归通过
[ ] MCP 兼容性回归通过
[ ] B 模式零 embedding 调用
[ ] A 模式零生成式 LLM 调用
[ ] Chat / Working / Coding 回归通过
[ ] Bench v2 检索评测通过
[ ] 整合质量评测通过
[ ] PostgreSQL/pgvector 集成测试通过
[ ] v1-v4 备份兼容性通过
[ ] 恢复失败回滚测试通过
[ ] 性能对比完成
[ ] 文档更新完成
[ ] 回滚方案验证完成
```

每一项必须填写：

- `PASS`：实际执行且通过。
- `FAIL`：实际执行但失败。
- `BLOCKED`：因环境或依赖无法执行。
- `NOT RUN`：尚未执行。

不得将 `BLOCKED` 或 `NOT RUN` 计入通过数量。

---

# 10. 执行顺序与完成标准

必须按以下顺序推进：

1. P0：审计并建立基线。
2. P1：先完成部署模式和低风险运行时优化。
3. P2：优化检索执行路径并验证质量。
4. P3：验证并收敛 A/B 整合中枢。
5. P4：保留认知记忆和 MCP 兼容性。
6. P5：完成数据库、备份、恢复、真实 PostgreSQL、性能和发布验收。

每个阶段结束后先执行相应测试，再进入下一阶段。遇到安全回归、数据损坏风险或兼容性失败时，必须停止后续高风险修改。

**最终完成标准：** 同一套 Memory Hub 核心代码可以支持个人轻量部署和完整生产部署；默认规则整合不产生 embedding 调用；A 模式只通过 embedding 扩大候选发现范围；Chat、Working、Coding、MCP、OAuth 2.1、ACL、PostgreSQL/pgvector、历史证据和备份恢复全部保留；所有性能改进均有可复现的实测证据。

开始执行时，第一条输出应是当前真实 HEAD、工作区状态、审计发现和基线测试结果，而不是直接修改代码。

<!-- kimi-badge -->
[![powered by KimiCode](../.github/assets/kimi-badge.svg)](https://www.kimi.com/code)
