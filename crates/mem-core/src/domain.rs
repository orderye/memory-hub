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
///
/// 「不可变」= 写入后不可悄悄改写；写入**之前**的脱敏与最小化是允许且必需的
/// （见 [`crate::redaction`]）。`expires_at` 是保留期限：到期后由清理任务物理删除。
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
    /// 保留期限到期时刻（None = 永久保留）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
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
    /// 保留期限到期时刻（None = 永久保留）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

/// 记忆实体关系（P1 引入）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRelation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<Namespace>,
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
    /// 证据保留期限（天）；0 = 永久保留。
    ///
    /// 证据行是"写入后不可改"的，所以敏感内容必须在**写入前**脱敏
    /// （见 [`crate::redaction`]）；保留期限则是第二道防线：
    /// 到期后由清理任务物理删除，不让不可变副本无限期留存。
    pub evidence_retention_days: u32,
}

impl Default for CognitivePolicy {
    fn default() -> Self {
        Self {
            mode: CognitiveGovernanceMode::Manual,
            auto_accept_threshold: 0.95,
            evidence_retention_days: 90,
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

    if contains_sensitive_credentials(trimmed) {
        return Err(common::HubError::InvalidArgument(
            "候选记忆包含敏感凭证或密钥，严禁持久化".into(),
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<Namespace>,
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
        // Authorization & Bearer
        "bearer ",
        // OpenAI & LLM Providers
        "sk-",
        "sk_live_",
        "sk_test_",
        // GitHub Tokens (all variations)
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        // GitLab Tokens
        "glpat-",
        // Slack Tokens (all variations)
        "xoxb-",
        "xoxp-",
        "xoxa-",
        "xoxr-",
        "xoxe-",
        "xoxs-",
        // AWS Access Keys & Secrets
        "akia",
        "asia",
        "aws_secret_access_key",
        "aws_access_key_id",
        // Private Keys (all formats)
        "-----begin private key-----",
        "-----begin rsa private key-----",
        "-----begin openssh private key-----",
        "-----begin dsa private key-----",
        "-----begin ec private key-----",
        "-----begin encrypted private key-----",
        "-----begin pgp private key-----",
        // Generic Password / Secret Key assignments
        "password=",
        "password:",
        "passwd=",
        "passwd:",
        "secret_key",
        "client_secret",
        "api_key=",
        "apikey=",
        "access_token=",
    ];
    sensitive_patterns.iter().any(|p| lower.contains(p))
}

/// 校验文件相对路径安全性：
/// 1. 不得为空；
/// 2. 拒绝空字符 '\0'；
/// 3. 拒绝路径穿越 ("..")；
/// 4. 拒绝绝对路径（'/' 或 '\\' 开头）；
/// 5. 拒绝 Windows UNC 共享路径 ("\\\\", "//", "\\?\", etc.)；
/// 6. 拒绝 Windows 盘符路径（如 "C:", "D:\"）；
/// 7. 拒绝包含敏感配置文件或密钥路径。
pub fn validate_safe_relative_path(path: &str) -> common::Result<()> {
    let p = path.trim();
    if p.is_empty() {
        return Err(common::HubError::InvalidArgument("文件路径不能为空".into()));
    }

    if p.as_bytes().contains(&0) {
        return Err(common::HubError::InvalidArgument(
            "文件路径包含非法空字符".into(),
        ));
    }

    if p.contains("..") {
        return Err(common::HubError::InvalidArgument(
            "文件路径存在路径穿越 (..) 风险".into(),
        ));
    }

    if p.starts_with('/') || p.starts_with('\\') {
        return Err(common::HubError::InvalidArgument(
            "文件路径不得为绝对路径或 UNC 路径".into(),
        ));
    }

    if p.contains("//") || p.contains(r"\\") {
        return Err(common::HubError::InvalidArgument(
            "文件路径包含非法连续分隔符或 UNC 模式".into(),
        ));
    }

    // Windows 盘符如 C: / C:\ / c:/
    let bytes = p.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Err(common::HubError::InvalidArgument(
            "文件路径包含 Windows 驱动器盘符".into(),
        ));
    }

    let p_lower = p.to_lowercase();
    let forbidden_patterns = [
        ".env",
        ".git/",
        ".git\\",
        "id_rsa",
        "id_ed25519",
        "id_ecdsa",
        "id_dsa",
        ".pem",
        ".key",
        "credentials.json",
        "service-account",
        "service_account",
        "authorized_keys",
        "known_hosts",
    ];

    if forbidden_patterns
        .iter()
        .any(|pattern| p_lower.contains(pattern))
    {
        return Err(common::HubError::InvalidArgument(format!(
            "禁止将敏感配置文件或私钥路径纳入代码演进记忆: {}",
            p
        )));
    }

    Ok(())
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

/// 代码变更类型 (P4)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeChangeKind {
    Add,
    Modify,
    Delete,
    Rename,
}

impl CodeChangeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            CodeChangeKind::Add => "add",
            CodeChangeKind::Modify => "modify",
            CodeChangeKind::Delete => "delete",
            CodeChangeKind::Rename => "rename",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "add" => Some(CodeChangeKind::Add),
            "modify" => Some(CodeChangeKind::Modify),
            "delete" => Some(CodeChangeKind::Delete),
            "rename" => Some(CodeChangeKind::Rename),
            _ => None,
        }
    }
}

/// 代码演进逻辑线程 (P4)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodingThread {
    pub id: Uuid,
    pub namespace: Namespace,
    pub repository: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<Uuid>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// 新建代码演进线程请求
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewCodingThread {
    pub id: Option<Uuid>,
    pub namespace: Namespace,
    pub repository: String,
    pub branch: Option<String>,
    pub task_id: Option<Uuid>,
    pub title: String,
    pub description: Option<String>,
}

/// 代码修订快照 (P4)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodingRevision {
    pub id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<Uuid>,
    pub namespace: Namespace,
    pub repository: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_sha: Option<String>,
    pub file_path: String,
    pub change_kind: CodeChangeKind,
    pub diff_content: String,
    pub diff_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol_fingerprint: Option<String>,
    pub is_committed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    pub occurred_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_evidence_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_task_id: Option<Uuid>,
}

/// 新建代码修订请求
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewCodingRevision {
    pub id: Option<Uuid>,
    pub thread_id: Option<Uuid>,
    pub namespace: Namespace,
    pub repository: String,
    pub branch: Option<String>,
    pub commit_sha: Option<String>,
    pub parent_sha: Option<String>,
    pub file_path: String,
    pub change_kind: CodeChangeKind,
    pub diff_content: String,
    pub symbol_fingerprint: Option<String>,
    pub is_committed: bool,
    pub author: Option<String>,
    pub occurred_at: Option<DateTime<Utc>>,
    pub source_evidence_id: Option<Uuid>,
    pub source_task_id: Option<Uuid>,
}

/// 最大单条 diff 大小限制：512 KB
pub const MAX_DIFF_SIZE_BYTES: usize = 512 * 1024;

/// 校验代码修订记录安全门槛 (P4)
pub fn validate_coding_revision(revision: &NewCodingRevision) -> common::Result<()> {
    let repo = revision.repository.trim();
    if repo.is_empty() {
        return Err(common::HubError::InvalidArgument("仓库标识不能为空".into()));
    }

    validate_safe_relative_path(&revision.file_path)?;

    // Diff 大小限制
    if revision.diff_content.len() > MAX_DIFF_SIZE_BYTES {
        return Err(common::HubError::InvalidArgument(format!(
            "Diff 内容超出最大允许上限 ({} 字节 > {} 字节)",
            revision.diff_content.len(),
            MAX_DIFF_SIZE_BYTES
        )));
    }

    // 二进制文件检测 (包含 \0 字符)
    if revision.diff_content.as_bytes().contains(&0) {
        return Err(common::HubError::InvalidArgument(
            "检测到二进制文件变更，严禁纳入文本代码演进记忆".into(),
        ));
    }

    // 敏感凭证扫描
    if contains_sensitive_credentials(&revision.diff_content) {
        return Err(common::HubError::InvalidArgument(
            "Diff 内容中包含敏感凭证或密钥，严禁持久化".into(),
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

impl MemoryEntry {
    /// 判定在特定时刻是否处于有效时间区间（valid_from <= t < valid_until）
    pub fn is_valid_at(&self, t: DateTime<Utc>) -> bool {
        if let Some(from) = self.valid_from {
            if t < from {
                return false;
            }
        }
        if let Some(until) = self.valid_until {
            if t >= until {
                return false;
            }
        }
        true
    }

    /// 判定在特定时刻是否已被取代或已失效（valid_until <= now）
    pub fn is_superseded_or_expired(&self, now: DateTime<Utc>) -> bool {
        if let Some(until) = self.valid_until {
            if until <= now {
                return true;
            }
        }
        false
    }
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
    /// 提案溯源：候选关系、来源、原因码、整合模式与策略版本
    ///
    /// 只记录结构化元数据，**不记录任何原始记忆文本、令牌或向量输入**。
    /// 旧记录没有该字段时读作空对象（`{}`），保证向后兼容。
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: serde_json::Value,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_safe_relative_path_blocks_traversal_and_sensitive_patterns() {
        // 合法安全相对路径
        assert!(validate_safe_relative_path("src/main.rs").is_ok());
        assert!(validate_safe_relative_path("crates/mem-core/src/domain.rs").is_ok());
        assert!(validate_safe_relative_path("README.md").is_ok());
        assert!(validate_safe_relative_path("docs/architecture/flow.png").is_ok());

        // 空路径
        assert!(validate_safe_relative_path("").is_err());
        assert!(validate_safe_relative_path("   ").is_err());

        // 空字符注入
        assert!(validate_safe_relative_path("src/foo\0bar.rs").is_err());

        // 路径穿越 (..)
        assert!(validate_safe_relative_path("../secret.txt").is_err());
        assert!(validate_safe_relative_path("src/../../etc/passwd").is_err());
        assert!(validate_safe_relative_path("foo/../bar").is_err());
        assert!(validate_safe_relative_path(r"foo\..\bar").is_err());

        // 绝对路径
        assert!(validate_safe_relative_path("/etc/passwd").is_err());
        assert!(validate_safe_relative_path(r"\Windows\System32\cmd.exe").is_err());

        // Windows UNC 共享路径
        assert!(validate_safe_relative_path(r"\\server\share\file.txt").is_err());
        assert!(validate_safe_relative_path("//server/share/file.txt").is_err());
        assert!(validate_safe_relative_path(r"\\?\C:\foo").is_err());

        // Windows 盘符路径
        assert!(validate_safe_relative_path(r"C:\Windows\System32").is_err());
        assert!(validate_safe_relative_path("c:/users/admin").is_err());
        assert!(validate_safe_relative_path("D:file.txt").is_err());

        // 敏感配置文件与私钥路径
        assert!(validate_safe_relative_path(".env").is_err());
        assert!(validate_safe_relative_path("config/.env.local").is_err());
        assert!(validate_safe_relative_path(".git/config").is_err());
        assert!(validate_safe_relative_path(r".git\HEAD").is_err());
        assert!(validate_safe_relative_path("keys/id_rsa").is_err());
        assert!(validate_safe_relative_path("keys/id_ed25519").is_err());
        assert!(validate_safe_relative_path("certs/server.pem").is_err());
        assert!(validate_safe_relative_path("certs/private.key").is_err());
        assert!(validate_safe_relative_path("credentials.json").is_err());
        assert!(validate_safe_relative_path("auth/service-account.json").is_err());
    }

    #[test]
    fn test_contains_sensitive_credentials_detects_all_token_variants() {
        // 安全正常内容
        assert!(!contains_sensitive_credentials(
            "Normal code and documentation"
        ));
        assert!(!contains_sensitive_credentials(
            "fn process_data(input: &str) -> Result<()>"
        ));

        // Bearer 令牌
        assert!(contains_sensitive_credentials(concat!(
            "Authorization: Bearer ",
            "eyJhbGciOiJIUzI1NiIs..."
        )));

        // OpenAI / LLM API keys
        assert!(contains_sensitive_credentials("sk-1234567890abcdef"));
        assert!(contains_sensitive_credentials(concat!(
            "sk_live_",
            "1234567890"
        )));
        assert!(contains_sensitive_credentials(concat!(
            "sk_test_",
            "1234567890"
        )));

        // GitHub 令牌变体
        assert!(contains_sensitive_credentials("ghp_1234567890abcdef"));
        assert!(contains_sensitive_credentials("gho_1234567890abcdef"));
        assert!(contains_sensitive_credentials("ghu_1234567890abcdef"));
        assert!(contains_sensitive_credentials("ghs_1234567890abcdef"));
        assert!(contains_sensitive_credentials("ghr_1234567890abcdef"));
        assert!(contains_sensitive_credentials(
            "github_pat_1234567890abcdef"
        ));

        // GitLab 令牌
        assert!(contains_sensitive_credentials("glpat-abcdef1234567890"));

        // Slack 令牌变体
        assert!(contains_sensitive_credentials("xoxb-1234567890"));
        assert!(contains_sensitive_credentials("xoxp-1234567890"));
        assert!(contains_sensitive_credentials("xoxa-1234567890"));
        assert!(contains_sensitive_credentials("xoxe-1234567890"));

        // AWS 访问密钥
        // AKIA/ASIA 形态分段拼接：本仓库公开，secret scanning 会把连续字面量
        // 当成真实凭据上报（曾误报 AWS 临时密钥）。拼接后运行时取值不变。
        assert!(contains_sensitive_credentials(concat!(
            "AKIA",
            "IOSFODNN7EXAMPLE"
        )));
        assert!(contains_sensitive_credentials(concat!(
            "ASIA",
            "IOSFODNN7EXAMPLE"
        )));
        assert!(contains_sensitive_credentials(concat!(
            "aws_secret_access_key = ",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
        )));

        // 各格式私钥
        assert!(contains_sensitive_credentials(
            "-----BEGIN PRIVATE KEY-----"
        ));
        assert!(contains_sensitive_credentials(
            "-----BEGIN RSA PRIVATE KEY-----"
        ));
        assert!(contains_sensitive_credentials(
            "-----BEGIN OPENSSH PRIVATE KEY-----"
        ));
        assert!(contains_sensitive_credentials(
            "-----BEGIN EC PRIVATE KEY-----"
        ));

        // 通用密码 / 敏感键分配
        assert!(contains_sensitive_credentials("db_password=supersecret"));
        assert!(contains_sensitive_credentials("password: 'mypassword'"));
        assert!(contains_sensitive_credentials("api_key=abcdef123456"));
        assert!(contains_sensitive_credentials("client_secret = 'secret'"));
    }

    #[test]
    fn test_validate_coding_revision_enforces_safety_and_diff_limits() {
        let mut rev = NewCodingRevision {
            id: None,
            thread_id: None,
            namespace: "coding-ns".to_string(),
            repository: "memory-hub".to_string(),
            branch: Some("main".to_string()),
            commit_sha: Some("abc1234".to_string()),
            parent_sha: None,
            file_path: "crates/mem-core/src/domain.rs".to_string(),
            change_kind: CodeChangeKind::Modify,
            diff_content: "+ pub fn test() {}".to_string(),
            symbol_fingerprint: None,
            is_committed: true,
            author: Some("developer".to_string()),
            occurred_at: None,
            source_evidence_id: None,
            source_task_id: None,
        };

        // 合法 revision
        assert!(validate_coding_revision(&rev).is_ok());

        // 包含敏感凭证
        rev.diff_content = "+ let api_key = \"ghp_1234567890abcdef\";".to_string();
        assert!(validate_coding_revision(&rev).is_err());

        // 包含危险路径
        rev.diff_content = "+ pub fn clean() {}".to_string();
        rev.file_path = "../../etc/shadow".to_string();
        assert!(validate_coding_revision(&rev).is_err());
    }
}
