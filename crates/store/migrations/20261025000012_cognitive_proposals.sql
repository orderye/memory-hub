-- 12. 认知提案持久化：Chat 认知提炼的人工审查队列
--
-- 背景：process_chat_session 生成的 CognitiveProposalRecord 此前只存在于
-- 返回值中，默认治理模式 Manual 下所有提炼结果直接丢失。本表给出
-- 认知提案的持久层，供 propose → list → review/resolve 审查闭环。
--
-- 设计决策（为什么是独立表而不是复用 integration_proposals）：
-- - 状态机不同：认知提案是 candidate/pending_review/accepted/rejected/superseded，
--   整合提案是 open/merged/dismissed/expired，CHECK 约束与消费方完全不同；
-- - 字段不同：认知提案的载荷是完整候选（candidate JSONB），没有
--   member_ids/similarity/kind 的概念；
-- - 混表会污染整合路径：open_exists 查重、memory_integration_list、
--   consolidation 统计都按整合语义查询，认知提案混入后每条查询都要
--   加类型过滤，且制造新的 ID 混用面。
-- 两套提案 ID 空间保持独立，裁决入口按提案实际归属分发。
--
-- 兼容性：
-- - 全新表，无存量数据迁移负担；
-- - candidate 载荷为 JSONB（CognitiveCandidate 整体序列化），未来字段
--   演进走 Rust 侧 serde 兼容，不动表结构；
-- - 幂等：可在已含该表的库上重复执行。

create table if not exists cognitive_proposals (
    id uuid primary key,
    namespace text not null references namespaces(namespace) on delete cascade,
    -- 完整候选载荷（CognitiveCandidate 序列化），只含结构化提炼结果与
    -- 已脱敏的候选正文，不含原始对话全文
    candidate jsonb not null,
    source_evidence_id uuid not null,
    status text not null default 'pending_review'
        check (status in ('candidate', 'pending_review', 'accepted', 'rejected', 'superseded')),
    rejection_reason text,
    resulting_memory_id uuid,
    resolved_by text,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);

create index if not exists idx_cognitive_proposals_ns_status
    on cognitive_proposals (namespace, status);

create index if not exists idx_cognitive_proposals_created
    on cognitive_proposals (namespace, created_at desc);

comment on table cognitive_proposals is
    '认知提案（Chat 认知提炼的人工审查队列）；与 integration_proposals 是两套独立 ID 空间，不得混用';
