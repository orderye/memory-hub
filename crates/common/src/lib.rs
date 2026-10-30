#![forbid(unsafe_code)]

//! 多智能体长期记忆服务共享基础库（错误、鉴权、配置、幂等、限流、脱敏日志、cron）

pub mod auth;
pub mod cognitive;
pub mod config;
pub mod consolidation;
pub mod cron;
pub mod error;
pub mod idempotency;
pub mod log;
pub mod profile;
pub mod rate_limit;
pub mod runtime;

pub use auth::{AccountId, Acl, AgentId, AuthCtx, NamespaceGrant, Scope, BOOTSTRAP_ACCOUNT_ID};
pub use cognitive::CognitiveExtractorMode;
pub use config::{AppConfig, DEFAULT_INTEGRATION_SCAN_MAX_ENTRIES, DEFAULT_RETRIEVAL_MIN_SCORE};
pub use consolidation::{
    ConsolidationFallback, ConsolidationMode, ConsolidationModeStatus,
    CONSOLIDATION_STRATEGY_VERSION,
};
pub use error::{ErrorDetail, HubError, Result};
pub use idempotency::{
    result_hash, IdempotencyCheck, IdempotencyEntry, IdempotencyGuard, IdempotencyKey,
    IdempotencyRecord, IdempotencyStore, InMemoryIdempotencyStore,
};
pub use log::{init_tracing, init_tracing_with_stdio};
pub use profile::{DeploymentProfile, DEPLOYMENT_PROFILE_ENV};
pub use rate_limit::{
    is_capacity_enabled, rate_limit_error, InMemoryRateLimitStore, RateDimension, RateLimitStore,
};
pub use runtime::build_runtime;
