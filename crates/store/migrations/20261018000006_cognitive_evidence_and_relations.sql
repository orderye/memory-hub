-- 6. 认知记忆升级：证据溯源、时间有效性与实体关系网络 (P1)
--
-- 1. memories 表增量扩充：场景 (scene)、分层 (layer)、时间有效性 (valid_from/valid_until/occurred_at)
-- 2. memory_versions 表增量扩充：保留历史版本对应的 scene/layer/validity
-- 3. memory_evidence 表：不可变原始证据存根 (append-only)
-- 4. memory_relations 表：事实关系网 (supports, contradicts, supersedes, derived_from, related_to 等)

alter table memories add column if not exists scene text check (scene is null or scene in ('chat', 'working', 'coding'));
alter table memories add column if not exists layer text check (layer is null or layer in ('raw', 'fact', 'summary', 'identity', 'knowledge', 'schema', 'intention'));
alter table memories add column if not exists valid_from timestamptz;
alter table memories add column if not exists valid_until timestamptz;
alter table memories add column if not exists occurred_at timestamptz;

create index if not exists idx_memories_scene_layer on memories (namespace, scene, layer);
create index if not exists idx_memories_occurred on memories (namespace, occurred_at desc);

alter table memory_versions add column if not exists scene text;
alter table memory_versions add column if not exists layer text;
alter table memory_versions add column if not exists valid_from timestamptz;
alter table memory_versions add column if not exists valid_until timestamptz;
alter table memory_versions add column if not exists occurred_at timestamptz;

create table if not exists memory_evidence (
    id uuid primary key,
    namespace text not null references namespaces(namespace) on delete cascade,
    source_kind text not null,
    source_ref text,
    content_hash text not null,
    excerpt text,
    occurred_at timestamptz,
    created_at timestamptz not null default now()
);

create index if not exists idx_memory_evidence_ns_created
    on memory_evidence (namespace, created_at desc);

create index if not exists idx_memory_evidence_source_ref
    on memory_evidence (namespace, source_kind, source_ref);

create table if not exists memory_relations (
    from_memory_id uuid not null references memories(id) on delete cascade,
    to_memory_id uuid not null references memories(id) on delete cascade,
    relation_kind text not null,
    confidence real not null default 1.0 check (confidence >= 0.0 and confidence <= 1.0),
    source_evidence_id uuid references memory_evidence(id) on delete set null,
    created_at timestamptz not null default now(),
    primary key (from_memory_id, to_memory_id, relation_kind),
    check (from_memory_id <> to_memory_id)
);

create index if not exists idx_memory_relations_to
    on memory_relations (to_memory_id, relation_kind);

create index if not exists idx_memory_relations_evidence
    on memory_relations (source_evidence_id);
