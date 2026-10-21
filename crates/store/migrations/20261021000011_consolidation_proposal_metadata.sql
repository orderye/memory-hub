-- 整合提案溯源：记录候选关系、来源、原因码与整合模式
--
-- 背景：整合中枢支持两种候选发现模式（纯本地规则 / 向量辅助），
-- 提案行需要能回答"这条提案是怎么来的"（关系类型、候选来源、原因码、当时的模式）。
-- 既有 integration_proposals 表只有 kind（duplicate/conflict）一个两值枚举，
-- 无法承载细粒度关系分类，且没有可扩展的元数据列。
--
-- 兼容性：
-- - 新增列可空语义由 NOT NULL DEFAULT '{}' 保证，旧行读作空对象；
-- - 不修改 kind 的 CHECK 约束，历史数据的 kind 语义完全不变；
-- - integration_proposals 不在任何备份快照载荷内，因此**不需要**升快照版本，
--   旧快照恢复不受影响；
-- - 幂等：可在已含该列的库上重复执行。

alter table integration_proposals
  add column if not exists metadata jsonb not null default '{}'::jsonb;

comment on column integration_proposals.metadata is
  '提案溯源：relation / candidate_source / reason_code / mode / strategy_version / similarity。只含结构化元数据，不含原始记忆文本';
