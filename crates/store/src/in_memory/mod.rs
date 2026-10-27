//! 内存级存储实现（开发、测试与无数据库环境）
//!
//! 单进程内实现全部存储契约（记忆 / 备份 / 整合提案 / 命名空间授权 / Agent / 账户），
//! 语义与 PostgreSQL 实现对齐：命名空间隔离、owner 首次写入、游标分页、
//! 软删除归档、合并溯源。

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use common::{AccountId, AgentId, HubError, Result};
use mem_core::{
    AccountRecord, BackupRecord, CodingRevision, CodingStore, CodingThread,
    CognitiveProposalRecord, CognitiveProposalStatus, CollectionBinding, EvidenceSourceKind,
    ExperienceStatus, ExperienceVersion, IntegrationProposal, MemoryCollection, MemoryEntry,
    MemoryEvidence, MemoryExperience, MemoryFilter, MemoryRelation, MemoryRelationKind,
    MemoryStats, MemoryStatus, MemoryTask, MemoryTaskExperience, MemoryVersion, Namespace,
    NamespaceGrantRecord, NamespaceInfo, NewCodingRevision, NewCodingThread, NewEvidence,
    NewExperienceVersion, NewMemoryExperience, NewMemoryRelation, NewMemoryTask, ProposalStatus,
    Scored, SessionRecord, TaskStatus, VersionChange,
};
use std::collections::BTreeMap;
use std::sync::RwLock;
use uuid::Uuid;

type RelationKey = (Uuid, Uuid, MemoryRelationKind);
type RelationEntry = (Namespace, MemoryRelation);

/// 内存存储（多合一）
#[derive(Default)]
pub struct InMemoryHubStore {
    /// (namespace, id) → 条目
    items: RwLock<BTreeMap<(Namespace, Uuid), MemoryEntry>>,
    versions: RwLock<BTreeMap<(Uuid, i64), MemoryVersion>>,
    evidences: RwLock<BTreeMap<(Namespace, Uuid), MemoryEvidence>>,
    relations: RwLock<BTreeMap<RelationKey, RelationEntry>>,
    tasks: RwLock<BTreeMap<(Namespace, Uuid), MemoryTask>>,
    experiences: RwLock<BTreeMap<(Namespace, Uuid), MemoryExperience>>,
    experience_versions: RwLock<BTreeMap<(Uuid, i64), ExperienceVersion>>,
    task_experiences: RwLock<Vec<(Namespace, MemoryTaskExperience)>>,
    coding_threads: RwLock<BTreeMap<(Namespace, Uuid), CodingThread>>,
    coding_revisions: RwLock<BTreeMap<(Namespace, Uuid), CodingRevision>>,
    backups: RwLock<BTreeMap<Uuid, BackupRecord>>,
    proposals: RwLock<BTreeMap<Uuid, IntegrationProposal>>,
    cognitive_proposals: RwLock<BTreeMap<Uuid, CognitiveProposalRecord>>,
    namespaces: RwLock<BTreeMap<Namespace, NamespaceInfo>>,
    grants: RwLock<Vec<NamespaceGrantRecord>>,
    collections: RwLock<Vec<MemoryCollection>>,
    collection_bindings: RwLock<Vec<CollectionBinding>>,
    agents: RwLock<BTreeMap<AgentId, StoredAgent>>,
    accounts: RwLock<BTreeMap<AccountId, StoredAccount>>,
    /// sha256(会话令牌) → 会话
    sessions: RwLock<BTreeMap<Vec<u8>, SessionRecord>>,
}

/// 内部账户记录（含口令散列）
#[derive(Default, Clone)]
struct StoredAccount {
    username: String,
    display_name: String,
    status: String,
    password_hash: Option<String>,
    created_at: Option<chrono::DateTime<Utc>>,
}

/// 内部 Agent 记录（含密钥散列与账户归属）
#[derive(Default, Clone)]
struct StoredAgent {
    name: String,
    kind: String,
    status: String,
    owner_account_id: Option<AccountId>,
    secret_hash: Option<String>,
    redirect_uris: Vec<String>,
    max_scopes: Vec<common::Scope>,
    created_at: Option<chrono::DateTime<Utc>>,
}

impl InMemoryHubStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 写回条目。`expected_version` 为 `Some(v)` 时要求库中当前版本恰为 v，
    /// 否则返回 Conflict（与 PostgreSQL 实现同一语义）。`None` 为无条件写回。
    /// 条目不存在时同样返回 Conflict：乐观锁的前提（该行存在且版本为 v）
    /// 不成立，与 PG 侧 update 0 行 → Conflict 的行为对齐。
    async fn apply_revision(
        &self,
        entry: &MemoryEntry,
        expected_version: Option<i64>,
    ) -> Result<MemoryEntry> {
        let mut w = self.items.write().unwrap();
        let key = (entry.namespace.clone(), entry.id);
        match w.get_mut(&key) {
            Some(current) => match expected_version {
                Some(v) if current.version != v => Err(common::HubError::Conflict),
                _ => {
                    *current = entry.clone();
                    Ok(entry.clone())
                }
            },
            None => Err(common::HubError::Conflict),
        }
    }

    /// 当前条目总数（测试辅助）
    pub fn count_all(&self) -> usize {
        self.items.read().unwrap().len()
    }

    /// 应用过滤条件（共享辅助）
    fn matches_filter(e: &MemoryEntry, f: &MemoryFilter) -> bool {
        if !Self::matches_filter_without_text(e, f) {
            return false;
        }
        if let Some(text) = &f.text_contains {
            // 与 PostgreSQL FTS 同源的分词部分匹配（OR 语义）
            if mem_core::fts_query_hits(&e.content, text) == 0 {
                return false;
            }
        }
        true
    }

    /// 除文本命中外的全部过滤条件（list 需要复用文档词元做相关度排序）
    fn matches_filter_without_text(e: &MemoryEntry, f: &MemoryFilter) -> bool {
        if let Some(ns) = &f.namespace {
            if &e.namespace != ns {
                return false;
            }
        }
        if let Some(kinds) = &f.kinds {
            if !kinds.contains(&e.kind) {
                return false;
            }
        }
        if let Some(statuses) = &f.statuses {
            if !statuses.contains(&e.status) {
                return false;
            }
        }
        if let Some(tags) = &f.tags {
            if !tags.iter().all(|t| e.tags.contains(t)) {
                return false;
            }
        }
        if let Some(agent) = &f.source_agent {
            if &e.source_agent != agent {
                return false;
            }
        }
        if let Some(key) = &f.key {
            if e.key.as_deref() != Some(key.as_str()) {
                return false;
            }
        }
        if let Some((ts, id)) = &f.before {
            if (e.updated_at, e.id) >= (*ts, *id) {
                return false;
            }
        }
        if let Some(scene) = &f.scene {
            if e.scene.as_ref() != Some(scene) {
                return false;
            }
        }
        if let Some(layer) = &f.layer {
            if e.layer.as_ref() != Some(layer) {
                return false;
            }
        }
        if let Some(valid_at) = &f.valid_at {
            if let Some(from) = e.valid_from {
                if *valid_at < from {
                    return false;
                }
            }
            if let Some(until) = e.valid_until {
                if *valid_at >= until {
                    return false;
                }
            }
        }
        true
    }
}

/// 便捷统计（运维排障）
pub fn stats_of(store: &InMemoryHubStore) -> MemoryStats {
    let items = store.items.read().unwrap();
    let mut stats = MemoryStats {
        namespace: "*".to_string(),
        ..Default::default()
    };
    for e in items.values() {
        match e.status {
            MemoryStatus::Active => stats.active += 1,
            MemoryStatus::Archived => stats.archived += 1,
            MemoryStatus::Merged => stats.merged += 1,
        }
        *stats
            .by_kind
            .entry(e.kind.as_str().to_string())
            .or_insert(0) += 1;
    }
    stats.total = stats.active + stats.archived + stats.merged;
    stats
}

#[cfg(test)]
mod tests;

mod access;
mod backup;
mod coding;
mod collections;
mod integration;
mod memory;
mod working;
