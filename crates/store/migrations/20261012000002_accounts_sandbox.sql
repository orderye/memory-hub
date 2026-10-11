-- 多账户系统与账户间沙箱隔离
--
-- 语义变更（v2.1）：
-- 1. 新增 accounts（人类账户，沙箱边界）与 account_sessions（控制台登录会话）；
-- 2. agents 归属账户（owner_account_id）；DCR 注册必须在账户身份下进行；
-- 3. 命名空间在存储层变为账户内键 `{account_id}/{namespace}`：
--    存量数据整体归属引导账户（固定 ID 见 common::BOOTSTRAP_ACCOUNT_ID）；
-- 4. 废除 `*` 通配授权：授权行必须精确匹配命名空间，跨账户无任何通路。
--
-- 幂等设计：本文件在 runtime 迁移账本与 docker initdb 两条路径下都可能执行，
-- 每条语句均可安全重放（旧键以"不含 /"判定，账户前缀含 /，二者不混淆）。

-- 1. 账户与会话
create table if not exists accounts (
  id uuid primary key,
  username text not null,
  display_name text not null default '',
  status text not null default 'active',              -- active | suspended
  password_hash text,                                 -- null = 登录禁用（如未设口令的引导账户）
  created_at timestamptz not null default now()
);

create unique index if not exists idx_accounts_username_lower on accounts (lower(username));

create table if not exists account_sessions (
  token_hash bytea primary key,                       -- sha256(会话令牌)
  account_id uuid not null references accounts(id) on delete cascade,
  expires_at timestamptz not null,
  created_at timestamptz not null default now()
);

create index if not exists idx_account_sessions_expires on account_sessions (expires_at);

-- 2. 引导账户（固定 ID；用户名/口令由 auth-server 启动时按环境变量确保与写入）
insert into accounts (id, username, display_name, status, created_at)
values ('00000000-0000-0000-0000-000000000001', 'bootstrap', '引导账户', 'active', now())
on conflict (id) do nothing;

-- 3. Agent 归属账户
alter table agents add column if not exists owner_account_id uuid references accounts(id);
create index if not exists idx_agents_owner_account on agents (owner_account_id);

-- 4. 废除通配授权（沙箱：授权只能在账户内精确授予）
delete from namespace_grants where namespace = '*';

-- 5. 存量命名空间前缀化：整体归属引导账户
-- 5a. 先插入带前缀的父行（子表外键此时仍指向旧行，不受影响）
insert into namespaces (namespace, owner_agent, created_at)
select '00000000-0000-0000-0000-000000000001/' || namespace, owner_agent, created_at
from namespaces
where position('/' in namespace) = 0
on conflict (namespace) do nothing;

-- 5b. 子表随迁（先子后父，满足外键约束）
update namespace_grants
set namespace = '00000000-0000-0000-0000-000000000001/' || namespace
where position('/' in namespace) = 0;

update memories
set namespace = '00000000-0000-0000-0000-000000000001/' || namespace
where position('/' in namespace) = 0;

update integration_proposals
set namespace = '00000000-0000-0000-0000-000000000001/' || namespace
where position('/' in namespace) = 0;

-- 备份表无外键；`*` 全量哨兵同样前缀化为 `{account}/*`
update memory_backups
set namespace = '00000000-0000-0000-0000-000000000001/' || namespace
where position('/' in namespace) = 0;

-- 幂等账本（namespace 可为空串，空串不参与前缀化）
update idempotency_keys
set namespace = '00000000-0000-0000-0000-000000000001/' || namespace
where namespace <> '' and position('/' in namespace) = 0;

-- 5c. 删除旧父行
delete from namespaces where position('/' in namespace) = 0;

-- 6. 回填 Agent 归属并收紧约束（重放安全：已非空时为无操作）
update agents
set owner_account_id = '00000000-0000-0000-0000-000000000001'
where owner_account_id is null;

alter table agents alter column owner_account_id set not null;