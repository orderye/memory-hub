# 真实 Embedding 检索评测边界报告

**阶段：** P1-C（Task 5）
**评估时间：** 2026-10-29
**基线 commit：** `d35f033`
**结论：** 三类能力边界已在代码与门禁中分离并可回归；**真实模型语义结论仍未验证**（无可用 endpoint/凭据）。

---

## 1. 三类能力的边界

| 类别 | 实现 | 能证明什么 | 不能证明什么 |
|---|---|---|---|
| 规则 / 词法 | `ConsolidationMode::Rules`、FTS + 关键词打分 | 管道连通、排序逻辑、业务约束（时间/场景/标签/层级/环境） | 语义召回能力 |
| 伪向量 | `TestEmbedder` / `MockEmbedder`（`provides_semantic_signal() == false`） | 向量路径接线、MMR、融合打分、降级 | 语义相似度质量 |
| 真实模型 | `HttpEmbedder`（OpenAI 兼容 `/embeddings`） | 语义召回、同义改写、跨表述匹配 | — |

打分侧已按能力区分：无语义信号的嵌入器，向量维度按"不可用"处理，剩余维度重新归一化，
**配置权重不变**——换成真实模型后该维度自动恢复参与。

## 2. 本轮发现并修复的评测治理缺陷

### 2.1 基线不可比时静默跳过门禁（已修复）

旧行为：数据集指纹、评测配置或嵌入器与基线不一致时，只 `println` 一行提示然后 `return`。
测试照样绿——"数据集被改动"与"质量达标"在 CI 里表现完全相同。

**修复**：抽出 `assert_baseline_comparable()`，三项任一不一致即 `panic`，要求显式重冻基线。
回归测试 `incomparable_baseline_must_fail_loudly` 覆盖指纹 / 配置 / 嵌入器三种不匹配，
并验证三者一致时正常通过。

### 2.2 "配置了端点"不等于"端点可用"（已澄清并锁定）

`HttpEmbedder::new()` 只构建 HTTP 客户端、不发起连接。因此"配置了 `HUB_EVAL_EMBED_ENDPOINT`"
不代表真实模型可用；真正的失败发生在首次 `embed()` 调用。

回归测试 `unreachable_configured_endpoint_fails_when_semantics_required`：
在**子进程**中以不可达端点 + `HUB_EVAL_REQUIRE_SEMANTIC=1` 运行，断言嵌入器仍声明
`provides_semantic_signal() == true`（不回落伪向量）且 `embed()` 显式失败。

> 测试实现说明：初版在同进程内 `std::env::set_var` 设置端点，污染了并行运行的
> `test_retrieval_bench_gate` 等用例（实测 3 个用例连到不可达端点失败）。已改为
> 子进程隔离，环境变量不再跨用例泄漏。这是测试隔离缺陷，不是产品缺陷。

## 3. 既有语义门禁（已核实生效）

- `HUB_EVAL_REQUIRE_SEMANTIC=1` + 伪向量 → 门禁 `panic`，禁止把伪向量指标当语义结论。
- 报告落盘 `semantic_ready` 字段，基线文件同名字段参与比对。
- `HUB_EMBED_PROVIDER=off`：不生成、不落库向量，检索纯词法，Lite 默认可用。
- provider 不可用：`HttpEmbedder` 请求失败上抛，检索层按 `HUB_CONSOLIDATION_FALLBACK`
  或软超时降级，不会挂起。

## 4. 指标覆盖

`eval_bench` 已覆盖任务书 E3 要求的指标：

- Recall@1 / 5 / 10、MRR@10、nDCG@10
- 硬负例违规率、首位误取率、负控制（域外）命中率
- 按类别分层：`exact` / `paraphrase` / `temporal` / `evolution` / `scene` / `tags` /
  `layer` / `env` / `no_embedding` / `negative`
- 场景分层：Chat / Working / Coding
- 进程内 p50 / p95
- 消融（`HUB_EVAL_ABLATIONS=1`）与阈值标定（`HUB_EVAL_CALIBRATE=1`）

数据集指纹 `9f5b3b075a558371`（480 文档 / 600 查询），指纹变化现在会**硬失败**。

## 5. 整合安全性（E4）

已由既有测试与策略保证，本轮未改动：

- 只有 exact / canonical duplicate 具备自动合并资格；
- semantic candidate **即使相似度极高也不自动合并**（`test_semantic_candidate_never_auto_merges_even_at_max_similarity`）；
- 规则模式零嵌入调用（`test_rules_mode_makes_zero_embedding_calls`）；
- 不确定候选保留为提案，不落库为记忆。

## 6. 未验证项（阻塞）

以下必须真实 embedding 服务才能产出结论，本轮**未执行**：

1. 真实模型下的 Recall@1/5/10、MRR@10、nDCG@10。
2. 同义改写、时间表达、否定句、实体混淆的分层表现。
3. 真实模型下重新标定 `HUB_RETRIEVAL_MIN_SCORE`（现有 0.65 是在伪向量模态下标定的）。
4. 查询延迟 p50/p95、计算成本、内存占用。
5. 真实模型下的错误合并率（E4 要求统计，而非只看召回）。

**在补齐上述实证前，任何"语义检索已达标"的结论都不成立。** 现有默认评测结果
（伪向量模态）只代表管道与排序逻辑，不代表语义能力。

## 7. 本轮代码变更

| 文件 | 变更 |
|---|---|
| `crates/retrieval/tests/eval_bench.rs` | 抽出 `assert_baseline_comparable` 并把不可比改为硬失败；新增两个回归测试；`BenchReport` 派生 `Default` 供测试构造 |

<!-- kimi-badge -->
[![powered by KimiCode](../../.github/assets/kimi-badge.svg)](https://www.kimi.com/code)
