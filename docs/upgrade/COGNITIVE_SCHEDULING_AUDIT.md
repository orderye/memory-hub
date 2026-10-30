# 认知任务生产调度核实报告

**阶段：** P1-A（Task 3）
**核实时间：** 2026-10-29
**基线 commit：** `d35f033`
**结论：** 认知作业**未接入生产调度**；抽取器**生产默认不装配**（`HUB_COGNITIVE_EXTRACTOR=disabled`），
仅开发时可用 Mock 占位实现。二者均不得视为已启用能力。

---

## 1. 任务类型与代码位置

| 作业 | 位置 | 形态 |
|---|---|---|
| `ChatCognitiveJob` | `crates/worker/src/cognitive_job.rs` | 可直接调用的执行器（`run_session` / `run_session_with_result`） |
| `ExperienceDistillJob` | `crates/worker/src/experience_job.rs` | 可直接调用的执行器（`run_distill` / `run_distill_with_result`） |

两者都只导出为库 API（`crates/worker/src/lib.rs` 的 `pub use`），没有任何二进制引用。

## 2. 是否被生产二进制调度：**否**

`EmbeddedJobs`（`crates/worker/src/embedded.rs`）是唯一的生产周期调度装配点，其字段只有：

- `backup_job`（定时备份）
- `integration_job`（整合扫描 + 过期归档）
- `evidence_job`（证据保留清理）

`run_periodic_loop` 的 30 秒 tick 中只执行上述三类作业。两个认知作业**不在其中**。

`crates/bins/backup-worker/src/main.rs` 与 `crates/bins/mcp-server/src/main.rs` 均未构造认知作业实例。

**已固化为契约测试**：`embedded::tests::embedded_jobs_do_not_schedule_cognitive_jobs`
扫描生产代码段（`#[cfg(test)]` 之前），断言不含 `ChatCognitiveJob::new` / `ExperienceDistillJob::new`
等注册痕迹。任何未来接入必须先通过该测试的修改评审。

## 3. 生命周期现状（对照任务书 C1）

| 维度 | 现状 |
|---|---|
| 创建条件 | 无。没有"待处理任务表"，也没有由会话或任务完成事件触发的入队逻辑 |
| 调度入口 | 无生产入口；仅测试与库调用 |
| 执行周期 | 无 |
| 任务领取机制 | 无（没有 claim / SKIP LOCKED / 状态机） |
| 租约与重领 | 无。`JobLease` 仅被备份/整合/证据三类周期作业使用 |
| 状态持久化 | 无。`ChatCognitiveJobReport` / `ExperienceDistillReport` 只作为返回值，不落库 |
| 失败可观测性 | 仅进程日志（`warn!`），不可查询、不可重放 |
| 重试 | 有：`crate::retry::with_retry`，仅对 `is_retryable()` 为真的错误重试，次数有上限并带退避 |
| 幂等 | 服务层保证：同一会话重复输入不会重复入库（`test_duplicate_input_idempotency`）；经验提炼同一来源任务重复执行不产生重复经验 |
| 事务边界 | 每次服务调用内部完成；无跨作业事务 |

## 4. 抽取器真实性：**Mock，且生产默认不装配**

原状：`crates/bins/mcp-server/src/main.rs` 无条件装配

```rust
let chat_extractor = Arc::new(mem_core::testing::MockChatCognitiveExtractor::new());
warn!("Chat 认知抽取器当前为 MockChatCognitiveExtractor（占位实现）...");
```

**本轮修正（2026-10-29 后续）**：装配改为受 `HUB_COGNITIVE_EXTRACTOR` 控制，**默认 `disabled`**：

| 配置 | 行为 | 副作用 |
|---|---|---|
| `disabled`（默认） | 不构造 `ChatCognitiveService`；`cognitive_chat_propose` 返回 `E_METHOD_NOT_FOUND`（说明未配置抽取器）；`cognitive_intentions_list` 走只读存储分支 | **不写证据、不产出提案** |
| `mock`（仅限开发） | 装配 `MockChatCognitiveExtractor` | 会写证据与认知提案（Manual 模式下只入待审队列） |

- `MockChatCognitiveExtractor` **不会理解对话内容**，候选完全由调用方注入（`with_candidates`）。
- 能力标记 `provides_semantic_extraction() -> false` 与契约测试
  `mock_chat_extractor_declares_no_semantic_extraction` 继续保留，供装配与健康检查区分占位与真实 LLM。
- 契约测试 `test_cognitive_chat_propose_is_disabled_without_extractor` 锁定默认行为：
  工具返回 `E_METHOD_NOT_FOUND` 且不产生任何正式记忆。

**结论：Chat 认知流的产出不得被描述为"LLM 提炼结果"；默认部署下该入口不可用。**

> 为什么不是"只靠 mock 返回空列表"：那是实现细节，改一行 mock 就会让占位产物进入正式数据。
> 关闭装配是部署契约层面的保证。

## 5. Lite / Production 差异

| 形态 | 后台作业 | 认知作业 |
|---|---|---|
| Production（独立 `backup-worker` 常驻） | 备份 / 整合 / 证据三类周期运行 | 不运行 |
| Production（`mcp-server` + `HUB_EMBEDDED_WORKER=1`） | 同一套三类作业内嵌运行，走 `JobLease` 跨副本互斥 | 不运行 |
| Lite（`HUB_DEPLOYMENT_PROFILE=lite`） | `backup-worker` **禁止常驻**，只允许 `--once`；compose 默认由 `mcp-server` 内嵌调度 | 不运行 |

三种形态下认知作业行为一致：**均未调度**。

## 6. 默认行为是否保守：**是**

- 规则整合仍是默认模式（`ConsolidationMode::Rules`）；后台扫描在纯规则模式下**零嵌入调用**（有测试锁定）。
- 向量能力保持可选（`HUB_EMBED_PROVIDER` 显式开启）。
- 未接入真实 LLM 抽取器，Mock 产物**不会**被当作正式认知结果：`CognitivePolicy` 默认为 `Manual`，
  候选进入待审提案队列而非直接入库。
- 未为满足"后台任务已运行"而自动写入低质量记忆。

## 7. 日志与敏感信息

核查 `cognitive_job.rs` / `experience_job.rs` 的失败日志：只输出 `worker`、`session_id`/`task_id`、
`attempts` 与 `error` 文本，**不输出消息正文、token 或凭据**。

核查 `validate_cognitive_candidate` 的全部错误消息，均为固定文案（"内容不能为空"、"超过 8192 字符上限"、
"置信度必须在 0.0 ~ 1.0 之间"、"标签数不能超过 32"、"包含敏感凭证"、"含糊猜测不能作为确定意图"），
**不回显候选正文**，因此 `rejection_reason` 落库不会泄露内容。

## 8. 未完成项与阻塞

- **无生产调度**：这是当前设计状态，不是缺陷。若未来要启用，必须先补齐：任务表与领取（含 `FOR UPDATE SKIP LOCKED`）、
  持久租约与过期重领、可查询状态、失败原因留存、多实例互斥验证。本轮**不做**该实现，因为任务书要求
  "不得仅为满足后台任务已运行而自动写入低质量记忆"，且真实抽取器尚未接入。
- **真实 LLM 抽取器未接入**：属于能力缺口，明确标记为未验证。
- **Lite/Production 运行态验收**（任务书 C4）需实际启动二进制并观察作业日志；本轮未执行真实部署，
  仅通过静态装配与契约测试确认调度集合。

## 9. 本轮代码变更

| 文件 | 变更 |
|---|---|
| `crates/worker/src/embedded.rs` | 新增契约测试 `embedded_jobs_do_not_schedule_cognitive_jobs` |
| `crates/mem-core/src/testing.rs` | `MockChatCognitiveExtractor::provides_semantic_extraction() -> false`；新增契约测试；补充"占位实现"文档注释 |
| `crates/common/src/cognitive.rs` | 新增 `CognitiveExtractorMode`（`disabled` 默认 / `mock`），未知值启动失败 |
| `crates/common/src/config.rs` | 新增 `HUB_COGNITIVE_EXTRACTOR` 读取与校验 |
| `crates/bins/mcp-server/src/main.rs` | 默认不构造 `ChatCognitiveService`；`mock` 时才装配并要求显式配置 |
| `crates/mcp/src/tools/tests.rs` | `test_cognitive_chat_propose_is_disabled_without_extractor` |

<!-- kimi-badge -->
[![powered by KimiCode](../../.github/assets/kimi-badge.svg)](https://www.kimi.com/code)
