# 阻塞项解决方案与实证结果

**更新时间：** 2026-10-29
**分支：** `reliability/productionization`（提交 `044eb73`）
**CI run：** `40828521543` — 全部 4 job 通过，**PG 集成测试 12 passed / 0 failed**

---

## 1. 解决方案总览

原阻塞 6 项。逐项给出解法并已实施，**仅"真实 embedding"一项无法在本地或 CI 解决**。

| 原阻塞 | 解法 | 状态 |
|---|---|---|
| v1–v6 全版本恢复矩阵 | 在 PG 集成测试里按各版本校验和算法手工构造 6 份历史 payload，逐个跑 DryRun + Merge，并断言 v7 被拒 | ✅ **已实证** |
| 迁移 018 历史重复清理 | 测试里先 drop 唯一索引 → 插入 3 条同键 open 提案 → 重放迁移的 `row_number` 清理 → 重建索引 → 断言只剩最早一条且二次插入被拒 | ✅ **已实证** |
| `EXPLAIN ANALYZE` / 索引命中 | 批量插入 20000 行随机 1024 维向量 + `ANALYZE` → `EXPLAIN` 断言计划含 `idx_memories_embedding_hf` | ✅ **已实证**（索引命中） |
| 大表 `CONCURRENTLY` 实测 | 需生产规模数据与维护窗口，不适合 CI | ⚠️ **方案已给**，待演练 |
| 真实 embedding 指标与阈值重标定 | 需真实模型端点 | ❌ **仍阻塞** |
| Lite/Production 运行态观察 | 需起二进制；PG 条件已具备 | ⚠️ **可做**，未做 |

## 2. 关键洞察：CI 的 PG 服务就是现成的实证环境

`.github/workflows/ci.yml` 的 `postgres` job 自带 `pgvector/pgvector:pg15`，并且

```bash
for f in crates/store/migrations/*.sql; do psql ... -v ON_ERROR_STOP=1 -f "$f"; done
cargo test -p store --features pg-integration -- --ignored --test-threads=1
```

**glob 会应用全部迁移**，因此新增迁移与新增 PG 测试自动纳入实证范围。这是"不新建云环境
也能验证"的关键——用 `workflow_dispatch` 在分支上触发即可，不改 `main`、不部署。

## 3. 三次失败与三次修复（都是真问题，都在测试侧）

| 轮次 | 失败 | 根因 | 修复 |
|---|---|---|---|
| 1 | `pg_atomic_namespace_replace_rolls_back_on_failure` | 整结构比较；PG `timestamptz` 微秒精度 vs `Utc::now()` 纳秒精度 | 改为断言不变量字段 |
| 2 | `pg_restore_matrix_v1_through_v6` 报"对象不存在" | 未给 agent 授权命名空间 | 补 `grant` |
| 2 | `pg_ann_query_actually_uses_partial_index` 走 Seq Scan | 500 行小表规划器合理选择 Seq Scan，**不足以证明索引可用** | 提到 20000 行批量插入 + `ANALYZE` |
| 3 | 同上 matrix 仍报"对象不存在" | ACL 按**账户作用域**判定，而测试用裸名建库/授权；且 payload 里嵌的是作用域键 | 命名空间与授权改用 `scope_name(...)`；payload 换成**用户可见名** |

第 3 轮的第 2 点揭示了一条真实契约：**备份负载存用户可见名，不存内部 `{account}/{ns}` 键**。
这与 Task 1 修复的 `all_relations_in_namespace` 泄漏是同一类问题，现已有 PG 侧证据。

> 过程失误记录：有一次提交带着编译错误就推送了（`a61986c`），随后 `044eb73` 修正。
> 教训是推送前必须先跑 `cargo test --no-run`，不能只看 clippy。

## 4. 仍未解决的两项及方案

### 4.1 真实 embedding（唯一真正的硬阻塞）

需要 OpenAI 兼容 `/embeddings` 端点。代码已就绪：

```bash
HUB_EVAL_EMBED_ENDPOINT=... HUB_EVAL_EMBED_MODEL=bge-m3 HUB_EVAL_EMBED_DIM=1024 \
HUB_EVAL_EMBED_API_KEY=... HUB_EVAL_REQUIRE_SEMANTIC=1 HUB_EVAL_WRITE_BASELINE=1 \
  cargo test -p retrieval --test eval_bench -- --nocapture
```

必须先冻结**真实模型基线**（现有基线是伪向量模态，指纹/嵌入器不同会被硬失败）。
随后 `HUB_EVAL_CALIBRATE=1` 重标定阈值（现有 0.65 是伪向量下标定的）。

**本机无法替代**：无 `ollama`/`llama-server`，Python 无 `torch`/`sentence_transformers`。

### 4.2 大表 `CONCURRENTLY`

需生产规模数据。方案已在 `docs/pg-maintenance-runbook.md` §2：先建后删、`maintenance_work_mem=512MB`、
`max_parallel_maintenance_workers=2`。**演练步骤**：

```sql
-- 维护窗口，先在预发/副本上测耗时
create index concurrently idx_new on memories using hnsw ((embedding::halfvec(1024)) halfvec_cosine_ops)
  where status in ('active','archived');
-- 验证有效后再删旧索引
select indisvalid from pg_index where indexrelid = 'idx_new'::regclass;
drop index concurrently idx_memories_embedding_hf;
alter index idx_new rename to idx_memories_embedding_hf;
```

## 5. 当前可宣称的结论

| 能力 | 证据 |
|---|---|
| P0-A 备份完整性、v6 格式、v1–v6 兼容、原子回滚、审计 | PG 实测 ✅ |
| P0-B 跨进程 open 提案唯一性、迁移 018 清理 | PG 实测 ✅ |
| 迁移 015 部分索引被真实查询命中 | PG `EXPLAIN` 实测 ✅ |
| 迁移 015 的 halfvec ANN 召回 | PG 实测 ✅ |
| CI 全绿、供应链审计、Release 构建 | CI run 40828521543 ✅ |
| 真实 embedding 语义质量 | ❌ 未验证 |
| 大表 `CONCURRENTLY` 耗时 | ⚠️ 未演练 |
| Lite/Production 运行态 | ⚠️ 未观察 |

<!-- kimi-badge -->
[![powered by KimiCode](../../.github/assets/kimi-badge.svg)](https://www.kimi.com/code)
