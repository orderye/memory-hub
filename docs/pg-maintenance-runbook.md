# PostgreSQL / pgvector 维护 runbook

适用部署：2C/2GB 小内存 VPS（本仓生产形态），`pgvector/pgvector:pg15`（生产实测 pgvector 0.8.7）。
迁移 015（2026-10-27）起向量索引为 **halfvec 半精度 + 部分索引**：

```sql
idx_memories_embedding_hf
  using hnsw ((embedding::halfvec(1024)) halfvec_cosine_ops)
  where status in ('active', 'archived')
```

查询侧（`store` 层 `similar()`）的 dist 表达式 `embedding::halfvec(1024) <=> $q`
必须与索引表达式逐字一致，改任何一侧都要同步另一侧，否则规划器不认表达式索引。

## 1. 会话参数（应用侧已内置，无需配置）

`PostgresHubStore::connect` 的 `after_connect` 对 pgvector ≥ 0.8 的连接设置：

- `hnsw.iterative_scan = 'relaxed_order'` —— 带过滤的 ANN 继续扫描直到凑满 k；
- `hnsw.ef_search = 100` —— 图探索宽度，默认 40 对 vec_k=100 过窄。

召回不足时的调参顺序：先调 `ef_search`（100→200），再考虑 `vec_k`；
两者都以 eval 套件（`cargo test -p retrieval --test eval_bench -- --ignored`，
`HUB_EVAL_PG=1` 指向独立测试库）的 recall@k 为准。

## 2. 索引重建（维护窗口）

迁移执行器内的事务不能用 `CONCURRENTLY`，大表上重建索引必须手动做
（应用侧不停写也可安全执行）：

```sql
-- 提升构建吞吐（会话级，一次性）
set maintenance_work_mem = '512MB';
set max_parallel_maintenance_workers = 2;

-- 例：未来出现其它状态组合的向量查询时，重建全量 fp32 索引
create index concurrently idx_memories_embedding
  on memories using hnsw (embedding vector_cosine_ops);

-- 例：更换嵌入模型（维度/模型变化）后重建 halfvec 索引
drop index concurrently if exists idx_memories_embedding_hf;
create index concurrently idx_memories_embedding_hf
  on memories using hnsw ((embedding::halfvec(1024)) halfvec_cosine_ops)
  where status in ('active', 'archived');
```

注意：`embedding` 列维度（1024）与 `HUB_EMBED_DIM` 绑定；换模型必须同步迁移、
配置与 `similar()` 的 halfvec typmod。

## 3. 观测

```sql
-- 索引大小（halfvec 应约为 fp32 的一半）
select indexrelname, pg_size_pretty(pg_relation_size(indexrelid)) as size
from pg_stat_user_indexes where relname = 'memories';

-- 死元组与 autovacuum（表级 5% 阈值，迁移 015 设置）
select n_live_tup, n_dead_tup, last_autovacuum, last_autoanalyze
from pg_stat_user_tables where relname = 'memories';

-- ANN 查询是否命中表达式索引（应出现 idx_memories_embedding_hf 的 Index Scan；
-- 表很小时 Seq Scan + Sort 反而正常，数据过万后必须走索引）
explain (analyze, buffers)
select id from memories
where namespace = 'ns' and status = 'active' and embedding is not null
order by embedding::halfvec(1024) <=> '[0.1, ...]'::halfvec(1024)
limit 20;
```

## 4. 写入路径注意事项

- `memories` 表级 autovacuum 已收紧（vacuum 5% / analyze 2%）；死元组持续
  增长说明 worker/autovacuum 被饿着了，先看 `pg_stat_user_tables` 再动参数。
- tsvector GIN（`idx_memories_tsv_tokens`）默认开 fastupdate：写入尖峰
  （pending list 满时的合并停顿）出现时，对该索引 `set (fastupdate = off)`
  或调小 `gin_pending_list_limit`。
- HNSW / GIN 都随行更新累积膨胀：长期运行后用 `pg_repack` 或维护窗口
  `reindex concurrently` 回收，平时以第 3 节的索引大小曲线判断时机。
