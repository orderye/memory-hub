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

pub mod access;
pub mod account_scope;
pub mod algorithm;
pub mod backup_service;
pub mod domain;
pub mod infra;
pub mod integration_service;
pub mod memory_service;
pub mod testing;

pub use access::{validate_key, validate_namespace, GrantChecker};
pub use account_scope::{scope_name, unscope_name, AccountScopedStore};
pub use algorithm::{
    calculate_item_score, cosine_similarity, keyword_score, l2_normalize, mmr_rerank,
    recency_score, SearchQuery,
};
pub use backup_service::{
    grant_checker, BackupCreateRequest, BackupService, DefaultBackupService, PruneReport,
    RestoreRequest,
};
pub use domain::{
    BackupRecord, BackupSummary, IntegrationKind, IntegrationProposal, MemoryEntry, MemoryFilter,
    MemoryKind, MemoryPatch, MemoryStats, MemoryStatus, MemoryVersion, Namespace,
    NamespaceGrantRecord, NamespaceInfo, NewMemory, Page, ProposalStatus, RestoreMode,
    RestoreReport, Scored, VersionChange,
};
pub use infra::{
    AccessChecker, AccountRecord, AccountStore, AgentRecord, AgentRegistration, AgentStore,
    BackupStore, Clock, Embedder, GrantStore, IntegrationStore, MemoryHubStore, MemoryStore,
    Retriever, SessionRecord, SystemClock, TokenIntrospector,
};
pub use integration_service::{
    DefaultIntegrationService, IntegrationService, ResolveAction, ResolveRequest, ScanReport,
    ScanRequest,
};
pub use memory_service::{
    decode_cursor, encode_cursor, DefaultMemoryService, GetRequest, ListRequest, MemoryService,
    PutRequest, SimpleRetriever,
};
pub use testing::TestEmbedder;
