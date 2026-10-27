-- 检索与列举路径缺失的索引（不改动表结构，仅补索引）
--
-- 依据 store 层实际发出的谓词补齐，避免生产库上的顺序扫描：
--
-- 1. `list` 的动态查询会带 `tags @> $n`（postgres.rs 的 MemoryFilter.tags），
--    而初始 schema 只给 tsvector 建了 GIN，text[] 的包含判断此前无任何索引可用。
-- 2. `get_by_key` 的谓词是 `namespace = $1 and key = $2`（不带 status 条件），
--    迁移 003 的 `idx_memories_ns_key_active` 是**条件**唯一索引（where status='active'），
--    查询谓词不蕴含索引谓词，规划器无法使用它，只能退化为该命名空间的全量扫描。
-- 3. `count` 与 `stats` 同样按 (namespace, kind, status) 组合过滤，
--    初始 schema 的 `idx_memories_kind` 已覆盖 (namespace, kind)，
--    这里补一个 (namespace, status, kind) 供统计聚合走索引。
--
-- 关于向量索引：`similar()` 带 `status = 'active'` 过滤，理论上更适合
-- partial HNSW（`where status = 'active'`）。但重建 HNSW 在本项目的迁移执行器里
-- 无法用 `CONCURRENTLY`（迁移语句跑在事务内，而 CONCURRENTLY 禁止事务内执行），
-- 在已有数据的库上原地重建会长时间持有锁。因此这一项**留作维护窗口的手动操作**，
-- 不在本文件里做。
-- （2026-10-27 已由迁移 015 在空表上落地：halfvec + partial，旧全量 fp32 索引移除。）

create index if not exists idx_memories_tags_gin on memories using gin (tags);

create index if not exists idx_memories_ns_key
  on memories (namespace, key)
  where key is not null;

create index if not exists idx_memories_ns_status_kind on memories (namespace, status, kind);
