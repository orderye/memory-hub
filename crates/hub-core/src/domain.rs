//! 记忆中枢核心领域类型（严格对应设计规范 17.1 节）

use chrono::{DateTime, Duration, Utc};
use common::{ClientId, UserId};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 记忆分层
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    Fact,
    Preference,
    State,
    Relation,
    Behavior,
    GoalBelief,
    NormPref,
    Ledger,
}

impl Layer {
    pub fn as_str(&self) -> &'static str {
        match self {
            Layer::Fact => "fact",
            Layer::Preference => "preference",
            Layer::State => "state",
            Layer::Relation => "relation",
            Layer::Behavior => "behavior",
            Layer::GoalBelief => "goal_belief",
            Layer::NormPref => "norm_pref",
            Layer::Ledger => "ledger",
        }
    }
}

/// 记忆来源可信度分类
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// 用户明确明说
    Stated,
    /// 行为观察归纳
    Observed,
    /// 模型推断得出
    Inferred,
}

/// 敏感度级别
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    Normal,
    Sensitive,
    Blocked,
}

/// 使用策略
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsePolicy {
    /// 自由使用
    Free,
    /// 涉及共享设备或敏感时先追问确认
    Ask,
    /// 绝不外显提及
    NeverMention,
}

/// 条目生命周期状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    Active,
    PendingConfirm,
    Retired,
}

/// 记忆条目实体
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryItem {
    pub id: Uuid,
    pub user_id: UserId,
    pub layer: Layer,
    pub text: String,
    pub provenance: Provenance,
    /// 置信度 0.0 ~ 1.0
    pub confidence: f32,
    pub sensitivity: Sensitivity,
    pub use_policy: UsePolicy,
    pub source_client: ClientId,
    pub status: ItemStatus,
    #[serde(default)]
    pub ttl: Option<Duration>,
    /// 业务扩展属性，如差序圈层 ring、关系类型等
    #[serde(default)]
    pub attrs: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
