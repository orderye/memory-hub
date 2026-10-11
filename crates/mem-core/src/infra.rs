//! 核心基础设施 trait：存储、嵌入、时钟、检索与令牌内省
//!
//! core crate 只依赖这些抽象，不直接依赖任何具体存储实现；
//! `store` crate 提供 PostgreSQL 与内存两套实现，可互换。

use crate::domain::{
    BackupRecord, IntegrationProposal, MemoryEntry, MemoryFilter, MemoryKind, MemoryVersion,
    Namespace, NamespaceGrantRecord, NamespaceInfo, ProposalStatus, Scored,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use common::{AgentId, AuthCtx, Result};
use serde::{Deserialize, Serialize};

/// 记忆存储：条目 CRUD、查询与向量近邻
#[async_trait]
pub trait MemoryStore: Send + Sync {
    /// 插入新条目（id 由调用方分配）
    async fn insert(&self, entry: &MemoryEntry) -> Result<MemoryEntry>;

    /// 按 ID 读取（含任意状态）
    async fn get(&self, namespace: &str, id: uuid::Uuid) -> Result<MemoryEntry>;

    /// 按业务键读取
    async fn get_by_key(&self, namespace: &str, key: &str) -> Result<Option<MemoryEntry>>;

    /// 全量写回（service 层负责版本号与时间戳推进）
    async fn save(&self, entry: &MemoryEntry) -> Result<MemoryEntry>;

    /// 物理删除
    async fn delete(&self, namespace: &str, id: uuid::Uuid) -> Result<()>;

    /// 条件列表（按 updated_at desc + id desc 排序）
    async fn list(&self, filter: &MemoryFilter) -> Result<Vec<MemoryEntry>>;

    /// 条件计数（不含分页游标与 limit）
    async fn count(&self, filter: &MemoryFilter) -> Result<usize>;

    /// 向量近邻（仅在 active 状态内召回）
    async fn similar(
        &self,
        namespace: &str,
        embedding: &[f32],
        k: usize,
        kinds: Option<&[MemoryKind]>,
    ) -> Result<Vec<Scored<MemoryEntry>>>;

    /// 命名空间下全部条目（备份与整合扫描用；merged 可排除）
    async fn all_in_namespace(
        &self,
        namespace: &str,
        include_merged: bool,
    ) -> Result<Vec<MemoryEntry>>;

    /// 全部命名空间清单（定时全量备份用）
    async fn list_namespaces(&self) -> Result<Vec<Namespace>>;

    /// 追加一条版本历史
    async fn record_version(&self, v: &MemoryVersion) -> Result<()>;

    /// 查询版本历史（按版本号升序）
    async fn versions(&self, id: uuid::Uuid) -> Result<Vec<MemoryVersion>>;

    /// 批量归档过期条目（worker 用），返回归档条数
    async fn archive_expired(&self, now: DateTime<Utc>) -> Result<usize>;
}

/// 备份存储：快照持久化
///
/// 方法名带 `backup` 后缀以避免与 `MemoryStore` 的同名方法产生歧义。
#[async_trait]
pub trait BackupStore: Send + Sync {
    async fn save_backup(&self, backup: &BackupRecord) -> Result<()>;
    async fn get_backup(&self, id: uuid::Uuid) -> Result<BackupRecord>;
    async fn list_backups(&self, namespace: Option<&str>) -> Result<Vec<BackupRecord>>;
    async fn delete_backup(&self, id: uuid::Uuid) -> Result<()>;
}

/// 整合提案存储
#[async_trait]
pub trait IntegrationStore: Send + Sync {
    async fn save_proposal(&self, p: &IntegrationProposal) -> Result<()>;
    async fn get_proposal(&self, id: uuid::Uuid) -> Result<IntegrationProposal>;
    async fn list_proposals(
        &self,
        namespace: &str,
        status: Option<ProposalStatus>,
    ) -> Result<Vec<IntegrationProposal>>;
    /// 是否已存在成员集合相同的 open 提案（防止重复扫描刷屏）
    async fn open_exists(
        &self,
        namespace: &str,
        kind_member: &[(String, Vec<uuid::Uuid>)],
    ) -> Result<bool>;
}

/// 命名空间与授权存储（auth-server 与 mcp-server 共享同一份真实数据）
#[async_trait]
pub trait GrantStore: Send + Sync {
    /// 注册命名空间（幂等）；返回 `(信息, 是否为本次新建)`
    async fn ensure_namespace(
        &self,
        namespace: &str,
        owner: &AgentId,
    ) -> Result<(NamespaceInfo, bool)>;
    async fn get_namespace(&self, namespace: &str) -> Result<Option<NamespaceInfo>>;
    /// 全部命名空间注册信息
    async fn list_namespace_infos(&self) -> Result<Vec<NamespaceInfo>>;
    /// 授予/更新授权（幂等 upsert）
    async fn grant(&self, record: &NamespaceGrantRecord) -> Result<()>;
    async fn revoke(&self, namespace: &str, agent_id: &AgentId) -> Result<()>;
    /// 某 Agent 名下的全部授权
    async fn grants_for(&self, agent_id: &AgentId) -> Result<Vec<NamespaceGrantRecord>>;
    async fn can_read(&self, namespace: &str, agent_id: &AgentId) -> Result<bool>;
    async fn can_write(&self, namespace: &str, agent_id: &AgentId) -> Result<bool>;
    /// 某 Agent 拥有的命名空间（owner 判定）
    async fn owned_namespaces(&self, agent_id: &AgentId) -> Result<Vec<Namespace>>;
}

/// 存储聚合：一套实现同时满足五类存储需求
pub trait MemoryHubStore:
    MemoryStore + BackupStore + IntegrationStore + GrantStore + AgentStore
{
}

impl<T: MemoryStore + BackupStore + IntegrationStore + GrantStore + AgentStore> MemoryHubStore
    for T
{
}

/// Agent 注册请求（授权服务器的 DCR 入口）
#[derive(Debug, Clone)]
pub struct AgentRegistration {
    pub agent_id: AgentId,
    pub name: String,
    /// service | assistant | worker
    pub kind: String,
    /// 机密客户端密钥（公共客户端为 None，只能走 PKCE 授权码）
    pub client_secret: Option<String>,
    pub redirect_uris: Vec<String>,
    pub max_scopes: Vec<common::Scope>,
}

/// Agent 注册记录（不含密钥本体）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRecord {
    pub agent_id: AgentId,
    pub name: String,
    pub kind: String,
    /// active | suspended
    pub status: String,
    pub redirect_uris: Vec<String>,
    pub max_scopes: Vec<common::Scope>,
    /// 是否为机密客户端（持有密钥）
    pub confidential: bool,
    pub created_at: DateTime<Utc>,
}

/// Agent 存储：注册、查询与密钥校验
#[async_trait]
pub trait AgentStore: Send + Sync {
    /// 注册（按 agent_id upsert）；返回最终记录
    async fn register_agent(&self, reg: &AgentRegistration) -> Result<AgentRecord>;
    async fn get_agent(&self, agent_id: &AgentId) -> Result<Option<AgentRecord>>;
    async fn list_agents(&self) -> Result<Vec<AgentRecord>>;
    async fn set_agent_status(&self, agent_id: &AgentId, status: &str) -> Result<()>;
    /// 校验机密客户端密钥（公共客户端恒 false）
    async fn verify_agent_secret(&self, agent_id: &AgentId, secret: &str) -> Result<bool>;
}

/// 嵌入器：文本 → 归一化向量
#[async_trait]
pub trait Embedder: Send + Sync {
    fn model_id(&self) -> &str;
    fn dim(&self) -> usize;
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

/// 时钟（测试可注入固定时间）
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// 系统时钟默认实现
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// 检索器：由 retrieval crate 实现（混合检索），core 只依赖抽象
#[async_trait]
pub trait Retriever: Send + Sync {
    async fn search(
        &self,
        auth: &AuthCtx,
        q: &crate::algorithm::SearchQuery,
    ) -> Result<Vec<Scored<MemoryEntry>>>;
}

/// 令牌内省：Bearer 令牌 → AuthCtx（auth-core 实现）
#[async_trait]
pub trait TokenIntrospector: Send + Sync {
    async fn introspect(&self, token: &str) -> Result<AuthCtx>;
}

/// 访问判定：命名空间级读/写/管理权（基于 GrantStore）
#[async_trait]
pub trait AccessChecker: Send + Sync {
    async fn can_read(&self, agent: &AgentId, namespace: &str) -> Result<bool>;
    async fn can_write(&self, agent: &AgentId, namespace: &str) -> Result<bool>;
    /// 是否为命名空间 owner（或持有通配授权）
    async fn is_owner(&self, agent: &AgentId, namespace: &str) -> Result<bool>;
    /// 首次写入时自动注册命名空间并建立 owner 授权
    async fn ensure_namespace(&self, namespace: &str, owner: &AgentId) -> Result<()>;
}
