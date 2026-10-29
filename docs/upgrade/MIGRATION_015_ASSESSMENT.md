# Migration 015 与向量索引升级评估

**阶段：** P1-B（Task 4）
**评估时间：** 2026-10-29
**基线 commit：** `d35f033`
**结论：** 发现并修复一处**索引失效缺陷**；锁与失败恢复风险已静态确认，但**运行期实证仍阻塞**（无隔离 PostgreSQL）。

---

## 1. 迁移实际内容（`20261027000015_vector_index_halfvec_partial.sql`）

```sql
set local maintenance_work_mem = '256MB';

drop index if exists idx_memories_tsv;

create index if not exists idx_memories_embedding_hf
  on memories using hnsw ((embedding::halfvec(1024)) halfvec_cosine_ops)
  where status in ('active', 'archived');

drop index if exists idx_memories_embedding;

alter table memories set (
  autovacuum_vacuum_scale_factor = 0.05,
  autovacuum_analyze_scale_factor = 0.02
);
```

| 审查项 | 结论 |
|---|---|
| 数据类型 / 维度 | `embedding::halfvec(1024)`，与 `HUB_EMBED_DIM` 绑定 |
| operator class | `halfvec_cosine_ops` |
| 谓词 | `status in ('active','archived')`，`merged` 不进索引 |
| 旧索引 | 删除 `idx_memories_tsv`（死索引）与 `idx_memories_embedding`（全量 fp32） |
| 是否在事务中 | **是**。运行时执行器每个迁移一个事务，见 §3 |
| 顺序风险 | **先建新索引、再删旧索引**：正确顺序，新索引建好后才移除旧的 |

## 2. 发现的真实缺陷：查询谓词无法被部分索引证明

索引谓词是字面量 `status in ('active','archived')`，而修复前 `similar()` 生成的是
**参数化** `status = any($1)`。

PostgreSQL 只有在能证明"查询谓词蕴含索引谓词"时才会选用部分索引。参数化数组的值在
计划阶段未知，规划器无法证明，**结果就是索引存在却从不被使用**。

这个缺陷在文档里被掩盖了：`docs/pg-maintenance-runbook.md` 的 EXPLAIN 示例写的是
字面量 `status = 'active'`，会命中索引；而真实代码路径不会。

**修复**（`crates/store/src/postgres/`）：

- 新增 `ann_status_predicate()`：把状态内联为字面量 `status in ('active')` /
  `status in ('active', 'archived')`，与索引谓词逐字一致。状态值来自固定的
  `MemoryStatus` 枚举，不是用户输入，无注入风险。
- 新增 `ann_query_sql()`：恒以单个 `where` 开头，调用方只追加 `and`。
- `similar()` 改用上述构造；状态之外的过滤条件抽到
  `push_memory_filters_rest()`，与普通查询共用，避免两条路径语义漂移。
- `merged` 状态不在部分索引内：`ann_status_predicate()` 返回 `None`，查询退化为
  非索引形态，而不是伪装成可索引。

**回归测试**（`crates/store/src/postgres/tests.rs`）：

- `test_ann_query_status_predicate_is_index_provable`：禁止参数化状态数组；断言单状态
  与双状态分别内联为与索引逐字一致的字面量；断言 `merged` 不伪装可索引。
- `test_ann_query_fragment_is_single_where_clause`：片段恒为单个 `where`，避免拼接
  产出非法 SQL。

## 3. 执行器与锁语义

`PostgresHubStore::run_migrations`：

- 每个迁移**独立事务**：`set local statement_timeout = 0` → `pg_advisory_xact_lock`
  → 账本检查 → 逐条执行 SQL → 写账本 → commit。
- 因此迁移 015 的 `CREATE INDEX` 是**事务内**的普通建索引：全程持锁，
  期间阻塞写入。`SET LOCAL maintenance_work_mem` 能生效正因为它在事务内。
- **不能**在本执行器里放 `CREATE INDEX CONCURRENTLY`：该语句禁止在事务块内运行。
- 失败语义：当前迁移事务整体回滚，`_hub_migrations` 不记录该版本，下次启动重试；
  已提交的先前迁移保留。

CI 与部署走另一条路径（`psql -f` 逐文件、`ON_ERROR_STOP=1`），每个文件一个隐式事务，
与运行时语义接近但不完全等同。

## 4. 生产大表策略

迁移注释已明确"假设空表或小表（2026-10-27 生产 `memories` 为 0 行）"。
大表必须走维护窗口手动 `CONCURRENTLY`，`docs/pg-maintenance-runbook.md` §2 已给出
可执行步骤（提升 `maintenance_work_mem` / 并行 worker、`drop index concurrently` +
`create index concurrently`、换模型时同步 typmod）。

**风险提示（本轮已写入 runbook）**：迁移 015 的 `drop index if exists idx_memories_embedding`
在事务内执行，若新索引构建失败，整个事务回滚、旧索引仍在——这是安全的。但若在维护窗口
**手动**先删旧索引再建新索引，中间窗口内向量检索会退化为顺序扫描，必须按 runbook 的
"先建后删"顺序操作。

## 5. 兼容与回滚

| 场景 | 行为 |
|---|---|
| 旧版本应用连升级后数据库 | 仍可工作：`status = any(...)` 只是不走索引，结果正确，仅性能下降 |
| 回滚到旧应用 | 无需回滚 schema；索引对新旧应用都是纯优化 |
| 回滚迁移 015 本身 | `drop index if exists uq_...` 不适用；需 `drop index concurrently idx_memories_embedding_hf` 并重建 `idx_memories_embedding`，属破坏性操作，需维护窗口 |
| Lite 与 Production | 共用同一 schema 与迁移清单，无差异 |

## 6. 未完成的实证（阻塞项）

以下必须真实 PostgreSQL + pgvector 才能验证，本轮**未执行**：

1. `EXPLAIN (ANALYZE, BUFFERS)` 确认修复后查询命中 `idx_memories_embedding_hf`。
2. 空表 / 小表 / 万级数据下的计划差异（小表 Seq Scan 正常）。
3. 索引 `indisvalid` 状态与 `pg_stat_user_indexes` 体积对比（halfvec ≈ fp32 一半）。
4. 迁移中断后重试、重复执行幂等。
5. 升级后旧应用查询行为与性能。
6. 大表 `CONCURRENTLY` 实测耗时。

在补齐上述实证前，**迁移 015 的部署安全性不予宣称通过**。

<!-- kimi-badge -->
[![powered by KimiCode](../../.github/assets/kimi-badge.svg)](https://www.kimi.com/code)
