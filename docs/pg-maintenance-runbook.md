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

### 2.1 顺序与锁：先建后删

上面的示例刻意写成 **先 `create index concurrently` 新索引，再 `drop index concurrently` 旧索引**。
反过来（先删后建）会让线上查询在一段时间里完全没有向量索引。迁移 015 内部顺序是
"先建新索引、再删旧索引"，但它用的是普通 `create index`（事务内、持 `ACCESS EXCLUSIVE` 锁），
只适合空表/小表；大表必须走本节的 concurrent 路径。

维护窗口建议操作序列：

```sql
-- 0) 观察有没有阻塞中的 DDL
select pid, state, wait_event_type, left(query, 80)
from pg_stat_activity where state <> 'idle' order by query_start;

-- 1) 建新索引（不阻塞读写）
create index concurrently idx_memories_embedding_hf_new
  on memories using hnsw ((embedding::halfvec(1024)) halfvec_cosine_ops)
  where status in ('active', 'archived');

-- 2) 有效性检查（must be true / true）
select c.relname, i.indisvalid, i.indisready
from pg_index i join pg_class c on c.oid = i.indexrelid
where c.relname = 'idx_memories_embedding_hf_new';

-- 3) 换名 + 删旧（三步都在同一维护窗口内）
alter index idx_memories_embedding_hf rename to idx_memories_embedding_hf_old;
alter index idx_memories_embedding_hf_new rename to idx_memories_embedding_hf;
drop index concurrently idx_memories_embedding_hf_old;

-- 4) 计划验证（应出现 Index Scan using idx_memories_embedding_hf）
explain (analyze, buffers)
select id from memories
where namespace = 'ns' and status = 'active' and embedding is not null
order by embedding::halfvec(1024) <=> '[0.1, ...]'::halfvec(1024) limit 20;
```

### 2.2 失败与重试

- 启动迁移执行器（`crates/store/src/postgres/mod.rs::run_migrations`）每个迁移一个事务：
  `set local statement_timeout = 0` → `pg_advisory_xact_lock` → 账本检查 → 执行 → 记账 → commit。
- 迁移 015 中途失败（锁等待被取消、磁盘满、语句被杀）时整条事务回滚：**索引不会留下半成品，
  `_hub_migrations` 也不会记录该版本**，下次启动自动重试。
- 手工执行 concurrent 建索引失败时，PostgreSQL 会留下一个 `indisvalid = false` 的索引对象；
  必须先 `drop index concurrently <name>` 再重试，不能直接重跑 `create index`（会因同名已存在而跳过）。
- CI 里另有一条"逐文件 `psql -f`"的路径（`.github/workflows/ci.yml`），它每个文件是隐式事务，
  与运行时语义接近但不完全等同；两者都要能跑通。

### 2.3 旧版本回滚后的性能影响（重要）

迁移 015 只重建索引、不改表结构，回滚应用版本**能启动**，但旧二进制的查询是 fp32 形式
`embedding <=> $1`，与 halfvec 表达式索引 `((embedding::halfvec(1024)) ...)` 不匹配，
规划器不会用该索引，向量检索退化为 **Seq Scan + Sort**。

- 数据量小时无感；数据上万后延迟会随表线性增长；
- 需要回滚到 015 之前的二进制时，请在维护窗口**临时重建 fp32 索引**：

```sql
create index concurrently idx_memories_embedding
  on memories using hnsw (embedding vector_cosine_ops);
```

- 回滚窗口结束后再 `drop index concurrently idx_memories_embedding`（新版本用不到它，留着是双份写放大）。

> 结论：**"旧版能启动"不等于"性能兼容"**。生产回滚演练请同时验证查询计划，而不只看服务是否起来。

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
--
-- 注意：状态条件必须写成**字面量**。部分索引的谓词是
-- `status in ('active','archived')`，若写成参数化数组（应用里的
-- `status = any($1)`）规划器无法证明蕴含，索引不会被选用。
-- 应用侧已在 store 层把状态内联为字面量（见 ann_status_predicate）；
-- 手工 EXPLAIN 也必须用字面量，否则会误判"索引无效"。
explain (analyze, buffers)
select id from memories
where namespace = 'ns' and status in ('active') and embedding is not null
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

<!-- kimi-badge -->
[![powered by KimiCode](../.github/assets/kimi-badge.svg)](https://www.kimi.com/code)
