# 记忆中枢（Memory Hub）

> 厂商中立的标准 MCP 记忆服务：为第三方个人助理提供个性化用户记忆。

**记忆中枢**是一个以 **MCP（Model Context Protocol）** 服务形式运行的个性化记忆系统。它从助理的使用情况中提取要素，持续完善用户记忆与表达规范，使任何一个符合标准的 AI 助理都能"记住"用户——而无需为每个厂商单独适配。

整个程序（核心服务、授权服务器、用户中心、编审工作台、离线任务、评测台）**全部使用 Rust 实现**，构建于单一 Cargo Workspace 之上。

- **定位**：轻量模式。中枢只返回记忆包、规范包与评分量表，由助理自己的 LLM 执行推理，中枢在线路径不做任何生成式推理。
- **面向**：仅面向中国用户。
- **协议**：MCP 2026-07-28（回退兼容 2025-11-25），Streamable HTTP（主）+ stdio 桥接。

## 核心特性

- **厂商中立**：核心是标准 MCP 服务，只依赖 `tools` 能力；不含任何厂商私有字段或私有协议，客户端不支持 resources/prompts 时自动降级。
- **三模块架构**：ToM（心智理论）、域代理、响应代理；域代理内热插拔六个 skill。
- **混合检索**：元数据过滤 + 全文检索（FTS）+ 嵌入向量（pgvector）双路召回，MMR 重排。
- **来源可溯**：每条记忆标记 `stated`（用户明说）/ `observed`（行为观察）/ `inferred`（推断），推断不经用户确认不得升格。
- **人在环**：全局规范的变更必须经双人审批编审；收紧可快，放宽须严。
- **可解释、可回滚**：每次装配记录 `pack_version`，每条规范有版本与 TTL，可原子热替换与一键回滚。
- **用户可见可控**：用户可查看、修改、暂停、删除、导出自己的记忆。
- **安全优先**：全仓库强制 `#![forbid(unsafe_code)]`；不持久化对话原文，只存派生特征；日志强制脱敏。

## 设计原则

1. **在线查表、离线生成**：运行时只做检索、装配、校验；规范的生成与进化放在离线并有人审。
2. **最小披露**：只发当前场景需要的记忆，原始对话尽量不进中枢。
3. **来源可溯**：推断（`inferred`）不经用户确认不得升格。
4. **硬软分级**：硬约束是底线，软约束是倾向；个人记忆与关系规范默认是软约束。
5. **规范不迎合**：个人记忆只调整"怎么说、说什么相关"，不改"能不能说"。
6. **人在环**：全局规范的变更必须经人审。
7. **可解释、可回滚**：装配记录版本，规范带 TTL。
8. **用户可见可控**：用户对自身记忆有完整控制权。
9. **标准优先**：核心是厂商中立的标准 MCP 服务。

## 系统架构

```
 第三方助理（LLM 执行推理）
   │  MCP（Streamable HTTP，tools 为核心）
   ▼
 ┌────────────────── 记忆中枢（无在线生成模型）──────────────────┐
 │ 接口层：hub_context / hub_observe / hub_check /              │
 │         hub_feedback / hub_memory_*                          │
 │ 包装配器：记忆包 + 规范包 + 评分量表（分档：紧凑/完整）       │
 │ 记忆库 M  │ 规范库 N（六 skill 的规范条目）│ 量表库 R         │
 │ 检索组件：元数据过滤 + 关键词 + 嵌入向量（仅检索，无生成）     │
 │ 事件日志（脱敏、只追加）                                      │
 │ 离线：巩固/衰减/聚类 → 候选池 → 编审工作台 → 灰度发布 → 回滚 │
 │ 评测台：场景集、回归、A/B、消融                               │
 └──────────────────────────────────────────────────────────────┘
```

**在线流程（单轮）**

1. 助理调用 `hub_context`（**必须在生成之前**）。
2. 中枢按场景装配包并返回（紧凑包默认；冲突或高风险时升级为完整包）。
3. 助理 LLM 在**一次调用内**按包中指令依次完成：推断假设 → 按规范精修选择 → 生成回答 → 按量表自评；自评未过则最多重写 N 次（默认 N=2）。
4. 助理回传 `hub_check`（自评与规则命中摘要）和 `hub_observe`（提取出的假设、预测的用户反应）。
5. 用户行为到来后，由中枢做预测与实际的比对，更新置信度（循环一致性）。

**降级**：中枢不可用或超时，助理按无记忆继续，不得阻塞回答。

## 仓库结构

单一 Cargo Workspace，18 个 Crate（14 个核心库 + 4 个部署可执行文件）：

| Crate | 职责 |
| :--- | :--- |
| [`crates/common`](crates/common) | 共享基础库：`HubError` 错误码映射、`AppConfig` 强校验、`AuthCtx`/`Acl` 鉴权上下文、`init_tracing` 强制脱敏日志 |
| [`crates/hub-core`](crates/hub-core) | 核心领域模型与算法引擎：打分/MMR、置信度衰减、信念升格、边际判定、软超时熔断降级、G1/G2 复验门禁、循环一致性、C6 冲突仲裁 |
| [`crates/hub-store`](crates/hub-store) | 存储引擎：PostgreSQL 15 + pgvector（1024 维）DDL 迁移、`InMemoryMemoryStore` 多租户隔离与注销级联删除 |
| [`crates/hub-embed`](crates/hub-embed) | 向量嵌入：`MockEmbedder` 确定性伪向量与 L2 模长归一化契约 |
| [`crates/hub-packs`](crates/hub-packs) | 规范包管理：`AtomicPackRegistry`（`ArcSwap` 无锁热替换、核心锁防篡改校验）、六大 Skill 清单定义 |
| [`crates/hub-rules`](crates/hub-rules) | 确定性规则引擎：纯函数词表/模式匹配、C1~C6 预编译冲突消解表 |
| [`crates/hub-retrieval`](crates/hub-retrieval) | 混合检索：全文 FTS + 向量双路召回、未成年人模式物理过滤、敏感层鉴权拦截、MMR 重排 |
| [`crates/hub-authz`](crates/hub-authz) | 资源服务器鉴权：RFC 9728 受保护资源元数据、RFC 8707 资源指示符强校验、过期/Scope 拦截 |
| [`crates/as-core`](crates/as-core) | OAuth 2.1 授权服务器核心：PKCE S256 强制校验、Token Family 轮换与重放全族作废、确认令牌即用即焚 |
| [`crates/as-web`](crates/as-web) | 用户中心与同意页（服务端渲染） |
| [`crates/hub-mcp`](crates/hub-mcp) | MCP 协议适配：协议版本协商、`ToolDispatcher` 结构化与 JSON 双模输出 |
| [`crates/ref-client`](crates/ref-client) | 参照客户端：模拟完整对话轮次，每轮生成前强制调用 `hub_context`，会话完成后回流 `hub_observe` |
| [`crates/offline`](crates/offline) | 离线任务：`DailyConsolidationJob` 每日批量衰减与超期清理、`ConflictClusterer` 未覆盖冲突聚类候选生成 |
| [`crates/eval`](crates/eval) | 评测与校准平台：场景自动化基准套件、量表软项优化器（反迎合下限 ≥ 0.15 归一化） |
| [`crates/editor-web`](crates/editor-web) | 规则编审工作台服务：双人审批状态机（防起草人自审）、放宽类强制双签、灰度与一键回滚、只追加审计 |
| [`crates/bins/mcp-server`](crates/bins/mcp-server) | **可运行主服务 1**：标准 MCP 服务（核心） |
| [`crates/bins/auth-server`](crates/bins/auth-server) | **可运行主服务 2**：OAuth 2.1 授权服务器、用户中心、同意页 |
| [`crates/bins/offline-worker`](crates/bins/offline-worker) | **可运行主服务 3**：离线定时批处理守护进程 |
| [`crates/bins/editor-server`](crates/bins/editor-server) | **可运行主服务 4**：规则编审工作台后端 |

## MCP 工具清单

按风险拆粒度，每个工具的注解（只读、破坏性、幂等）准确，客户端可据此决定是否需要用户确认。

| 工具 | 作用 | 关键输入 | 注解 |
| :--- | :--- | :--- | :--- |
| `hub_context` | 装配并返回记忆包/规范包/量表 | `conversation_id`, `turn_id`, `interaction_context` | 只读、幂等 |
| `hub_observe` | 提交助理提取的假设与预测反应 | 假设列表、预测、`turn_id` | 幂等 |
| `hub_check` | 回传自评与规则命中摘要 | 自评结果、命中规则 | 幂等 |
| `hub_feedback` | 用户对某次回答的反馈 | 评分、说明 | 幂等 |
| `hub_memory_pause` | 暂停或恢复记忆使用与采集 | `on/off`, `scope` | 幂等 |

默认只给助理前四个核心工具；用户控制类工具（`hub_memory_*`）需要 `memory:manage` 授权（首次调用走分步 step-up 授权）。

**授权 scope**

| scope | 允许 |
| :--- | :--- |
| `memory:context` | `hub_context`，读取 resources |
| `memory:write` | `hub_observe`、`hub_check`、`hub_feedback` |
| `memory:manage` | `hub_memory_*`（分步授权） |

## 六技能框架

域代理内热插拔六个 skill，热插拔以**声明式规范包（数据）**实现，用原子替换加载，无需动态加载代码：

| 层 | skill | 角色 |
| :--- | :--- | :--- |
| 规范源 | 天理与人欲 | 产出带来源和版本的规范条目 |
| 规范源 | 国民总记忆 | 产出带来源和版本的规范条目（事实层） |
| 统合 | 知守与知行 | 把源规范编译成"守（禁/限）+ 行（宜/表达）"的场景补丁与冲突消解表 |
| 约束 | 天心即民心 | 按中国传统叙事所载伦理道德规范输出（"取其义、不取其行"过滤） |
| 约束 | 我命即天命 | 按用户记忆修正输出（只改"怎么说、哪些相关"，必含反迎合项） |
| 仲裁 | 如梦亦如幻 | 冲突检测与调度，不自带内容规则；裁决可解释，所有规范带版本与 TTL |

**运行时调度**：天心与我命并行修正；兼容则合并；冲突则由如梦亦如幻查知守与知行预先编译的**冲突消解表**（C1~C6）；表中无对应项时，下发保守策略并把案例记入候选池。运行时不做动态生成。

## 记忆模型与评分量表

- **记忆条目**：`kind`（preference/relation/behavior/state/goal_belief）、`provenance`（stated/observed/inferred）、`confidence`、`use`（free/never_mention）、`ttl`、`pack_version`。
- **升格门槛**：`HUB_PROMOTE_MIN_OBS`（≥3 次观察）、`MIN_CONTEXTS`（≥2）、`MIN_SPAN_DAYS`（≥7）、`MIN_CONF`（≥0.7），边际差 `MARGIN_DELTA`（0.15）。
- **衰减周期**：state 7 天、preference 90 天、relation 365 天、behavior 90 天、goal_belief 180 天。
- **响应代理自检量表**：
  - **硬门**：G1 合规底线（法律与安全）、G2 不外显敏感或 `never_mention` 记忆、G3 涉及国民总记忆事实层时不编造——硬门不通过直接重写，不参与加权。
  - **软项**：共情对齐、连贯、关系措辞、记忆使用恰当性、反迎合。
  - 综合得分 U = Σ w·s；通过阈值 τ=0.80，重写上限 N=2。

## 快速开始

环境要求：Rust 1.80+、Docker 与 Docker Compose、PostgreSQL 15+（含 pgvector 扩展）。

### 本地开发

```bash
# 编译（所有二进制）
cargo build --release

# 运行测试与静态检查
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
```

### 一键部署（Docker Compose）

```bash
cp .env.example .env    # 按需修改
docker compose up -d
```

启动五个服务：

| 服务 | 端口 | 职责 |
| :--- | :--- | :--- |
| `postgres` | 5432 | PostgreSQL 15 + pgvector 持久化存储 |
| `auth-server` | 8080 | OAuth 2.1 授权服务器 + 用户中心 |
| `mcp-server` | 3000 | 记忆中枢主 MCP Server（Streamable HTTP / stdio） |
| `offline-worker` | — | 离线批处理守护进程（每日衰减清理 & 聚类分析） |
| `editor-server` | 8081 | 规则编审工作台（双人审批、灰度与一键回滚） |

PostgreSQL 会在容器首次初始化时自动执行 `crates/hub-store/migrations/20261009000001_initial_schema.sql`。

### 配置

完整配置项见 [`.env.example`](.env.example) 与设计文档第 17.5 节，关键配置：

```bash
HUB_DB_URL=postgresql://hub_app:secret@localhost:5432/memory_hub?sslmode=disable
HUB_TOKEN_BUDGET_DEFAULT=600      # 检索与装配令牌预算
HUB_SOFT_TIMEOUT_MS=300           # 软超时，超时返回降级紧凑包
HUB_RETRIEVAL_FTS_K=20            # 全文召回数
HUB_RETRIEVAL_VEC_K=20            # 向量召回数
HUB_SCORE_WEIGHTS=0.40,0.20,0.15,0.10,0.15
HUB_MMR_LAMBDA=0.7
FEATURE_NATIONAL_MEMORY=false     # 国民总记忆开关（默认关闭）
FEATURE_LEDGER_AVAILABLE=true     # 人情账开关（默认关闭，用户主动开启）
```

## 安全与合规

- **数据红线**：绝不持久化对话原文；草稿对象在硬门校验通过后即刻脱离作用域释放；敏感信息（身份证、银行卡）强制阻断并返回 `E_POLICY_BLOCKED`。
- **授权**：服务端是 OAuth 2.1 资源服务器，实现 RFC 9728 受保护资源元数据与 RFC 8707 资源指示符绑定受众；公共客户端用 PKCE；不接受发给其他服务的令牌，不做令牌透传。
- **限流与边界**：校验 Origin；按客户端和用户限流；请求大小与超时上限；全链路追踪。
- **日志**：`tracing` 输出强制经过脱敏格式化，仅留 trace ID、方法名、耗时与脱敏摘要。
- **责任边界**：中枢不保证助理最终输出的合规性，只提供规范与量表。

## 测试与质量门禁

- `cargo test --workspace`：单元与集成测试全部通过（覆盖领域模型、检索重排、协议序列化、安全防御及端到端流程）。
- `cargo fmt --all -- --check`：100% 对齐。
- `cargo clippy --all-targets -- -D warnings`：零编译警告。
- 所有 crate 默认 `#![forbid(unsafe_code)]`。
- 依赖准入：`cargo-deny` 与 `cargo-audit` 进 CI；依赖锁定。

## 文档

- [系统设计与技能框架（开发交付版 v1.0）](记忆中枢系统设计与技能框架（开发交付版%20v1.0）.md) — 权威设计文档
- [工程实施全量验收交付报告](Docs/实施交付报告.md)
- [实施计划](Docs/实施计划.md) · [工程落地版](Docs/实施计划（工程落地版%20v1.0）.md)
- [疑问与假设追踪](Docs/questions.md)

## 路线图

- **P1 最小闭环**：MCP 服务核心 + 参照客户端端到端验证（已完成）。
- **P2 完整框架**：Skill 框架、关系流水线、循环一致性、离线任务、评测校准（已完成）。
- **P3 治理与规模化**：规则编审工作台、融合与可运行二进制、部署（已完成）。

## License

`MIT OR Apache-2.0`（见 [Cargo.toml](Cargo.toml)）。
