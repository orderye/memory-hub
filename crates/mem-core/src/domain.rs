//! 多智能体长期记忆服务核心领域类型
//!
//! 与旧版"个人记忆 + 行为规范"模型不同，本服务只做三件事：
//! **存储**（多 Agent 共享的长期记忆条目）、**备份**（命名空间快照与恢复）、
//! **整合**（跨 Agent 写入的去重与冲突归并）。

use chrono::{DateTime, Utc};
use common::AgentId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// 命名空间：记忆的逻辑隔离空间（如 `default`、`team-x`、`project-y`）
pub type Namespace = String;

/// 记忆条目分类
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// 客观事实
    Fact,
    /// 偏好设定
    Preference,
    /// 事件片段
    Episode,
    /// 自由笔记
    Note,
    /// 技能/方法
    Skill,
    /// 流程/步骤
    Procedure,
}

impl MemoryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryKind::Fact => "fact",
            MemoryKind::Preference => "preference",
            MemoryKind::Episode => "episode",
            MemoryKind::Note => "note",
            MemoryKind::Skill => "skill",
            MemoryKind::Procedure => "procedure",
        }
    }

    pub fn label_zh(&self) -> &'static str {
        match self {
            MemoryKind::Fact => "事实",
            MemoryKind::Preference => "偏好",
            MemoryKind::Episode => "事件",
            MemoryKind::Note => "笔记",
            MemoryKind::Skill => "技能",
            MemoryKind::Procedure => "流程",
        }
    }

    /// 从字符串解析（未知值返回 None）
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "fact" => Some(MemoryKind::Fact),
            "preference" => Some(MemoryKind::Preference),
            "episode" => Some(MemoryKind::Episode),
            "note" => Some(MemoryKind::Note),
            "skill" => Some(MemoryKind::Skill),
            "procedure" => Some(MemoryKind::Procedure),
            _ => None,
        }
    }

    /// 全部取值（schema 枚举用）
    pub fn all() -> &'static [MemoryKind] {
        &[
            MemoryKind::Fact,
            MemoryKind::Preference,
            MemoryKind::Episode,
            MemoryKind::Note,
            MemoryKind::Skill,
            MemoryKind::Procedure,
        ]
    }
}

/// 条目生命周期状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    /// 活跃可用
    Active,
    /// 已归档（软删除，仍参与备份）
    Archived,
    /// 已被整合合并进其它条目（保留溯源）
    Merged,
}

impl MemoryStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryStatus::Active => "active",
            MemoryStatus::Archived => "archived",
            MemoryStatus::Merged => "merged",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "active" => Some(MemoryStatus::Active),
            "archived" => Some(MemoryStatus::Archived),
            "merged" => Some(MemoryStatus::Merged),
            _ => None,
        }
    }
}

/// 记忆条目实体
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: Uuid,
    pub namespace: Namespace,
    /// 业务去重键（命名空间内唯一），提供时 put 走 upsert 语义
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub kind: MemoryKind,
    pub content: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    /// 归一化嵌入向量（可选）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,
    /// 生成该向量的模型标识
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_model: Option<String>,
    /// 写入该条目的 Agent
    pub source_agent: AgentId,
    /// 置信度 0.0 ~ 1.0
    pub confidence: f32,
    pub status: MemoryStatus,
    /// 内容版本号：每次内容修订 +1
    pub version: i64,
    /// 过期时间（到点后由 worker 归档）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    /// status = Merged 时指向幸存条目
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_into: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// 新记忆写入请求
#[derive(Debug, Clone)]
pub struct NewMemory {
    pub namespace: Namespace,
    pub key: Option<String>,
    pub kind: MemoryKind,
    pub content: String,
    pub tags: Vec<String>,
    pub metadata: serde_json::Value,
    pub confidence: f32,
    /// 生存秒数（None 表示不过期）
    pub ttl_seconds: Option<i64>,
    pub source_agent: AgentId,
}

/// 记忆修订补丁（只改显式提供的字段）
#[derive(Debug, Clone, Default)]
pub struct MemoryPatch {
    pub content: Option<String>,
    pub kind: Option<MemoryKind>,
    pub tags: Option<Vec<String>>,
    pub metadata: Option<serde_json::Value>,
    pub confidence: Option<f32>,
    /// 设置过期：`Some(n)` = n 秒后过期；`Some(0)` = 清除过期；`None` = 不变
    pub expires_in_seconds: Option<i64>,
}

/// 记忆版本历史（每次内容修订追加一条，支撑备份与审计）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryVersion {
    pub memory_id: Uuid,
    pub version: i64,
    pub content: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    pub confidence: f32,
    pub changed_by: AgentId,
    pub change_type: VersionChange,
    pub changed_at: DateTime<Utc>,
}

/// 版本变更类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionChange {
    Created,
    Updated,
    Archived,
    Merged,
}

/// 列表过滤条件
#[derive(Debug, Clone, Default)]
pub struct MemoryFilter {
    pub namespace: Option<Namespace>,
    pub kinds: Option<Vec<MemoryKind>>,
    pub statuses: Option<Vec<MemoryStatus>>,
    pub tags: Option<Vec<String>>,
    pub source_agent: Option<AgentId>,
    pub key: Option<String>,
    /// 内容/标签包含子串（不区分大小写）
    pub text_contains: Option<String>,
    /// 游标分页：updated_at + id 复合键之前
    pub before: Option<(DateTime<Utc>, Uuid)>,
    pub limit: Option<usize>,
}

/// 分页结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// 下一页游标（无更多数据时为 None）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

impl<T> Page<T> {
    pub fn empty() -> Self {
        Self {
            items: Vec::new(),
            next_cursor: None,
        }
    }
}

/// 命名空间注册信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamespaceInfo {
    pub namespace: Namespace,
    pub owner_agent: AgentId,
    pub created_at: DateTime<Utc>,
}

/// 命名空间授权记录（GrantStore 的持久化形态）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamespaceGrantRecord {
    pub namespace: Namespace,
    pub agent_id: AgentId,
    pub can_read: bool,
    pub can_write: bool,
    pub granted_by: AgentId,
    pub created_at: DateTime<Utc>,
}

// ===== 备份 =====

/// 备份快照记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupRecord {
    pub id: Uuid,
    /// 目标命名空间；`*` 表示全量快照
    pub namespace: Namespace,
    pub created_by: AgentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub entry_count: usize,
    /// 快照内容的 SHA-256（hex），用于完整性校验
    pub checksum: String,
    /// 完整快照负载（entries 数组）
    pub payload: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retain_until: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// 备份摘要（列表不下发完整负载）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupSummary {
    pub id: Uuid,
    pub namespace: Namespace,
    pub created_by: AgentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub entry_count: usize,
    pub checksum: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retain_until: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl From<&BackupRecord> for BackupSummary {
    fn from(b: &BackupRecord) -> Self {
        Self {
            id: b.id,
            namespace: b.namespace.clone(),
            created_by: b.created_by.clone(),
            note: b.note.clone(),
            entry_count: b.entry_count,
            checksum: b.checksum.clone(),
            retain_until: b.retain_until,
            created_at: b.created_at,
        }
    }
}

/// 恢复模式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreMode {
    /// 只预演：计算差异不落库
    DryRun,
    /// 合并：快照中更新/更全的条目覆盖当前（默认）
    Merge,
    /// 替换：先备份当前状态，再按快照重建命名空间
    Replace,
}

impl RestoreMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "dry_run" => Some(RestoreMode::DryRun),
            "merge" => Some(RestoreMode::Merge),
            "replace" => Some(RestoreMode::Replace),
            _ => None,
        }
    }
}

/// 恢复报告
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreReport {
    pub backup_id: Uuid,
    pub mode: RestoreMode,
    pub total: usize,
    pub created: usize,
    pub updated: usize,
    pub unchanged: usize,
    /// Replace 模式下自动创建的安全备份 ID
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safety_backup_id: Option<Uuid>,
}

// ===== 整合 =====

/// 整合提案类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationKind {
    /// 内容高度相似的重复条目
    Duplicate,
    /// 同一 key 但内容冲突
    Conflict,
}

impl IntegrationKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            IntegrationKind::Duplicate => "duplicate",
            IntegrationKind::Conflict => "conflict",
        }
    }

    pub fn label_zh(&self) -> &'static str {
        match self {
            IntegrationKind::Duplicate => "重复",
            IntegrationKind::Conflict => "冲突",
        }
    }
}

/// 提案状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    Open,
    Merged,
    Dismissed,
    Expired,
}

/// 整合提案
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrationProposal {
    pub id: Uuid,
    pub namespace: Namespace,
    pub kind: IntegrationKind,
    /// 参与合并的条目 ID 列表
    pub member_ids: Vec<Uuid>,
    /// 建议的幸存条目（通常为最新更新者）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_survivor: Option<Uuid>,
    /// 成员间的最高相似度
    pub similarity: f32,
    pub status: ProposalStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_note: Option<String>,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<DateTime<Utc>>,
}

/// 检索结果（带打分）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scored<T> {
    pub item: T,
    pub score: f32,
}

/// 记忆库统计
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryStats {
    pub namespace: Namespace,
    pub total: usize,
    pub active: usize,
    pub archived: usize,
    pub merged: usize,
    pub by_kind: HashMap<String, usize>,
    pub by_agent: HashMap<String, usize>,
    pub expiring_24h: usize,
}
