-- 迁移 000017: Agent 记忆集合与绑定（任务书 4.4）
--
-- 语义边界：
-- 1. 集合是**账户级配置**，只描述候选访问范围（命名空间/场景/类型/标签/状态），
--    不包含任何记忆内容，也不授予任何权限；
-- 2. 绑定只决定“该 Agent 可以使用哪个集合”，实际读取仍走原始 ACL 与实时 grant；
-- 3. 删除集合级联删除绑定，但绝不删除底层记忆。

create table if not exists memory_collections (
    id uuid primary key,
    account_id uuid not null,
    name text not null,
    description text,
    filter jsonb not null default '{}'::jsonb,
    created_by text not null,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    constraint memory_collections_name_check check (char_length(name) between 1 and 64),
    unique (account_id, name)
);

create index if not exists idx_memory_collections_account
    on memory_collections (account_id, updated_at desc);

create table if not exists memory_collection_bindings (
    collection_id uuid not null references memory_collections(id) on delete cascade,
    agent_id text not null,
    created_by text not null,
    created_at timestamptz not null default now(),
    primary key (collection_id, agent_id)
);

create index if not exists idx_memory_collection_bindings_agent
    on memory_collection_bindings (agent_id);
