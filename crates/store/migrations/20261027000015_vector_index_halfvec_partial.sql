-- 15. 向量索引重建：halfvec 半精度 + 部分索引；清理死索引；memories 表级 autovacuum
--
-- 检索性能整改（2026-10-27）三件事：
--
-- 1. 死索引 idx_memories_tsv：初始 schema 建在 to_tsvector('simple', content) 上，
--    迁移 005 之后全部查询都走 tsv_tokens 表达式（见 store 层 push_memory_filters
--    与 list 的排序），该索引零查询引用、纯写入放大（8KB 中文内容展开 2-gram
--    后 GIN 条目很大，每次 upsert 都在白养它）。
--
-- 2. 向量索引 halfvec 化 + 部分化：
--    - fp32 vector(1024) 每行 4KB，halfvec 索引体积减半；小内存部署（2GB VPS）
--      的 page cache 命中率直接受益。bge-m3 场景 fp16 距离漂移在小数点后第三位，
--      默认阈值 0.65 不受影响（eval 套件可复核）。
--    - 查询恒带 status 过滤（默认 active；回溯时 active+archived），merged
--      行（被整合取代的历史）不进索引。部分索引谓词与两种查询形态都匹配。
--    - 旧的全量 fp32 索引 idx_memories_embedding 一并移除：现有查询形态已全部
--      被部分索引覆盖，留着就是双份写放大。若未来出现其它状态组合的向量查询，
--      按需重建全量索引（见 docs/pg-maintenance-runbook.md）。
--    - 查询侧（store 层 similar()）的 dist 表达式必须与索引表达式逐字一致：
--      `embedding::halfvec(1024) <=> $q`，否则规划器不认这个表达式索引。
--    - 本迁移假设在空表或小表上执行（2026-10-27 生产 memories 为 0 行）；
--      大表上应改用 CREATE INDEX CONCURRENTLY 在维护窗口手动执行。
--
-- 3. memories 高频 upsert（version/updated_at 每次修订都动），默认
--    autovacuum_vacuum_scale_factor=0.1 在大表上推迟清理，放大 HNSW/GIN
--    死元组；收紧到 5% 并同步 analyze。

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
