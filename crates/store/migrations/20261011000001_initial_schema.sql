-- 多智能体长期记忆服务初始 schema（PostgreSQL 15+ / pgvector）
--
-- 定位：多 Agent 长期记忆的存储、备份、整合。
-- 主体是 Agent（OAuth client），记忆按命名空间（namespace）隔离。

create extension if not exists "uuid-ossp";
create extension if not exists "vector";

-- 1. Agent 注册（OAuth 客户端即 Agent）
create table if not exists agents (
  agent_id text primary key,
  name text not null,
  kind text not null default 'service',              -- service | assistant | worker
  status text not null default 'active',             -- active | suspended
  client_secret_hash text,                           -- 机密客户端（client_credentials 授予）
  redirect_uris text[] not null default '{}',
  max_scopes text[] not null default '{memory:read,memory:write}',
  created_at timestamptz not null default now()
);

-- 2. 命名空间与授权
create table if not exists namespaces (
  namespace text primary key,
  owner_agent text not null references agents(agent_id) on delete cascade,
  created_at timestamptz not null default now()
);

create table if not exists namespace_grants (
  namespace text not null references namespaces(namespace) on delete cascade,
  agent_id text not null references agents(agent_id) on delete cascade,
  can_read boolean not null default true,
  can_write boolean not null default false,
  granted_by text not null,
  created_at timestamptz not null default now(),
  primary key (namespace, agent_id)
);

-- 3. 记忆核心表
create table if not exists memories (
  id uuid primary key,
  namespace text not null references namespaces(namespace) on delete cascade,
  key text,
  kind text not null check (kind in ('fact','preference','episode','note','skill','procedure')),
  content text not null check (length(content) between 1 and 8192),
  tags text[] not null default '{}',
  metadata jsonb not null default '{}',
  embedding vector(1024),                            -- 维度须与 HUB_EMBED_DIM 一致
  embedding_model text,
  source_agent text not null,
  confidence real not null default 1.0 check (confidence between 0 and 1),
  status text not null default 'active' check (status in ('active','archived','merged')),
  version bigint not null default 1,
  expires_at timestamptz,
  merged_into uuid references memories(id) on delete set null,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  unique (namespace, key)
);

-- 列表分页主序：(namespace, updated_at desc, id desc)
create index if not exists idx_memories_ns_updated on memories (namespace, updated_at desc, id desc);
create index if not exists idx_memories_ns_status on memories (namespace, status);
create index if not exists idx_memories_kind on memories (namespace, kind);
create index if not exists idx_memories_expires on memories (expires_at) where status = 'active';
-- 全文检索（simple 配置：按空白/标点切分，中英文均可用）
create index if not exists idx_memories_tsv on memories using gin (to_tsvector('simple', content));
-- 向量近邻（HNSW 余弦）
create index if not exists idx_memories_embedding on memories using hnsw (embedding vector_cosine_ops);

-- 4. 版本历史（备份与审计的事实来源）
create table if not exists memory_versions (
  memory_id uuid not null references memories(id) on delete cascade,
  version bigint not null,
  content text not null,
  tags text[] not null default '{}',
  metadata jsonb not null default '{}',
  confidence real not null,
  changed_by text not null,
  change_type text not null check (change_type in ('created','updated','archived','merged')),
  changed_at timestamptz not null default now(),
  primary key (memory_id, version)
);

-- 5. 备份快照
create table if not exists memory_backups (
  id uuid primary key,
  namespace text not null,                           -- '*' 表示全量
  created_by text not null,
  note text,
  entry_count integer not null default 0,
  checksum text not null,
  payload jsonb not null,
  retain_until timestamptz,
  created_at timestamptz not null default now()
);

create index if not exists idx_backups_ns_created on memory_backups (namespace, created_at desc);

-- 6. 整合提案
create table if not exists integration_proposals (
  id uuid primary key,
  namespace text not null references namespaces(namespace) on delete cascade,
  kind text not null check (kind in ('duplicate','conflict')),
  member_ids uuid[] not null,
  suggested_survivor uuid,
  similarity real not null default 0,
  status text not null default 'open' check (status in ('open','merged','dismissed','expired')),
  resolution_note text,
  created_at timestamptz not null default now(),
  resolved_at timestamptz
);

create index if not exists idx_proposals_ns_status on integration_proposals (namespace, status);

-- 7. OAuth 2.1 持久化
create table if not exists refresh_tokens (
  token_hash bytea primary key,
  agent_id text not null references agents(agent_id) on delete cascade,
  family_id uuid not null,
  scopes text[] not null,
  grants jsonb not null default '[]',
  expires_at timestamptz not null,
  rotated_at timestamptz,
  revoked_at timestamptz
);

create table if not exists confirm_tokens (
  token_hash bytea primary key,
  agent_id text not null references agents(agent_id) on delete cascade,
  op text not null,
  expires_at timestamptz not null,
  used_at timestamptz
);

-- 8. 幂等与限流
create table if not exists idempotency_keys (
  key text not null,
  agent_id text not null references agents(agent_id) on delete cascade,
  namespace text not null default '',
  op text not null,
  response_hash text,
  created_at timestamptz not null default now(),
  primary key (agent_id, namespace, op, key)
);

create table if not exists rate_limit_buckets (
  dimension text not null,
  bucket_key text not null,
  tokens real not null,
  updated_at timestamptz not null default now(),
  primary key (dimension, bucket_key)
);

-- 9. 只追加事件与审计日志（脱敏）
create table if not exists events (
  id bigserial primary key,
  agent_id text,
  namespace text,
  action text not null,
  payload jsonb not null default '{}',
  created_at timestamptz not null default now()
);

create table if not exists audit_log (
  id bigserial primary key,
  actor text not null,
  action text not null,
  target text,
  detail jsonb not null default '{}',
  created_at timestamptz not null default now()
);

-- 10. 权限硬防御：只追加角色（禁止 events / audit_log 的 update/delete/truncate）
do $$
begin
  if not exists (select from pg_roles where rolname = 'audit_writer') then
    create role audit_writer;
  end if;
end $$;

grant insert on events to audit_writer;
grant insert on audit_log to audit_writer;
revoke update, delete, truncate on events from audit_writer;
revoke update, delete, truncate on audit_log from audit_writer;
