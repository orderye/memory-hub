-- 14. 认知查重扫描索引：memories (namespace, updated_at desc)
--
-- 背景：`MemoryStore::all_in_namespace_limited`（chat 认知查重 / 演化目标选择）
-- 此前按 `created_at` 升序截断，大命名空间下只看到最老的一批条目，
-- 新重复会漏判。改为 `updated_at desc` 截断后，若无配套索引，
-- 每次查重都要对该命名空间全量排序取前 N。
--
-- `updated_at` 随每次修订更新，普通 btree 即可；desc 与 asc 在 btree
-- 上只需反向扫描，这里显式写 desc 与查询排序一致，便于执行计划对齐。

create index if not exists idx_memories_ns_updated
    on memories (namespace, updated_at desc);
