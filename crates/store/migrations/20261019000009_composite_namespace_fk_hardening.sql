-- 9. 认知记忆安全硬化：复合唯一约束与命名空间复合外键强隔离
--
-- 1. 为 memories, memory_evidence, memory_tasks, memory_experiences, coding_threads 增加 (namespace, id) 复合唯一键
-- 2. 为 memory_relations 补充 namespace 字段与复合外键 (namespace, from_memory_id), (namespace, to_memory_id), (namespace, source_evidence_id)
-- 3. 为 memory_task_experiences 补充 namespace 字段与复合外键 (namespace, task_id), (namespace, experience_id)
-- 4. 为 coding_threads 补充复合外键 (namespace, task_id)
-- 5. 为 coding_revisions 补充复合外键 (namespace, thread_id), (namespace, source_task_id), (namespace, source_evidence_id)
-- 6. 为 memory_tasks 补充复合外键 (namespace, source_evidence_id)

-- 1. 父表补充 (namespace, id) 复合唯一约束
do $$
begin
    if not exists (select 1 from pg_constraint where conname = 'uq_memories_ns_id') then
        alter table memories add constraint uq_memories_ns_id unique (namespace, id);
    end if;
    if not exists (select 1 from pg_constraint where conname = 'uq_memory_evidence_ns_id') then
        alter table memory_evidence add constraint uq_memory_evidence_ns_id unique (namespace, id);
    end if;
    if not exists (select 1 from pg_constraint where conname = 'uq_memory_tasks_ns_id') then
        alter table memory_tasks add constraint uq_memory_tasks_ns_id unique (namespace, id);
    end if;
    if not exists (select 1 from pg_constraint where conname = 'uq_memory_experiences_ns_id') then
        alter table memory_experiences add constraint uq_memory_experiences_ns_id unique (namespace, id);
    end if;
    if not exists (select 1 from pg_constraint where conname = 'uq_coding_threads_ns_id') then
        alter table coding_threads add constraint uq_coding_threads_ns_id unique (namespace, id);
    end if;
end $$;

-- 2. 关系网 memory_relations 补充 namespace 列与复合外键
alter table memory_relations add column if not exists namespace text references namespaces(namespace) on delete cascade;

update memory_relations r
set namespace = m.namespace
from memories m
where r.from_memory_id = m.id and r.namespace is null;

alter table memory_relations alter column namespace set not null;

create index if not exists idx_memory_relations_ns on memory_relations (namespace);

do $$
begin
    if not exists (select 1 from pg_constraint where conname = 'fk_mem_rel_from_ns') then
        alter table memory_relations
            add constraint fk_mem_rel_from_ns
            foreign key (namespace, from_memory_id) references memories(namespace, id) on delete cascade;
    end if;
    if not exists (select 1 from pg_constraint where conname = 'fk_mem_rel_to_ns') then
        alter table memory_relations
            add constraint fk_mem_rel_to_ns
            foreign key (namespace, to_memory_id) references memories(namespace, id) on delete cascade;
    end if;
    if not exists (select 1 from pg_constraint where conname = 'fk_mem_rel_evidence_ns') then
        alter table memory_relations
            add constraint fk_mem_rel_evidence_ns
            foreign key (namespace, source_evidence_id) references memory_evidence(namespace, id) on delete set null;
    end if;
end $$;

-- 3. 任务-经验 memory_task_experiences 补充 namespace 列与复合外键
alter table memory_task_experiences add column if not exists namespace text references namespaces(namespace) on delete cascade;

update memory_task_experiences mte
set namespace = t.namespace
from memory_tasks t
where mte.task_id = t.id and mte.namespace is null;

alter table memory_task_experiences alter column namespace set not null;

create index if not exists idx_memory_task_experiences_ns on memory_task_experiences (namespace);

do $$
begin
    if not exists (select 1 from pg_constraint where conname = 'fk_mte_task_ns') then
        alter table memory_task_experiences
            add constraint fk_mte_task_ns
            foreign key (namespace, task_id) references memory_tasks(namespace, id) on delete cascade;
    end if;
    if not exists (select 1 from pg_constraint where conname = 'fk_mte_exp_ns') then
        alter table memory_task_experiences
            add constraint fk_mte_exp_ns
            foreign key (namespace, experience_id) references memory_experiences(namespace, id) on delete cascade;
    end if;
end $$;

-- 4. coding_threads 补充复合外键
do $$
begin
    if not exists (select 1 from pg_constraint where conname = 'fk_coding_threads_task_ns') then
        alter table coding_threads
            add constraint fk_coding_threads_task_ns
            foreign key (namespace, task_id) references memory_tasks(namespace, id) on delete set null;
    end if;
end $$;

-- 5. coding_revisions 补充复合外键
do $$
begin
    if not exists (select 1 from pg_constraint where conname = 'fk_coding_rev_thread_ns') then
        alter table coding_revisions
            add constraint fk_coding_rev_thread_ns
            foreign key (namespace, thread_id) references coding_threads(namespace, id) on delete cascade;
    end if;
    if not exists (select 1 from pg_constraint where conname = 'fk_coding_rev_task_ns') then
        alter table coding_revisions
            add constraint fk_coding_rev_task_ns
            foreign key (namespace, source_task_id) references memory_tasks(namespace, id) on delete set null;
    end if;
    if not exists (select 1 from pg_constraint where conname = 'fk_coding_rev_evidence_ns') then
        alter table coding_revisions
            add constraint fk_coding_rev_evidence_ns
            foreign key (namespace, source_evidence_id) references memory_evidence(namespace, id) on delete set null;
    end if;
end $$;

-- 6. memory_tasks 补充复合外键 (namespace, source_evidence_id)
do $$
begin
    if not exists (select 1 from pg_constraint where conname = 'fk_memory_tasks_evidence_ns') then
        alter table memory_tasks
            add constraint fk_memory_tasks_evidence_ns
            foreign key (namespace, source_evidence_id) references memory_evidence(namespace, id) on delete set null;
    end if;
end $$;
