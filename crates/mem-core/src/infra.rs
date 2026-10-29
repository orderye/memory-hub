//! 核心基础设施 trait：存储、嵌入、时钟、检索与令牌内省
//!
//! core crate 只依赖这些抽象，不直接依赖任何具体存储实现；
//! `store` crate 提供 PostgreSQL 与内存两套实现，可互换。

use crate::domain::{
    BackupRecord, ChatCognitiveInput, CodingRevision, CodingThread, CognitiveCandidate,
    CognitiveProposalRecord, CognitiveProposalStatus, CollectionBinding, EvidenceSourceKind,
    ExperienceStatus, ExperienceVersion, IntegrationProposal, MemoryCollection, MemoryEntry,
    MemoryEvidence, MemoryExperience, MemoryFilter, MemoryRelation, MemoryRelationKind, MemoryTask,
    MemoryTaskExperience, MemoryVersion, Namespace, NamespaceGrantRecord, NamespaceInfo,
    NewCodingRevision, NewCodingThread, NewEvidence, NewExperienceVersion, NewMemoryExperience,
    NewMemoryRelation, NewMemoryTask, ProposalStatus, Scored, TaskStatus,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use common::{AccountId, AgentId, AuthCtx, Result};
use serde::{Deserialize, Serialize};

/// 记忆存储：条目 CRUD、查询与向量近邻
#[async_trait]
pub trait MemoryStore: Send + Sync {
    /// 插入新条目（id 由调用方分配）
    async fn insert(&self, entry: &MemoryEntry) -> Result<MemoryEntry>;

    /// 原子插入新条目并追加初始版本历史（VersionChange::Created）。
    ///
    /// 插入条目与写入初始版本历史必须同成同败，杜绝"有条目无创建历史"的脏数据分叉。
    /// 默认实现按序执行两个既有操作，生产存储与内存存储都应覆盖为原子实现。
    async fn insert_initial(&self, entry: &MemoryEntry) -> Result<MemoryEntry> {
        let saved = self.insert(entry).await?;
        self.record_version(&crate::domain::MemoryVersion {
            memory_id: saved.id,
            version: saved.version,
            content: saved.content.clone(),
            tags: saved.tags.clone(),
            metadata: saved.metadata.clone(),
            confidence: saved.confidence,
            changed_by: saved.source_agent.clone(),
            change_type: crate::domain::VersionChange::Created,
            changed_at: saved.created_at,
            scene: saved.scene,
            layer: saved.layer,
            valid_from: saved.valid_from,
            valid_until: saved.valid_until,
            occurred_at: saved.occurred_at,
        })
        .await?;
        Ok(saved)
    }

    /// 按 ID 读取（含任意状态）
    async fn get(&self, namespace: &str, id: uuid::Uuid) -> Result<MemoryEntry>;

    /// 按业务键读取
    async fn get_by_key(&self, namespace: &str, key: &str) -> Result<Option<MemoryEntry>>;

    /// 全量写回已存在条目（service 层负责版本号与时间戳推进）。
    ///
    /// 契约：**要求条目已存在**——`(namespace, id)` 不存在时返回 `NotFound`，
    /// 不做 upsert。新建条目必须走 `insert` / `insert_initial`。
    /// 两个后端（PostgreSQL / 内存）在这一点上语义一致。
    async fn save(&self, entry: &MemoryEntry) -> Result<MemoryEntry>;

    /// 乐观并发修订：仅当库中该行当前版本为 `expected_version` 时才写入。
    ///
    /// `save` 是无条件 `update ... where namespace/id`，两个 Agent 同时读到同一条目
    /// 再各自写回时，后写者会静默覆盖前者的修订与版本推进（丢更新）。
    /// 所有"读-改-写"路径都应改用本方法。
    ///
    /// 契约：乐观锁前置条件不成立（条目不存在，或当前版本 ≠
    /// `expected_version`）一律返回 `Conflict`，绝不静默覆盖。
    ///
    /// 默认实现退化为无条件 `save`，供无需该保证的测试替身使用；
    /// 生产存储与内存存储都必须覆盖。
    async fn save_if_version(
        &self,
        entry: &MemoryEntry,
        expected_version: i64,
    ) -> Result<MemoryEntry> {
        let _ = expected_version;
        self.save(entry).await
    }

    /// 原子修订：写回条目（乐观锁）并追加版本历史，二者必须同成同败。
    ///
    /// 契约：与 `save_if_version` 同一乐观锁语义——版本不符或条目不存在
    /// 返回 `Conflict`，且不追加版本历史。
    ///
    /// `save` + `record_version` 分两步时，第二步失败会留下"条目已推进而历史缺那一版"
    /// 的分叉——而版本历史正是备份与审计的事实来源。`changed_by` 取自
    /// `entry.source_agent`（各服务在写前都已把作者设为操作者，与原先逐点传入等价）。
    ///
    /// 默认实现按序调用两个既有方法，供无此需求的测试替身使用；
    /// 生产存储与内存存储都必须覆盖为真正原子的实现。
    async fn revise(
        &self,
        entry: &MemoryEntry,
        expected_version: i64,
        change: crate::domain::VersionChange,
    ) -> Result<MemoryEntry> {
        let saved = self.save_if_version(entry, expected_version).await?;
        self.record_version(&crate::domain::MemoryVersion {
            memory_id: saved.id,
            version: saved.version,
            content: saved.content.clone(),
            tags: saved.tags.clone(),
            metadata: saved.metadata.clone(),
            confidence: saved.confidence,
            changed_by: saved.source_agent.clone(),
            change_type: change,
            changed_at: saved.updated_at,
            scene: saved.scene,
            layer: saved.layer,
            valid_from: saved.valid_from,
            valid_until: saved.valid_until,
            occurred_at: saved.occurred_at,
        })
        .await?;
        Ok(saved)
    }

    /// 物理删除
    async fn delete(&self, namespace: &str, id: uuid::Uuid) -> Result<()>;

    /// 条件列表（默认按 updated_at desc + id desc 排序；
    /// 带 `text_contains` 时按词法相关度排序，保证大候选池不漏掉旧而更相关的文档）
    async fn list(&self, filter: &MemoryFilter) -> Result<Vec<MemoryEntry>>;

    /// 条件计数（不含分页游标与 limit）
    async fn count(&self, filter: &MemoryFilter) -> Result<usize>;

    /// 向量近邻召回：过滤条件（命名空间/状态/场景/层级/时间有效性/标签/类型）
    /// 必须下推到存储层执行，只返回满足条件的最近邻；排序为相似度降序。
    ///
    /// 返回的条目**不携带 embedding**（瘦列契约）：ANN 只需要距离排序，
    /// 整包回传 1024 维向量会让每次检索白拉 ~12KB/行。需要真实向量做
    /// 两两相似度（MMR）的调用方，对入围候选用 [`Self::embeddings_by_ids`]
    /// 二次取回。
    async fn similar(
        &self,
        filter: &MemoryFilter,
        embedding: &[f32],
        k: usize,
    ) -> Result<Vec<Scored<MemoryEntry>>>;

    /// 批量取回条目的嵌入向量（MMR 重排的专用二次取回）。
    ///
    /// 只返回 `namespace` 内、`ids` 中实际带向量的条目 `(id, 向量)`，
    /// 其余（无向量 / 不在命名空间 / 已被并发删除）缺席。
    /// 命名空间入参是硬隔离要求：实现不得凭 id 跨命名空间取数。
    async fn embeddings_by_ids(
        &self,
        namespace: &str,
        ids: &[uuid::Uuid],
    ) -> Result<Vec<(uuid::Uuid, Vec<f32>)>>;

    /// 命名空间下全部条目（备份与整合扫描用；merged 可排除）
    async fn all_in_namespace(
        &self,
        namespace: &str,
        include_merged: bool,
    ) -> Result<Vec<MemoryEntry>>;

    /// 有上限的命名空间条目扫描（认知查重用）：最多返回 `limit` 条**最近更新**的
    /// 条目，避免命名空间条目爆炸时全量加载。
    ///
    /// 排序为 `updated_at desc`：查重与演化目标选择的语义是"和最近的事实比对"，
    /// 按 `created_at` 升序截断会让大命名空间只看到最老的一批条目，新重复漏判。
    /// 默认实现调用 `all_in_namespace` 排序截断，PostgreSQL 等生产存储应覆盖为
    /// SQL 层 `ORDER BY ... LIMIT`。
    async fn all_in_namespace_limited(
        &self,
        namespace: &str,
        include_merged: bool,
        limit: usize,
    ) -> Result<Vec<MemoryEntry>> {
        let mut all = self.all_in_namespace(namespace, include_merged).await?;
        all.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(b.id.cmp(&a.id)));
        if all.len() > limit {
            all.truncate(limit);
        }
        Ok(all)
    }

    /// 全部命名空间清单（定时全量备份用）
    async fn list_namespaces(&self) -> Result<Vec<Namespace>>;

    /// 追加一条版本历史
    async fn record_version(&self, v: &MemoryVersion) -> Result<()>;

    /// 查询版本历史（按版本号升序）
    async fn versions(&self, id: uuid::Uuid) -> Result<Vec<MemoryVersion>>;

    /// 命名空间下所有记忆版本历史（完整备份用）。
    async fn all_versions_in_namespace(&self, namespace: &str) -> Result<Vec<MemoryVersion>> {
        let entries = self.all_in_namespace(namespace, true).await?;
        let mut versions = Vec::new();
        for entry in entries {
            versions.extend(self.versions(entry.id).await?);
        }
        Ok(versions)
    }

    /// 批量归档过期条目（worker 用），返回归档条数
    async fn archive_expired(&self, now: DateTime<Utc>) -> Result<usize>;

    /// 存储就绪探活（真探库）：执行底层探测以确认服务可用
    async fn ping(&self) -> Result<()> {
        Ok(())
    }
}

/// 证据存根存储（不可变追加与查询，P1 引入）
#[async_trait]
pub trait EvidenceStore: Send + Sync {
    /// 追加一条证据存根（返回持久化后的记录）
    async fn append_evidence(&self, evidence: &NewEvidence) -> Result<MemoryEvidence>;

    /// 按 ID 读取证据存根
    async fn get_evidence(&self, namespace: &str, id: uuid::Uuid) -> Result<MemoryEvidence>;

    /// 条件列表
    async fn list_evidence(
        &self,
        namespace: &str,
        source_kind: Option<EvidenceSourceKind>,
        source_ref: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemoryEvidence>>;

    /// 按内容哈希点查证据（会话幂等查重用）；`None` = 不存在。
    ///
    /// 默认实现退化为 `list_evidence` 窗口扫描（历史语义，窗口外的哈希会漏）；
    /// 生产存储应覆盖为带索引的点查——幂等查重必须覆盖该会话的**全部**历史证据，
    /// 否则同一会话的证据超过窗口条数后会重复落证据。
    async fn find_evidence_by_hash(
        &self,
        namespace: &str,
        source_kind: EvidenceSourceKind,
        source_ref: &str,
        content_hash: &str,
    ) -> Result<Option<MemoryEvidence>> {
        let candidates = self
            .list_evidence(namespace, Some(source_kind), Some(source_ref), 256)
            .await?;
        Ok(candidates
            .into_iter()
            .find(|e| e.content_hash == content_hash))
    }

    /// 命名空间下全部证据（备份/恢复用）
    async fn all_evidence_in_namespace(&self, namespace: &str) -> Result<Vec<MemoryEvidence>>;

    /// 物理删除单条证据
    async fn delete_evidence(&self, namespace: &str, id: uuid::Uuid) -> Result<()>;

    /// 清理指定命名空间下已过保留期限的证据，返回删除条数。
    ///
    /// 证据写入前已脱敏（见 [`crate::redaction`]），本方法是保留期限的执行点：
    /// 到期即物理删除，不让不可变副本无限期留存。
    async fn purge_expired_evidence(&self, namespace: &str, now: DateTime<Utc>) -> Result<usize>;
}

/// 关系网存储（P1 引入）
#[async_trait]
pub trait RelationStore: Send + Sync {
    /// 增加一条关系（两端条目必须都属于 namespace）
    async fn add_relation(
        &self,
        namespace: &str,
        relation: &NewMemoryRelation,
    ) -> Result<MemoryRelation>;

    /// 列出某记忆条目出发的所有关系
    async fn list_relations_from(
        &self,
        namespace: &str,
        from_memory_id: uuid::Uuid,
    ) -> Result<Vec<MemoryRelation>>;

    /// 列出指向某记忆条目的所有关系
    async fn list_relations_to(
        &self,
        namespace: &str,
        to_memory_id: uuid::Uuid,
    ) -> Result<Vec<MemoryRelation>>;

    /// 删除指定关系
    async fn delete_relation(
        &self,
        namespace: &str,
        from_memory_id: uuid::Uuid,
        to_memory_id: uuid::Uuid,
        relation_kind: MemoryRelationKind,
    ) -> Result<()>;

    /// 命名空间下全部关系（备份/恢复用）
    async fn all_relations_in_namespace(&self, namespace: &str) -> Result<Vec<MemoryRelation>>;
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

    /// Replace a set of namespaces from complete snapshots in one atomic operation.
    async fn replace_namespaces_from_snapshots(
        &self,
        snapshots: &[(Namespace, crate::domain::RestoreSnapshot)],
    ) -> Result<()> {
        let _ = snapshots;
        Err(common::HubError::InvalidArgument(
            "atomic namespace restore is not supported by this store".to_string(),
        ))
    }
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

/// 认知提案存储（Chat 认知提炼的人工审查队列）
///
/// 与 [`IntegrationStore`] 是两套**独立的提案 ID 空间**：
/// 整合提案面向"既有条目怎么归并"，认知提案面向"新候选要不要入库"。
/// 两者的 UUID 不得互相传用；裁决入口按提案实际归属分发，
/// 混用时返回明确错误而不是笼统的 NotFound。
#[async_trait]
pub trait CognitiveProposalStore: Send + Sync {
    /// 新建认知提案（id 冲突时报错，不做覆盖更新）
    async fn save_cognitive_proposal(&self, p: &CognitiveProposalRecord) -> Result<()>;

    /// 按 ID 读取认知提案（不存在返回 NotFound）
    async fn get_cognitive_proposal(&self, id: uuid::Uuid) -> Result<CognitiveProposalRecord>;

    /// 条件列表（status 为 None 时列出全部状态，按 created_at 降序）
    async fn list_cognitive_proposals(
        &self,
        namespace: &str,
        status: Option<CognitiveProposalStatus>,
    ) -> Result<Vec<CognitiveProposalRecord>>;

    /// 更新提案状态（裁决动作）。
    ///
    /// `expected_status` 为乐观前置：提案当前状态不是它时返回 `Conflict`，
    /// 防止两个审查者对同一提案重复裁决（采纳两次会写入两条重复记忆）。
    async fn update_cognitive_proposal_status(
        &self,
        id: uuid::Uuid,
        expected_status: CognitiveProposalStatus,
        new_status: CognitiveProposalStatus,
        rejection_reason: Option<&str>,
        resulting_memory_id: Option<uuid::Uuid>,
        resolved_by: Option<&str>,
    ) -> Result<CognitiveProposalRecord>;
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

/// 任务事实存储（Working 场景，P3 引入）
#[async_trait]
pub trait TaskStore: Send + Sync {
    /// 创建一条任务记录
    async fn create_task(&self, task: &NewMemoryTask) -> Result<MemoryTask>;

    /// 更新任务状态及执行结果
    async fn update_task(&self, task: &MemoryTask) -> Result<MemoryTask>;

    /// 按 ID 读取任务
    async fn get_task(&self, namespace: &str, id: uuid::Uuid) -> Result<Option<MemoryTask>>;

    /// 列表任务
    async fn list_tasks(
        &self,
        namespace: &str,
        status: Option<TaskStatus>,
        limit: usize,
    ) -> Result<Vec<MemoryTask>>;

    /// 命名空间下全部任务（备份/恢复用）
    async fn all_tasks_in_namespace(&self, namespace: &str) -> Result<Vec<MemoryTask>>;

    /// 物理删除单条任务事实
    async fn delete_task(&self, namespace: &str, id: uuid::Uuid) -> Result<()>;
}

/// 经验记忆存储（Working 场景，P3 引入）
#[async_trait]
pub trait ExperienceStore: Send + Sync {
    /// 插入经验记忆条目
    async fn save_experience(&self, experience: &NewMemoryExperience) -> Result<MemoryExperience>;

    /// 更新经验记忆条目（包括反馈分、状态、方案等）
    async fn update_experience(&self, experience: &MemoryExperience) -> Result<MemoryExperience>;

    /// 乐观并发更新：仅当库中 `version == expected_version` 时写入。
    ///
    /// 经验内容的“读-改-写”路径（状态流转、内容修订）必须走本方法：
    /// 两个 Agent 同时读到同一版本再各自写回时，后写者不得静默覆盖前者。
    /// 版本不匹配返回 `HubError::Conflict`。
    ///
    /// 默认实现 = 先读后比再写，供无并发要求的测试替身使用；
    /// 生产存储与内存存储必须覆盖为原子实现。
    async fn update_experience_if_version(
        &self,
        experience: &MemoryExperience,
        expected_version: i64,
    ) -> Result<MemoryExperience> {
        match self
            .get_experience(&experience.namespace, experience.id)
            .await?
        {
            Some(current) if current.version == expected_version => {
                self.update_experience(experience).await
            }
            Some(_) => Err(common::HubError::Conflict),
            None => Err(common::HubError::NotFound),
        }
    }

    /// 追加经验版本快照（只追加；同一 `(experience_id, version)` 幂等忽略）
    async fn append_experience_version(
        &self,
        version: &NewExperienceVersion,
    ) -> Result<ExperienceVersion>;

    /// 读取某条经验的版本历史（version 降序）
    async fn list_experience_versions(
        &self,
        namespace: &str,
        experience_id: uuid::Uuid,
        limit: usize,
    ) -> Result<Vec<ExperienceVersion>>;

    /// 命名空间下全部版本快照（备份/恢复用）
    async fn all_experience_versions_in_namespace(
        &self,
        namespace: &str,
    ) -> Result<Vec<ExperienceVersion>>;

    /// 按 ID 读取经验记忆
    async fn get_experience(
        &self,
        namespace: &str,
        id: uuid::Uuid,
    ) -> Result<Option<MemoryExperience>>;

    /// 分页列出经验记忆（`offset` 为跳过的条数，按 updated_at 降序）
    ///
    /// 检索路径依赖分页扫描全量候选，硬编码"只取最近 N 条"会系统性漏召回。
    async fn list_experiences(
        &self,
        namespace: &str,
        status: Option<ExperienceStatus>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<MemoryExperience>>;

    /// 关联任务与经验
    async fn link_task_experience(&self, link: &MemoryTaskExperience) -> Result<()>;

    /// 查询某任务衍生的所有经验关联
    async fn list_experiences_for_task(
        &self,
        task_id: uuid::Uuid,
    ) -> Result<Vec<MemoryTaskExperience>>;

    /// 查询某条经验的全部来源任务关联（验证门槛与 source_count 维护用）
    ///
    /// 默认实现扫描命名空间全量映射；生产存储应覆盖为按 experience_id 的定向查询。
    async fn list_links_for_experience(
        &self,
        namespace: &str,
        experience_id: uuid::Uuid,
    ) -> Result<Vec<MemoryTaskExperience>> {
        Ok(self
            .all_task_experiences_in_namespace(namespace)
            .await?
            .into_iter()
            .filter(|l| l.experience_id == experience_id)
            .collect())
    }

    /// 命名空间下全部经验记忆（备份/恢复用）
    async fn all_experiences_in_namespace(&self, namespace: &str) -> Result<Vec<MemoryExperience>>;

    /// 命名空间下全部任务-经验映射（备份/恢复用）
    async fn all_task_experiences_in_namespace(
        &self,
        namespace: &str,
    ) -> Result<Vec<MemoryTaskExperience>>;

    /// 物理删除单条经验记忆
    async fn delete_experience(&self, namespace: &str, id: uuid::Uuid) -> Result<()>;
}

/// Agent 记忆集合存储（账户级配置，任务书 4.4）
///
/// 集合与绑定**不含任何记忆内容**，只描述“哪些命名空间/场景/类型可作为候选范围”。
/// 删除集合或绑定不得删除底层记忆。
#[async_trait]
pub trait CollectionStore: Send + Sync {
    /// 新建或更新集合（按 `(account_id, name)` 唯一）
    async fn upsert_collection(&self, collection: &MemoryCollection) -> Result<MemoryCollection>;

    async fn get_collection(
        &self,
        account_id: AccountId,
        id: uuid::Uuid,
    ) -> Result<Option<MemoryCollection>>;

    async fn get_collection_by_name(
        &self,
        account_id: AccountId,
        name: &str,
    ) -> Result<Option<MemoryCollection>>;

    async fn list_collections(&self, account_id: AccountId) -> Result<Vec<MemoryCollection>>;

    /// 删除集合（级联删除绑定，不触碰记忆）
    async fn delete_collection(&self, account_id: AccountId, id: uuid::Uuid) -> Result<()>;

    async fn bind_collection(&self, binding: &CollectionBinding) -> Result<()>;

    async fn unbind_collection(&self, collection_id: uuid::Uuid, agent_id: &AgentId) -> Result<()>;

    async fn list_bindings(&self, collection_id: uuid::Uuid) -> Result<Vec<CollectionBinding>>;

    async fn is_bound(&self, collection_id: uuid::Uuid, agent_id: &AgentId) -> Result<bool>;
}

/// 代码演进存储（Coding 场景，P4 引入）
#[async_trait]
pub trait CodingStore: Send + Sync {
    /// 创建代码演进逻辑线程
    async fn create_thread(&self, thread: &NewCodingThread) -> Result<CodingThread>;

    /// 获取代码演进线程
    async fn get_thread(&self, namespace: &str, id: uuid::Uuid) -> Result<Option<CodingThread>>;

    /// 列表代码演进线程
    async fn list_threads(
        &self,
        namespace: &str,
        repository: Option<&str>,
        limit: usize,
    ) -> Result<Vec<CodingThread>>;

    /// 保存代码修订记录
    async fn save_revision(
        &self,
        revision: &NewCodingRevision,
        diff_hash: &str,
    ) -> Result<CodingRevision>;

    /// 按 ID 获取代码修订记录
    async fn get_revision(&self, namespace: &str, id: uuid::Uuid)
        -> Result<Option<CodingRevision>>;

    /// 按 Diff 哈希查重检索
    async fn get_revision_by_hash(
        &self,
        namespace: &str,
        repository: &str,
        diff_hash: &str,
    ) -> Result<Option<CodingRevision>>;

    /// 列表某线程下的全部修订记录
    async fn list_revisions_for_thread(
        &self,
        namespace: &str,
        thread_id: uuid::Uuid,
        limit: usize,
    ) -> Result<Vec<CodingRevision>>;

    /// 列表某文件路径下的修订历史链
    async fn list_revisions_for_file(
        &self,
        namespace: &str,
        repository: &str,
        file_path: &str,
        limit: usize,
    ) -> Result<Vec<CodingRevision>>;

    /// 命名空间下全部代码线程（备份/恢复用）
    async fn all_threads_in_namespace(&self, namespace: &str) -> Result<Vec<CodingThread>>;

    /// 命名空间下全部代码修订记录（备份/恢复用）
    async fn all_revisions_in_namespace(&self, namespace: &str) -> Result<Vec<CodingRevision>>;

    /// 物理删除单条代码演进线程
    async fn delete_thread(&self, namespace: &str, id: uuid::Uuid) -> Result<()>;

    /// 物理删除单条代码修订记录
    async fn delete_revision(&self, namespace: &str, id: uuid::Uuid) -> Result<()>;
}

/// 存储聚合：一套实现同时满足全部核心与认知存储需求
pub trait MemoryHubStore:
    MemoryStore
    + BackupStore
    + IntegrationStore
    + CognitiveProposalStore
    + GrantStore
    + AgentStore
    + EvidenceStore
    + RelationStore
    + TaskStore
    + ExperienceStore
    + CodingStore
    + CollectionStore
{
}

impl<
        T: MemoryStore
            + BackupStore
            + IntegrationStore
            + CognitiveProposalStore
            + GrantStore
            + AgentStore
            + EvidenceStore
            + RelationStore
            + TaskStore
            + ExperienceStore
            + CodingStore
            + CollectionStore,
    > MemoryHubStore for T
{
}

/// Agent 注册请求（授权服务器的 DCR 入口）
#[derive(Debug, Clone)]
pub struct AgentRegistration {
    pub agent_id: AgentId,
    pub name: String,
    /// service | assistant | worker
    pub kind: String,
    /// 归属账户（沙箱边界；注册时由授权服务器注入，不取自请求体）
    pub owner_account_id: AccountId,
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
    /// 归属账户
    pub owner_account_id: AccountId,
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

/// 账户注册记录（不含口令哈希本体）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountRecord {
    pub id: AccountId,
    pub username: String,
    pub display_name: String,
    /// active | suspended
    pub status: String,
    /// 是否已设置登录口令
    pub has_password: bool,
    pub created_at: DateTime<Utc>,
}

/// 账户登录会话记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub account_id: AccountId,
    pub expires_at: DateTime<Utc>,
}

/// 账户存储：注册、登录凭据与登录会话（auth-server 使用）
///
/// 账户是沙箱边界：命名空间数据不直接携带 account 列，
/// 而是通过 `AccountScopedStore` 按账户前缀隔离（见 account_scope 模块）。
#[async_trait]
pub trait AccountStore: Send + Sync {
    /// 创建账户（用户名冲突返回 InvalidArgument）
    async fn create_account(
        &self,
        username: &str,
        password_hash: Option<&str>,
        display_name: &str,
    ) -> Result<AccountRecord>;
    /// 按固定 ID 确保账户存在（引导账户；已存在时原样返回，不改口令）
    async fn ensure_account(
        &self,
        id: &AccountId,
        username: &str,
        display_name: &str,
    ) -> Result<AccountRecord>;
    async fn get_account(&self, id: &AccountId) -> Result<Option<AccountRecord>>;
    async fn get_account_by_username(&self, username: &str) -> Result<Option<AccountRecord>>;
    /// 读取口令哈希（仅认证路径使用，禁止用于展示）
    async fn password_hash_of(&self, username: &str) -> Result<Option<String>>;
    async fn set_account_password(&self, id: &AccountId, password_hash: &str) -> Result<()>;
    async fn set_account_status(&self, id: &AccountId, status: &str) -> Result<()>;
    async fn list_accounts(&self) -> Result<Vec<AccountRecord>>;

    // ===== 登录会话（token_hash 为会话令牌的 SHA-256）=====
    async fn create_session(
        &self,
        token_hash: &[u8],
        account_id: &AccountId,
        expires_at: DateTime<Utc>,
    ) -> Result<()>;
    async fn get_session(&self, token_hash: &[u8]) -> Result<Option<SessionRecord>>;
    async fn delete_session(&self, token_hash: &[u8]) -> Result<()>;
    /// 清理过期会话，返回删除条数
    async fn prune_sessions(&self, now: DateTime<Utc>) -> Result<usize>;
}

/// 嵌入器：文本 → 归一化向量
#[async_trait]
pub trait Embedder: Send + Sync {
    fn model_id(&self) -> &str;
    fn dim(&self) -> usize;
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;

    /// 该嵌入器是否提供**真实语义信号**。
    ///
    /// 伪向量实现（按哈希字节构造的 Mock / Test 嵌入器）返回 `false`：
    /// 同一语义的两次文本得到互不相关的向量，不相关文本反而可能偶然相近，
    /// 它的余弦相似度是噪声而不是语义。调用方据此决定：
    ///
    /// - 打分时不得把向量维度当作有效信号注入排序（否则噪声按权重放大）；
    /// - 不得基于向量维度声称"语义能力就绪"（健康检查与评测报告都要区分）；
    /// - 不得用伪向量下的分数校准真实语义阈值（阈值只在真实模型下校准）。
    ///
    /// 真实模型（远程嵌入服务）保持默认 `true`。
    fn provides_semantic_signal(&self) -> bool {
        true
    }

    /// 该嵌入器是否**启用**（能否产出向量）。
    ///
    /// 与 `provides_semantic_signal` 是两个不同问题：前者回答"向量有没有语义"，
    /// 本方法回答"要不要为这篇记忆/这次查询花一次嵌入调用"。
    /// 只有显式关闭嵌入（`HUB_EMBED_PROVIDER=off`，个人轻量部署默认）时返回 `false`；
    /// 调用方据此跳过嵌入调用与向量召回，而不是先发一次必然失败的请求再降级。
    ///
    /// 默认 `true`：所有既有实现行为不变。
    fn is_enabled(&self) -> bool {
        true
    }
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

/// 访问判定：命名空间级读/写/管理权（基于 GrantStore，账户沙箱内判定）
///
/// 所有判定以 `auth.account_id` 为边界：命名空间名先按账户作用域化，
/// 跨账户的命名空间在授权表中永远找不到匹配行。
#[async_trait]
pub trait AccessChecker: Send + Sync {
    async fn can_read(&self, auth: &AuthCtx, namespace: &str) -> Result<bool>;
    async fn can_write(&self, auth: &AuthCtx, namespace: &str) -> Result<bool>;
    /// 是否为命名空间 owner
    async fn is_owner(&self, auth: &AuthCtx, namespace: &str) -> Result<bool>;
    /// 首次写入时自动注册命名空间并建立 owner 授权
    async fn ensure_namespace(&self, auth: &AuthCtx, namespace: &str) -> Result<()>;
}

/// 对话认知提炼抽取器（由外部 LLM provider 或 Mock 实现）
#[async_trait]
pub trait ChatCognitiveExtractor: Send + Sync {
    async fn extract_candidates(
        &self,
        input: &ChatCognitiveInput,
    ) -> Result<Vec<CognitiveCandidate>>;
}
