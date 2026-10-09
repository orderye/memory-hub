//! 记忆中枢业务服务接口与契约类型（基于设计规范 17.1 与 17.2 节）

use crate::domain::{ItemStatus, Layer, MemoryItem, Provenance, UsePolicy};
use async_trait::async_trait;
use common::{AuthCtx, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

// ==================== hub_context ====================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextRequest {
    pub conversation_id: Option<String>,
    pub turn_id: String,
    #[serde(default)]
    pub scene_tags: Vec<String>,
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default)]
    pub interaction_context: Option<InteractionContext>,
    #[serde(default)]
    pub user_flags: Option<UserFlags>,
    #[serde(default)]
    pub client_hints: Option<ClientHints>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InteractionContext {
    pub modality: Option<String>,
    pub shared_device: Option<bool>,
    pub speaker_verified: Option<bool>,
    pub locale: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserFlags {
    pub minor_mode: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientHints {
    pub token_budget: Option<usize>,
    pub latency_budget_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextResponse {
    pub pack_version: String,
    pub degraded: bool,
    pub tier: String,
    pub memory_pack: Vec<MemoryPackItem>,
    pub norm_pack: NormPack,
    pub rubric: Rubric,
    pub guidance: Guidance,
    pub instructions: String,
    pub notice: String,
    pub token_estimate: usize,
    pub cache_ttl_s: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryPackItem {
    pub id: String,
    pub text: String,
    pub kind: String,
    pub provenance: String,
    pub confidence: f32,
    pub use_policy: String,
    pub ring: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormPack {
    pub rules: Vec<NormRuleRef>,
    pub conflict_hits: Vec<String>,
    pub relation_policies: Option<RelationPolicies>,
    pub ambiguous: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormRuleRef {
    pub id: String,
    pub level: String,
    pub then: String,
    pub text: Option<String>,
    pub core: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelationPolicies {
    pub emotional: Option<String>,
    pub mixed: Option<String>,
    pub instrumental: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rubric {
    pub tier: String,
    pub gates: Vec<String>,
    pub dims: Vec<RubricDim>,
    pub tau: f32,
    pub max_revisions: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RubricDim {
    pub id: String,
    pub weight: f32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Guidance {
    pub tone: Option<String>,
    pub address: Option<String>,
    #[serde(default)]
    pub memory_anchors: Vec<String>,
    #[serde(default)]
    pub do_not_mention: Vec<String>,
}

#[async_trait]
pub trait ContextService: Send + Sync {
    async fn context(&self, a: &AuthCtx, r: ContextRequest) -> Result<ContextResponse>;
}

// ==================== hub_observe ====================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObserveRequest {
    pub conversation_id: Option<String>,
    pub turn_id: String,
    #[serde(default)]
    pub hypotheses: Vec<HypothesisInput>,
    pub predicted_reaction: Option<PredictedReaction>,
    #[serde(default)]
    pub events: Vec<ObserveEventInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HypothesisInput {
    pub kind: String,
    pub content: String,
    pub confidence: f32,
    #[serde(default)]
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PredictedReaction {
    pub kind: String,
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObserveEventInput {
    pub r#type: String,
    pub ts: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObserveResponse {
    pub accepted: usize,
    pub rejected: Vec<RejectedItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectedItem {
    pub index: usize,
    pub reason: String,
}

#[async_trait]
pub trait ObserveService: Send + Sync {
    async fn observe(&self, a: &AuthCtx, r: ObserveRequest) -> Result<ObserveResponse>;
}

// ==================== hub_check ====================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckRequest {
    pub turn_id: String,
    pub self_eval: SelfEval,
    #[serde(default)]
    pub rule_hits: Vec<RuleHitInput>,
    /// 草稿处理后立即丢弃，不得落库或写日志
    pub draft: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelfEval {
    pub gates: HashMap<String, bool>,
    pub scores: HashMap<String, f32>,
    pub revisions: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleHitInput {
    pub rule_id: String,
    pub hit: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResponse {
    pub gate_results: HashMap<String, bool>,
    pub advice: Vec<String>,
}

#[async_trait]
pub trait CheckService: Send + Sync {
    async fn check(&self, a: &AuthCtx, r: CheckRequest) -> Result<CheckResponse>;
}

// ==================== hub_feedback ====================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedbackRequest {
    pub turn_id: String,
    pub r#type: String,
    pub target: Option<String>,
    pub detail: Option<String>,
}

#[async_trait]
pub trait FeedbackService: Send + Sync {
    async fn feedback(&self, a: &AuthCtx, r: FeedbackRequest) -> Result<()>;
}

// ==================== hub_memory_* ====================

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryFilter {
    pub layer: Option<Layer>,
    pub provenance: Option<Provenance>,
    pub status: Option<ItemStatus>,
}

pub type Cursor = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<Cursor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryPatch {
    pub text: Option<String>,
    pub use_policy: Option<UsePolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Confirm {
    pub confirm_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DeleteTarget {
    Single(Uuid),
    Scope {
        layer: Option<Layer>,
        source_client: Option<String>,
        all: bool,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ExportFormat {
    Json,
    Csv,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceLink {
    pub url: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum PauseScope {
    Collect,
    Use,
    Both,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PauseState {
    pub paused: bool,
    pub scope: PauseScope,
}

#[async_trait]
pub trait MemoryAdminService: Send + Sync {
    async fn list(
        &self,
        a: &AuthCtx,
        f: MemoryFilter,
        c: Option<Cursor>,
    ) -> Result<Page<MemoryItem>>;
    async fn edit(
        &self,
        a: &AuthCtx,
        id: Uuid,
        patch: MemoryPatch,
        confirm: Confirm,
    ) -> Result<MemoryItem>;
    async fn delete(&self, a: &AuthCtx, t: DeleteTarget, confirm: Confirm) -> Result<u64>;
    async fn export(&self, a: &AuthCtx, fmt: ExportFormat) -> Result<ResourceLink>;
    async fn pause(&self, a: &AuthCtx, on: bool, scope: PauseScope) -> Result<PauseState>;
}
