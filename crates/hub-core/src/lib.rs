#![forbid(unsafe_code)]

//! 记忆中枢核心领域模型与业务抽象（hub-core）
//!
//! 本 crate 遵循纯领域设计原则，无外部 IO、无 Web/DB 强依赖。

pub mod algorithm;
pub mod assembler;
pub mod check_service;
pub mod domain;
pub mod fusion;
pub mod infra;
pub mod observe_service;
pub mod service;

pub use check_service::{DefaultCheckService, DefaultFeedbackService};
pub use fusion::{MemoryEvidenceItem, MemoryFusionEngine};
pub use observe_service::DefaultObserveService;

pub use algorithm::{
    apply_decay, calculate_item_score, can_promote_to_belief, check_margin, mmr_rerank,
    update_confidence_on_event, ConfidenceEvent, ItemScoreParams, MarginResult, PromotionCandidate,
};
pub use assembler::Assembler;
pub use domain::{ItemStatus, Layer, MemoryItem, Provenance, Sensitivity, UsePolicy};
pub use infra::{
    AuditEvent, AuditSink, CheckInput, CheckResult, Clock, ConflictFlag, ConflictTable,
    DecayReport, Embedder, MemoryStore, NewMemory, PackRegistry, PackSet, Resolution,
    RetrievalQuery, Retriever, Rule, RuleEngine, Scored, StoreQuery, SystemClock,
};
pub use service::{
    CheckRequest, CheckResponse, CheckService, Confirm, ContextRequest, ContextResponse,
    ContextService, DeleteTarget, ExportFormat, FeedbackRequest, FeedbackService, Guidance,
    InteractionContext, MemoryAdminService, MemoryFilter, MemoryPackItem, MemoryPatch, NormPack,
    NormRuleRef, ObserveRequest, ObserveResponse, ObserveService, Page, PauseScope, PauseState,
    Rubric, RubricDim, SelfEval, UserFlags,
};
