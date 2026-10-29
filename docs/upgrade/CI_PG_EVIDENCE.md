# CI 与 PG 实证结果

**执行时间：** 2026-10-29
**分支：** `reliability/productionization`
**提交：** `7e1fbe5`
**CI run：** `40825986648`（`workflow_dispatch`）

---

## 1. 结论

**全部 4 个 CI job 通过**，其中 PostgreSQL 集成测试 **9 passed / 0 failed**，覆盖了此前所有
"仅编译通过"的 P0 断言。

| Job | 结果 | 耗时 |
|---|---|---|
| 质量门禁（fmt / clippy / test） | ✅ success | 2m27s |
| **PostgreSQL 集成测试** | ✅ success | 1m07s |
| 依赖准入与安全审计（cargo-deny / audit） | ✅ success | 34s |
| Release 构建（三二进制 + jemalloc） | ✅ success | — |

## 2. PG 集成测试逐项结果（9 passed / 0 failed）

环境：GitHub Actions service `pgvector/pgvector:pg15`；迁移按文件名 glob 逐个 `psql -f` 应用
（含本轮新增的 `20261028000018_proposal_open_uniqueness.sql`）。

| 用例 | 覆盖的验收项 | 结果 |
|---|---|---|
| `pg_atomic_namespace_replace_rolls_back_on_failure` | **P0-A 原子多 namespace 回滚** | ✅ |
| `pg_open_proposal_business_key_is_unique` | **P0-B 迁移 018 唯一索引 + 关闭后可再建** | ✅ |
| `pg_backup_and_restore_roundtrip` | P0-A 备份/恢复往返 | ✅ |
| `pg_vector_ann_halfvec_and_embeddings_by_ids` | P1-B halfvec 部分索引 + 瘦列契约 | ✅ |
| `pg_fts_backfill_retokenizes_legacy_rows` | 迁移 013 回填 | ✅ |
| `pg_memory_crud_and_namespace_isolation` | 命名空间隔离 | ✅ |
| `pg_grants_and_agent_registry` | 授权与 Agent 注册 | ✅ |
| `idempotency::pg_register_is_idempotent_and_prunes` | 幂等键 | ✅ |
| `idempotency::pg_response_hash_is_text_not_bytea` | 幂等响应哈希类型 | ✅ |

## 3. 首轮失败与修复（真实证据）

首轮 CI（run `40825357517`）PG job 失败，**8 passed / 1 failed**：

```
pg_atomic_namespace_replace_rolls_back_on_failure
assertion `left == right` failed
  left:  ... created_at: 2026-10-29T05:47:29.476674Z,   updated_at: ...476674Z
  right: ... created_at: 2026-10-29T05:47:29.476674662Z, updated_at: ...476674762Z
```

**根因：不是产品缺陷，是断言过严。** 回滚本身是正确的（原行仍在、替换行不存在），
但测试比较整个 `MemoryEntry`：PostgreSQL `timestamptz` 是**微秒**精度，而 `Utc::now()`
有**纳秒**精度，往返后 `created_at` / `updated_at` 必然不等。

**修复**（commit `7e1fbe5`）：改为断言定义不变量的字段——
`id`、`namespace`、`content`、`version`、`status`、`tags`、`metadata`、`source_agent`。

这正体现了"先证明现状"的价值：如果不跑 CI，这个测试会以"编译通过"的状态被误当作已验证。

## 4. 由此解除的阻塞

| 原阻塞 | 状态 |
|---|---|
| B1 原子多 namespace 替换的真实回滚 | **已解除**（PG 实测通过） |
| B2 PG 备份/恢复往返 | **已解除** |
| B4 迁移 018 唯一索引 + 关闭后可再建 | **已解除**（真实并发语义由唯一索引保证） |
| B6 迁移 015 索引可用性（部分） | **部分解除**（`pg_vector_ann_halfvec` 验证索引可命中；`EXPLAIN` 明细仍缺） |
| B11 正式 CI 结果 | **已解除**（run 40825986648 全绿） |
| B12 供应链审计 | **已解除**（CI supply-chain job 通过） |

## 5. 仍阻塞的项

| 阻塞 | 原因 | 解除条件 |
|---|---|---|
| B3 v1–v6 全版本恢复矩阵 | CI 只跑了 v1 往返样本 | 补一个遍历 v1–v6 的 PG 用例 |
| B5 迁移 018 历史重复清理 | CI 库无历史重复数据 | 生产执行前 dump + 重复行核对 |
| B6 完整部分（`EXPLAIN ANALYZE`、`indisvalid`、体积、中断重试） | 需交互式分析 | 手工在 PG 上执行 runbook §3 查询 |
| B7 大表 `CONCURRENTLY` 实测 | 需生产规模数据 | 维护窗口演练 |
| B8/B9 真实 embedding 指标与阈值重标定 | 无 endpoint/凭据 | 提供 OpenAI 兼容端点 |
| B10 Lite/Production 运行态观察 | 未部署 | 起二进制（现已有 PG 条件） |

## 6. 发布溯源

- 推送分支：`origin reliability/productionization`（未推 CNB，未改 `main`，未部署）。
- 权威源判定依据：`scripts/push-cnb.sh` 按 patch-id 从 GitHub 重放到 CNB，
  且 CNB 只对 `main`/tag 部署 → **GitHub 为权威写入源，CNB 为发布镜像**。
- 镜像偏差检测：当前无自动化；建议在 CNB 侧加"main HEAD 必须能在 GitHub 找到同名 patch-id"的检查。

<!-- kimi-badge -->
[![powered by KimiCode](../../.github/assets/kimi-badge.svg)](https://www.kimi.com/code)
