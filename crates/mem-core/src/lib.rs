#![forbid(unsafe_code)]

//! 多智能体长期记忆服务核心库（core）
//!
//! 定位：多 Agent 长期记忆的**存储、备份、整合**基础设施。
//! 不做任何行为规范、技能框架或生成式推理——那些属于 Agent 自己的事。
//!
//! 模块划分：
//! - [`domain`]：记忆条目、备份快照、整合提案等核心类型
//! - [`infra`]：存储 / 嵌入 / 时钟 / 检索 / 令牌内省抽象
//! - [`account_scope`]：账户沙箱作用域（命名空间按账户前缀隔离）
//! - [`memory_service`]：记忆 CRUD 与检索门面
//! - [`backup_service`]：快照创建、恢复与保留策略
//! - [`integration_service`]：去重与冲突归并
//! - [`access`]：命名空间 ACL 与访问判定
//! - [`algorithm`]：打分、余弦与 MMR 等纯函数
//! - [`redaction`]：证据持久化前的凭证脱敏与最小化

pub mod access;
pub mod account_scope;
pub mod algorithm;
pub mod backup_service;
pub mod chat_cognitive_service;
pub mod cjk;
pub mod coding_memory_service;
pub mod domain;
pub mod fingerprint;
pub mod infra;
pub mod integration_service;
pub mod memory_service;
pub mod redaction;
pub mod testing;
pub mod working_memory_service;

pub use cjk::{
    fts_doc_tokens, fts_or_query, fts_query_hits, fts_query_tokens, tokenize_for_fts,
    tokenize_query_for_fts,
};

pub use access::{validate_key, validate_namespace, GrantChecker};
pub use account_scope::{scope_name, unscope_name, AccountScopedStore};
pub use algorithm::{
    calculate_item_score, calculate_item_score_with_availability, cosine_similarity,
    document_tokens, extract_terms, inverse_document_frequency, is_superseded_at, keyword_score,
    keyword_score_weighted, l2_normalize, layer_prior, matches_tag_filter, mmr_rerank,
    passes_validity_filter, recency_score, recency_tau_days_for, retrieval_reference_time,
    scene_preference_score, validity_filter_time, SceneMode, SearchQuery, SCENE_PREFER_BONUS,
    SCENE_PREFER_PENALTY,
};
pub use backup_service::{
    grant_checker, BackupCreateRequest, BackupService, DefaultBackupService, PruneReport,
    RestoreRequest,
};
pub use chat_cognitive_service::{
    ChatCognitiveResult, ChatCognitiveService, CognitiveReviewDecision,
};
pub use coding_memory_service::CodingMemoryService;
pub use domain::{
    is_intention_active, is_speculative_intention, validate_coding_revision,
    validate_cognitive_candidate, validate_experience_candidate, BackupRecord, BackupSummary,
    ChatCognitiveInput, ChatMessage, ChatRole, CodeChangeKind, CodingRevision, CodingThread,
    CognitiveCandidate, CognitiveGovernanceMode, CognitiveLayer, CognitivePolicy,
    CognitiveProposalRecord, CognitiveProposalStatus, EvidenceSourceKind, ExperienceCandidate,
    ExperienceStatus, IntegrationKind, IntegrationProposal, MemoryEntry, MemoryEvidence,
    MemoryExperience, MemoryFilter, MemoryKind, MemoryPatch, MemoryRelation, MemoryRelationKind,
    MemoryScene, MemoryStats, MemoryStatus, MemoryTask, MemoryTaskExperience, MemoryVersion,
    Namespace, NamespaceGrantRecord, NamespaceInfo, NewCodingRevision, NewCodingThread,
    NewEvidence, NewMemory, NewMemoryExperience, NewMemoryRelation, NewMemoryTask, Page,
    ProposalStatus, RestoreMode, RestoreReport, Scored, TaskStatus, VersionChange,
};
pub use fingerprint::{canonical_form, content_fingerprint, same_fingerprint};
pub use infra::{
    AccessChecker, AccountRecord, AccountStore, AgentRecord, AgentRegistration, AgentStore,
    BackupStore, ChatCognitiveExtractor, Clock, CodingStore, CognitiveProposalStore, Embedder,
    EvidenceStore, ExperienceStore, GrantStore, IntegrationStore, MemoryHubStore, MemoryStore,
    RelationStore, Retriever, SessionRecord, SystemClock, TaskStore, TokenIntrospector,
};
pub use integration_service::{
    IntegrationService, ResolveAction, ResolveRequest, ScanReport, ScanRequest,
};
pub use memory_service::{
    decode_cursor, encode_cursor, DefaultMemoryService, GetRequest, ListRequest, MemoryService,
    PutRequest,
};
pub use redaction::{
    redact, sanitize_evidence_excerpt, RedactionFinding, RedactionKind, RedactionReport,
    EVIDENCE_EXCERPT_MAX_CHARS,
};
pub use testing::{CountingEmbedder, FixedClock, SemanticTestEmbedder, TestEmbedder, TestHubStore};
pub use working_memory_service::{ExperienceSearchResult, WorkingMemoryService};
