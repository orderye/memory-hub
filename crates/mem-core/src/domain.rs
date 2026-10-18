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

/// 场景管线（P1 引入）
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScene {
    /// 对话场景：事实/摘要/偏好/意图
    Chat,
    /// 工作任务场景：任务上下文与提炼经验
    Working,
    /// 编程场景：代码演进、diff 链与重构
    Coding,
}

impl MemoryScene {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryScene::Chat => "chat",
            MemoryScene::Working => "working",
            MemoryScene::Coding => "coding",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "chat" => Some(MemoryScene::Chat),
            "working" => Some(MemoryScene::Working),
            "coding" => Some(MemoryScene::Coding),
            _ => None,
        }
    }
}

/// 认知分层（P1 引入）
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CognitiveLayer {
    /// 原始输入/存根
    Raw,
    /// 原子事实
    Fact,
    /// 事件/会话摘要
    Summary,
    /// 实体/角色画像
    Identity,
    /// 领域知识
    Knowledge,
    /// 稳定模式/认知图式
    Schema,
    /// 前瞻意图/待办动机
    Intention,
}

impl CognitiveLayer {
    pub fn as_str(&self) -> &'static str {
        match self {
            CognitiveLayer::Raw => "raw",
            CognitiveLayer::Fact => "fact",
            CognitiveLayer::Summary => "summary",
            CognitiveLayer::Identity => "identity",
            CognitiveLayer::Knowledge => "knowledge",
            CognitiveLayer::Schema => "schema",
            CognitiveLayer::Intention => "intention",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "raw" => Some(CognitiveLayer::Raw),
            "fact" => Some(CognitiveLayer::Fact),
            "summary" => Some(CognitiveLayer::Summary),
            "identity" => Some(CognitiveLayer::Identity),
            "knowledge" => Some(CognitiveLayer::Knowledge),
            "schema" => Some(CognitiveLayer::Schema),
            "intention" => Some(CognitiveLayer::Intention),
            _ => None,
        }
    }
}

/// 关系网关系类型（P1 引入）
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryRelationKind {
    /// 支撑关系
    Supports,
    /// 矛盾/冲突关系
    Contradicts,
    /// 取代/版本演进（新事实取代旧事实）
    Supersedes,
    /// 衍生自
    DerivedFrom,
    /// 一般相关
    RelatedTo,
    /// 任务提炼出的可复用经验
    DerivedExperience,
    /// 代码修订演进关系
    CodeRevision,
}

impl MemoryRelationKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryRelationKind::Supports => "supports",
            MemoryRelationKind::Contradicts => "contradicts",
            MemoryRelationKind::Supersedes => "supersedes",
            MemoryRelationKind::DerivedFrom => "derived_from",
            MemoryRelationKind::RelatedTo => "related_to",
            MemoryRelationKind::DerivedExperience => "derived_experience",
            MemoryRelationKind::CodeRevision => "code_revision",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "supports" => Some(MemoryRelationKind::Supports),
            "contradicts" => Some(MemoryRelationKind::Contradicts),
            "supersedes" => Some(MemoryRelationKind::Supersedes),
            "derived_from" => Some(MemoryRelationKind::DerivedFrom),
            "related_to" => Some(MemoryRelationKind::RelatedTo),
            "derived_experience" => Some(MemoryRelationKind::DerivedExperience),
            "code_revision" => Some(MemoryRelationKind::CodeRevision),
            _ => None,
        }
    }
}

/// 证据来源类别（P1 引入）
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSourceKind {
    Session,
    Task,
    ToolCall,
    GitCommit,
    Manual,
}

impl EvidenceSourceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EvidenceSourceKind::Session => "session",
            EvidenceSourceKind::Task => "task",
            EvidenceSourceKind::ToolCall => "tool_call",
            EvidenceSourceKind::GitCommit => "git_commit",
            EvidenceSourceKind::Manual => "manual",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "session" => Some(EvidenceSourceKind::Session),
            "task" => Some(EvidenceSourceKind::Task),
            "tool_call" => Some(EvidenceSourceKind::ToolCall),
            "git_commit" => Some(EvidenceSourceKind::GitCommit),
            "manual" => Some(EvidenceSourceKind::Manual),
            _ => None,
        }
    }
}

/// 认知提案状态（与旧版 Integration ProposalStatus 区分）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CognitiveProposalStatus {
    Candidate,
    PendingReview,
    Accepted,
    Rejected,
    Superseded,
}

/// 原始证据存根（P1 引入，Append-only）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEvidence {
    pub id: Uuid,
    pub namespace: Namespace,
    pub source_kind: EvidenceSourceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    pub content_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// 新证据存根追加请求
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewEvidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    pub namespace: Namespace,
    pub source_kind: EvidenceSourceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    pub content_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<DateTime<Utc>>,
}

/// 记忆实体关系（P1 引入）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRelation {
    pub from_memory_id: Uuid,
    pub to_memory_id: Uuid,
    pub relation_kind: MemoryRelationKind,
    pub confidence: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_evidence_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// 新记忆关系添加请求
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewMemoryRelation {
    pub from_memory_id: Uuid,
    pub to_memory_id: Uuid,
    pub relation_kind: MemoryRelationKind,
    #[serde(default = "default_relation_confidence")]
    pub confidence: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_evidence_id: Option<Uuid>,
}

fn default_relation_confidence() -> f32 {
    1.0
}

/// 消息角色（Chat 认知流）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatRole {
    User,
    Assistant,
    System,
    Tool,
}

impl ChatRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChatRole::User => "user",
            ChatRole::Assistant => "assistant",
            ChatRole::System => "system",
            ChatRole::Tool => "tool",
        }
    }
}

/// 对话消息项
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
}

/// Chat 认知提取请求输入
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatCognitiveInput {
    pub namespace: Namespace,
    pub session_id: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<DateTime<Utc>>,
}

/// 从对话中提炼出的记忆候选
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CognitiveCandidate {
    pub content: String,
    pub scene: MemoryScene,
    pub layer: CognitiveLayer,
    pub confidence: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_from: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_until: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    /// 认知关系类别（如取代旧记忆时填 Supersedes）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relation_kind: Option<MemoryRelationKind>,
    /// 关联或演化目标记忆 ID
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_memory_id: Option<Uuid>,
}

/// 认知提案记录（供人工审查或审计状态流转）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CognitiveProposalRecord {
    pub id: Uuid,
    pub namespace: Namespace,
    pub candidate: CognitiveCandidate,
    pub source_evidence_id: Uuid,
    pub status: CognitiveProposalStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejection_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resulting_memory_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// 认知提炼治理模式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CognitiveGovernanceMode {
    /// 全手动：生成提案待审，绝不自动入库
    #[default]
    Manual,
    /// 半自动：高置信度且无冲突自动入库，其余生成提案
    SemiAuto,
    /// 全自动：有效候选自动入库
    FullAuto,
}

/// 认知治理策略
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CognitivePolicy {
    pub mode: CognitiveGovernanceMode,
    pub auto_accept_threshold: f32,
}

impl Default for CognitivePolicy {
    fn default() -> Self {
        Self {
            mode: CognitiveGovernanceMode::Manual,
            auto_accept_threshold: 0.95,
        }
    }
}

/// 校验候选条目合法性
pub fn validate_cognitive_candidate(candidate: &CognitiveCandidate) -> common::Result<()> {
    let trimmed = candidate.content.trim();
    if trimmed.is_empty() {
        return Err(common::HubError::InvalidArgument(
            "候选记忆内容不能为空".into(),
        ));
    }
    if trimmed.chars().count() > 8192 {
        return Err(common::HubError::InvalidArgument(
            "候选记忆内容超过 8192 字符上限".into(),
        ));
    }
    if candidate.confidence < 0.0 || candidate.confidence > 1.0 {
        return Err(common::HubError::InvalidArgument(
            "置信度必须在 0.0 ~ 1.0 之间".into(),
        ));
    }
    if candidate.tags.len() > 32 {
        return Err(common::HubError::InvalidArgument(
            "标签数不能超过 32".into(),
        ));
    }

    // 意图规则：含糊猜测表述不可作为确定意图
    if candidate.layer == CognitiveLayer::Intention && is_speculative_intention(trimmed) {
        return Err(common::HubError::InvalidArgument(
            "含糊猜测或未经确认的表述不能提取为确定意图".into(),
        ));
    }

    Ok(())
}

/// 检查是否为含糊猜测表述（不能作为确定前瞻意图）
pub fn is_speculative_intention(text: &str) -> bool {
    let lower = text.to_lowercase();
    let speculative_markers = [
        "可能会考虑",
        "可能考虑",
        "或许会",
        "也许会",
        "还没想好",
        "待定",
        "说不定",
        "maybe consider",
        "might consider",
        "possibly",
        "tentatively",
        "not decided",
    ];
    speculative_markers.iter().any(|m| lower.contains(m))
}

/// 检查意图是否在指定时刻处于有效期内
pub fn is_intention_active(
    layer: CognitiveLayer,
    valid_until: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> bool {
    if layer != CognitiveLayer::Intention {
        return true;
    }
    match valid_until {
        Some(vu) => vu > now,
        None => true,
    }
}

/// 任务状态（Working 场景）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Running => "running",
            TaskStatus::Completed => "completed",
            TaskStatus::Failed => "failed",
            TaskStatus::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "running" => Some(TaskStatus::Running),
            "completed" => Some(TaskStatus::Completed),
            "failed" => Some(TaskStatus::Failed),
            "cancelled" => Some(TaskStatus::Cancelled),
            _ => None,
        }
    }
}

/// 任务事实（Working 场景）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryTask {
    pub id: Uuid,
    pub namespace: Namespace,
    pub scene: MemoryScene,
    pub task: String,
    pub status: TaskStatus,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_evidence_id: Option<Uuid>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

/// 新建任务事实请求
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewMemoryTask {
    pub id: Option<Uuid>,
    pub namespace: Namespace,
    pub scene: MemoryScene,
    pub task: String,
    pub status: TaskStatus,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_evidence_id: Option<Uuid>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

/// 经验条目生命周期状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperienceStatus {
    Candidate,
    Active,
    Deprecated,
    Archived,
}

impl ExperienceStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExperienceStatus::Candidate => "candidate",
            ExperienceStatus::Active => "active",
            ExperienceStatus::Deprecated => "deprecated",
            ExperienceStatus::Archived => "archived",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "candidate" => Some(ExperienceStatus::Candidate),
            "active" => Some(ExperienceStatus::Active),
            "deprecated" => Some(ExperienceStatus::Deprecated),
            "archived" => Some(ExperienceStatus::Archived),
            _ => None,
        }
    }
}

/// 经验记忆实体
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryExperience {
    pub id: Uuid,
    pub namespace: Namespace,
    pub task: String,
    #[serde(default)]
    pub search_keys: Vec<String>,
    pub solution: String,
    #[serde(default)]
    pub preconditions: serde_json::Value,
    #[serde(default)]
    pub boundary_envs: Vec<String>,
    #[serde(default)]
    pub failure_modes: Vec<String>,
    #[serde(default)]
    pub validation: serde_json::Value,
    pub confidence: f32,
    pub status: ExperienceStatus,
    #[serde(default = "default_feedback_score")]
    pub feedback_score: f32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

fn default_feedback_score() -> f32 {
    1.0
}

/// 新建经验记忆请求
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewMemoryExperience {
    pub id: Option<Uuid>,
    pub namespace: Namespace,
    pub task: String,
    pub search_keys: Vec<String>,
    pub solution: String,
    pub preconditions: serde_json::Value,
    pub boundary_envs: Vec<String>,
    pub failure_modes: Vec<String>,
    pub validation: serde_json::Value,
    pub confidence: f32,
    pub status: ExperienceStatus,
    pub feedback_score: f32,
}

/// 任务与经验关联映射
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryTaskExperience {
    pub task_id: Uuid,
    pub experience_id: Uuid,
    pub relation_kind: String,
    pub created_at: DateTime<Utc>,
}

/// 经验提炼候选结构
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperienceCandidate {
    pub task_pattern: String,
    pub search_keys: Vec<String>,
    pub solution: String,
    pub preconditions: serde_json::Value,
    pub boundary_envs: Vec<String>,
    pub failure_modes: Vec<String>,
    pub validation: serde_json::Value,
    pub confidence: f32,
    pub source_task_id: Uuid,
}

/// 扫描敏感凭证（API Token、私钥、密码模式）
pub fn contains_sensitive_credentials(text: &str) -> bool {
    let lower = text.to_lowercase();
    let sensitive_patterns = [
        "bearer ",
        "sk-",
        "ghp_",
        "gho_",
        "xoxb-",
        "xoxp-",
        "akia",
        "-----begin private key-----",
        "-----begin rsa private key-----",
        "-----begin openssh private key-----",
        "password=",
        "secret_key",
        "client_secret",
    ];
    sensitive_patterns.iter().any(|p| lower.contains(p))
}

/// 验证经验候选质量门槛
pub fn validate_experience_candidate(candidate: &ExperienceCandidate) -> common::Result<()> {
    if contains_sensitive_credentials(&candidate.solution)
        || contains_sensitive_credentials(&candidate.task_pattern)
        || contains_sensitive_credentials(&candidate.preconditions.to_string())
    {
        return Err(common::HubError::InvalidArgument(
            "经验候选包含敏感凭证或密钥，严禁持久化".into(),
        ));
    }

    let pattern = candidate.task_pattern.trim();
    if pattern.is_empty() {
        return Err(common::HubError::InvalidArgument("任务模式不能为空".into()));
    }
    let local_only_markers = [
        "本次任务",
        "刚才我",
        "今天我完成了",
        "刚刚执行了",
        "this specific run",
        "just completed task",
    ];
    if local_only_markers.iter().any(|m| pattern.contains(m)) {
        return Err(common::HubError::InvalidArgument(
            "经验必须具备通用可复用性，不得为单次任务的纯局部描述".into(),
        ));
    }

    let solution = candidate.solution.trim();
    if solution.is_empty() {
        return Err(common::HubError::InvalidArgument("解决方案不能为空".into()));
    }

    if candidate.confidence < 0.5 {
        return Err(common::HubError::InvalidArgument(
            "经验置信度未达到 0.5 最低有效门槛".into(),
        ));
    }

    let val_str = candidate.validation.to_string();
    if val_str == "{}" || val_str == "null" || val_str.trim().is_empty() {
        return Err(common::HubError::InvalidArgument(
            "经验必须包含可验证结果或验证方法说明".into(),
        ));
    }

    if candidate.boundary_envs.is_empty()
        && (candidate.preconditions.is_null() || candidate.preconditions == serde_json::json!({}))
    {
        return Err(common::HubError::InvalidArgument(
            "经验必须标明适用环境或前置条件".into(),
        ));
    }

    Ok(())
}

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
    /// 场景管线（Chat / Working / Coding）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scene: Option<MemoryScene>,
    /// 认知分层（Raw / Fact / Summary / Identity / Knowledge / Schema / Intention）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<CognitiveLayer>,
    /// 事实生效时间
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_from: Option<DateTime<Utc>>,
    /// 事实失效时间
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_until: Option<DateTime<Utc>>,
    /// 客观事件发生时间
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<DateTime<Utc>>,
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
    pub scene: Option<MemoryScene>,
    pub layer: Option<CognitiveLayer>,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_until: Option<DateTime<Utc>>,
    pub occurred_at: Option<DateTime<Utc>>,
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
    pub scene: Option<MemoryScene>,
    pub layer: Option<CognitiveLayer>,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_until: Option<DateTime<Utc>>,
    pub occurred_at: Option<DateTime<Utc>>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scene: Option<MemoryScene>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<CognitiveLayer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_from: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_until: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<DateTime<Utc>>,
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
    pub scene: Option<MemoryScene>,
    pub layer: Option<CognitiveLayer>,
    /// 查询在特定时刻有效的事实（valid_from <= t < valid_until）
    pub valid_at: Option<DateTime<Utc>>,
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
