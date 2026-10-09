-- 记忆中枢初始数据库迁移（基于设计规范 17.3 节）
-- PostgreSQL 15+，包含 pgvector 扩展

create extension if not exists "uuid-ossp";
create extension if not exists "vector";

-- 1. 用户与授权
create table if not exists users (
  id uuid primary key,
  status text not null default 'active',
  minor_mode boolean not null default false,
  personalization_enabled boolean not null default true,   -- hub_memory_pause 的结果
  ledger_enabled boolean not null default false,           -- 人情账，默认关闭
  created_at timestamptz not null default now()
);

create table if not exists oauth_clients (
  client_id text primary key,
  name text not null,
  org text,
  contact text,
  redirect_uris text[] not null,
  status text not null default 'pending',  -- pending | approved | suspended
  approved_by text,
  agreement_ref text,
  max_scopes text[] not null,
  created_at timestamptz not null default now()
);

create table if not exists consents (
  id uuid primary key,
  user_id uuid not null references users(id) on delete cascade,
  client_id text not null references oauth_clients(client_id) on delete cascade,
  scopes text[] not null,
  layers_read text[] not null,
  layers_write text[] not null,
  sensitive_layers text[] not null default '{}',           -- 单独同意的敏感层
  consent_text_version text not null,
  receipt_hash text not null,
  granted_at timestamptz not null default now(),
  revoked_at timestamptz
);

create table if not exists refresh_tokens (
  token_hash bytea primary key,
  user_id uuid not null references users(id) on delete cascade,
  client_id text not null references oauth_clients(client_id) on delete cascade,
  family_id uuid not null,
  scopes text[] not null,
  expires_at timestamptz not null,
  rotated_at timestamptz,
  revoked_at timestamptz
);

create table if not exists confirm_tokens (
  token_hash bytea primary key,
  user_id uuid not null references users(id) on delete cascade,
  op text not null,
  expires_at timestamptz not null,
  used_at timestamptz
);

-- 2. 记忆核心表
create table if not exists memory_items (
  id uuid primary key,
  user_id uuid not null references users(id) on delete cascade,
  layer text not null,
  text text not null,
  provenance text not null check (provenance in ('stated','observed','inferred')),
  confidence real not null check (confidence between 0 and 1),
  sensitivity text not null default 'normal',
  use_policy text not null default 'free',
  source_client text not null,
  status text not null default 'active',
  ttl_seconds bigint,
  expires_at timestamptz,
  attrs jsonb not null default '{}',
  tsv tsvector,                                            -- 全文检索
  embedding vector(1024),                                  -- 维度由系统配置统一决定
  embedding_model text,
  updated_at timestamptz not null default now(),
  created_at timestamptz not null default now()
);

create index if not exists idx_memory_items_user_layer on memory_items (user_id, layer, status);
create index if not exists idx_memory_items_tsv on memory_items using gin (tsv);
create index if not exists idx_memory_items_embedding on memory_items using hnsw (embedding vector_cosine_ops);

create table if not exists memory_evidence (
  id uuid primary key,
  item_id uuid not null references memory_items(id) on delete cascade,
  user_id uuid not null references users(id) on delete cascade,
  source_client text not null,
  kind text not null,
  context_tag text,
  observed_at timestamptz not null,
  payload jsonb not null default '{}'
);

create table if not exists hypotheses (                                  -- 循环一致性验证
  id uuid primary key,
  user_id uuid not null references users(id) on delete cascade,
  turn_id text not null,
  client_id text not null,
  kind text not null,
  content text not null,
  confidence real not null,
  predicted_reaction jsonb,
  status text not null default 'open',   -- open | matched | missed | expired
  outcome jsonb,
  resolved_at timestamptz,
  unique (user_id, client_id, turn_id, kind, content)
);

create table if not exists events (                                      -- 只追加，脱敏
  id bigserial primary key,
  user_id uuid,
  client_id text,
  turn_id text,
  type text not null,
  payload jsonb not null default '{}',
  created_at timestamptz not null default now()
);

create table if not exists idempotency_keys (
  key text not null,
  user_id uuid not null references users(id) on delete cascade,
  client_id text not null references oauth_clients(client_id) on delete cascade,
  op text not null,
  response_hash bytea,
  created_at timestamptz not null default now(),
  primary key (user_id, client_id, op, key)
);

-- 3. 规范库与 Skill 体系
create table if not exists norm_rules (
  rule_id text not null,
  version int not null,
  source_skill text not null,
  sub_source text not null,
  level text not null check (level in ('hard','soft')),
  scope jsonb not null,
  when_text text not null,
  then_action text not null,
  rationale text,
  evidence jsonb not null default '[]',
  reviewer text,
  core boolean not null default false,
  ttl_days int,
  review_by date,
  status text not null default 'candidate',                -- candidate | active | retired
  primary key (rule_id, version)
);

create table if not exists skills (
  id text not null,
  version text not null,
  manifest jsonb not null,
  status text not null default 'inactive',
  rollout jsonb not null default '{}',
  primary key (id, version)
);

create table if not exists conflict_tables (
  version int primary key,
  entries jsonb not null,
  status text not null
);

create table if not exists rubrics (
  tier text not null,
  version int not null,
  content jsonb not null,
  status text not null,
  primary key (tier, version)
);

create table if not exists pack_sets (
  pack_version text primary key,
  content_ref text not null,
  status text not null default 'draft',
  rollout_percent int not null default 0,
  core_hash text not null
);

create table if not exists source_whitelist (
  id text primary key,
  tier text not null check (tier in ('A','B','C','D')),
  name text not null,
  domain text,
  content_types text[] not null,
  status text not null default 'active'
);

create table if not exists national_events (
  event_id text not null,
  version int not null,
  content jsonb not null,
  status text not null default 'candidate',
  review_by date,
  primary key (event_id, version)
);

-- 4. 审计日志（只追加）
create table if not exists audit_log (
  id bigserial primary key,
  actor text not null,
  action text not null,
  target text,
  detail jsonb not null default '{}',
  created_at timestamptz not null default now()
);
