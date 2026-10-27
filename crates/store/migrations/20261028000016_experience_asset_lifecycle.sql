-- 迁移 000016: 经验资产生命周期与版本历史
--
-- 目标（任务书 P2 / 4.2 / 4.3 / 4.6）：
-- 1. 经验条目升级为可审核的知识资产：新增 rejected 状态与状态流转原因；
-- 2. 新增 version 与 memory_experience_versions（只追加），经验内容变更不丢历史；
-- 3. 记录验证元数据（verified_at / verified_by / verification_note）与来源计数；
-- 4. 支持“被替代”的显式指向（superseded_by），保留旧版本可追溯。
--
-- 兼容性：
-- - 全部为 additive 变更，不改名/不删除既有列；
-- - 既有行回填 version=1、source_count=实际关联数，可重复执行（幂等）；
-- - 旧客户端不传新字段时由 serde 默认值兜底。

-- 1. 状态约束：candidate / active / deprecated / rejected / archived
--    先按定义内容匹配并删除旧约束（不依赖自动生成的约束名），再重建。
do $$
declare
    c record;
begin
    for c in
        select conname
        from pg_constraint
        where conrelid = 'memory_experiences'::regclass
          and contype = 'c'
          and pg_get_constraintdef(oid) like '%status%'
    loop
        execute format('alter table memory_experiences drop constraint %I', c.conname);
    end loop;
end $$;

alter table memory_experiences
    add constraint memory_experiences_status_check
    check (status in ('candidate', 'active', 'deprecated', 'rejected', 'archived'));

alter table memory_experiences
    add column if not exists version integer not null default 1,
    add column if not exists superseded_by uuid
        references memory_experiences(id) on delete set null,
    add column if not exists verified_at timestamptz,
    add column if not exists verified_by text,
    add column if not exists verification_note text,
    add column if not exists status_reason text,
    add column if not exists source_count integer not null default 0;

-- 2. 来源计数回填：可重复执行（每次重算为真实值）
update memory_experiences e
set source_count = coalesce(s.cnt, 0)
from (
    select experience_id, count(distinct task_id) as cnt
    from memory_task_experiences
    group by experience_id
) s
where s.experience_id = e.id
  and e.source_count <> s.cnt;

update memory_experiences e
set source_count = 0
where not exists (
    select 1 from memory_task_experiences l where l.experience_id = e.id
)
  and e.source_count <> 0;

-- 3. 只追加版本历史：每次内容型变更前写入上一版本快照
create table if not exists memory_experience_versions (
    id uuid primary key,
    experience_id uuid not null references memory_experiences(id) on delete cascade,
    namespace text not null references namespaces(namespace) on delete cascade,
    version integer not null,
    snapshot jsonb not null,
    change_kind text not null,
    changed_by text not null,
    status text not null,
    note text,
    changed_at timestamptz not null,
    unique (experience_id, version)
);

create index if not exists idx_memory_experience_versions_ns
    on memory_experience_versions (namespace, experience_id, version desc);
