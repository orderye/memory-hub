-- 优化 key 唯一性约束为条件唯一索引
-- 允许已合并 (merged) 的历史记录保留原 key，仅 active 活跃记忆保证 key 唯一

alter table memories drop constraint if exists memories_namespace_key_key;

create unique index if not exists idx_memories_ns_key_active
  on memories (namespace, key)
  where status = 'active' and key is not null;
