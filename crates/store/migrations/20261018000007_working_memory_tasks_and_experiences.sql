-- 7. 认知记忆升级：Working 任务事实与经验记忆持久化 (P3)
--
-- 1. memory_tasks 表：任务事实（目标、执行状态、结果摘要、失败原因、证据存根）
-- 2. memory_experiences 表：经验记忆（可复用解决方案、前置条件、边界环境、失败模式、验证方法、置信度与反馈分）
-- 3. memory_task_experiences 表：任务事实与经验关联映射 (多对多)

create table if not exists memory_tasks (
    id uuid primary key,
    namespace text not null references namespaces(namespace) on delete cascade,
    scene text not null default 'working' check (scene in ('chat', 'working', 'coding')),
    task text not null,
    status text not null default 'running' check (status in ('running', 'completed', 'failed', 'cancelled')),
    started_at timestamptz not null default now(),
    finished_at timestamptz,
    result_summary text,
    failure_reason text,
    source_evidence_id uuid references memory_evidence(id) on delete set null,
    metadata jsonb not null default '{}'::jsonb
);

create index if not exists idx_memory_tasks_namespace_status
    on memory_tasks (namespace, status, started_at desc);

create table if not exists memory_experiences (
    id uuid primary key,
    namespace text not null references namespaces(namespace) on delete cascade,
    task text not null,
    search_keys text[] not null default '{}',
    solution text not null,
    preconditions jsonb not null default '{}'::jsonb,
    boundary_envs text[] not null default '{}',
    failure_modes text[] not null default '{}',
    validation jsonb not null default '{}'::jsonb,
    confidence real not null default 0.5 check (confidence >= 0.0 and confidence <= 1.0),
    status text not null default 'candidate' check (status in ('candidate', 'active', 'deprecated', 'archived')),
    feedback_score real not null default 1.0 check (feedback_score >= 0.0 and feedback_score <= 1.0),
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);

create index if not exists idx_memory_experiences_namespace_status
    on memory_experiences (namespace, status, updated_at desc);

create table if not exists memory_task_experiences (
    task_id uuid not null references memory_tasks(id) on delete cascade,
    experience_id uuid not null references memory_experiences(id) on delete cascade,
    relation_kind text not null default 'derived',
    created_at timestamptz not null default now(),
    primary key (task_id, experience_id, relation_kind)
);

create index if not exists idx_memory_task_experiences_experience
    on memory_task_experiences (experience_id, task_id);
