//! 基础设施接口抽象与可替换组件（基于设计规范 17.1 节）

use crate::domain::{ItemStatus, Layer, MemoryItem, Provenance, Sensitivity, UsePolicy};
use crate::service::DeleteTarget;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use common::{ClientId, Result, UserId};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

// ==================== MemoryStore ====================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewMemory {
    pub user_id: UserId,
    pub layer: Layer,
    pub text: String,
    pub provenance: Provenance,
    pub confidence: f32,
    pub sensitivity: Sensitivity,
    pub use_policy: UsePolicy,
    pub source_client: ClientId,
    pub ttl_seconds: Option<i64>,
    pub attrs: serde_json::Value,
}

#[derive(Debug, Clone, Default)]
pub struct StoreQuery {
    pub user_id: UserId,
    pub layers: Option<Vec<Layer>>,
    pub status: Option<ItemStatus>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DecayReport {
    pub decayed_count: u64,
    pub retired_count: u64,
    pub purged_count: u64,
}

#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn upsert(&self, item: NewMemory) -> Result<MemoryItem>;
    async fn get(&self, user: UserId, id: Uuid) -> Result<MemoryItem>;
    async fn query(&self, q: StoreQuery) -> Result<Vec<MemoryItem>>;
    async fn delete_cascade(&self, user: UserId, t: DeleteTarget) -> Result<u64>;
    async fn apply_decay(&self, now: DateTime<Utc>) -> Result<DecayReport>;
}

// ==================== Embedder ====================

#[async_trait]
pub trait Embedder: Send + Sync {
    fn model_id(&self) -> &str;
    fn dim(&self) -> usize;
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

// ==================== Retriever ====================

#[derive(Debug, Clone)]
pub struct RetrievalQuery {
    pub user_id: UserId,
    pub query_text: String,
    pub scene_tags: Vec<String>,
    pub token_budget: usize,
    pub allow_sensitive: bool,
    pub minor_mode: bool,
}

#[derive(Debug, Clone)]
pub struct Scored<T> {
    pub item: T,
    pub score: f32,
}

#[async_trait]
pub trait Retriever: Send + Sync {
    async fn retrieve(&self, q: RetrievalQuery) -> Result<Vec<Scored<MemoryItem>>>;
}

// ==================== PackRegistry ====================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackSet {
    pub pack_version: String,
    pub core_hash: String,
    pub raw_content: serde_json::Value,
}

pub trait PackRegistry: Send + Sync {
    fn active_for(&self, user: UserId) -> Arc<PackSet>;
    fn swap(&self, new: PackSet) -> Result<()>;
}

// ==================== RuleEngine ====================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckInput {
    pub draft: Option<String>,
    pub roles: Vec<String>,
    pub scene_tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub level: String,
    pub when_expr: String,
    pub then_action: String,
    pub core: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    pub passed: bool,
    pub violations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConflictFlag {
    C1,
    C2,
    C3,
    C4,
    C5,
    C6,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictTable {
    pub version: u32,
    pub entries: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resolution {
    pub strategy: String,
    pub advice: Option<String>,
}

pub trait RuleEngine: Send + Sync {
    fn check(&self, input: &CheckInput, rules: &[Rule]) -> CheckResult;
    fn resolve(&self, conflicts: &[ConflictFlag], table: &ConflictTable) -> Resolution;
}

// ==================== Clock ====================

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

#[derive(Debug, Default, Clone)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

// ==================== AuditSink ====================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub actor: String,
    pub action: String,
    pub target: Option<String>,
    pub detail: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

pub trait AuditSink: Send + Sync {
    fn record(&self, e: AuditEvent);
}
