//! 内存级存储实现（开发、测试与无数据库环境）
//!
//! 单进程内实现全部存储契约（记忆 / 备份 / 整合提案 / 命名空间授权 / Agent / 账户），
//! 语义与 PostgreSQL 实现对齐：命名空间隔离、owner 首次写入、游标分页、
//! 软删除归档、合并溯源。

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use common::{AccountId, AgentId, HubError, Result};
use mem_core::{
    AccountRecord, BackupRecord, IntegrationProposal, MemoryEntry, MemoryFilter, MemoryKind,
    MemoryStats, MemoryStatus, MemoryVersion, Namespace, NamespaceGrantRecord, NamespaceInfo,
    ProposalStatus, Scored, SessionRecord, VersionChange,
};
use std::collections::BTreeMap;
use std::sync::RwLock;
use uuid::Uuid;

/// 内存存储（多合一）
#[derive(Default)]
pub struct InMemoryHubStore {
    /// (namespace, id) → 条目
    items: RwLock<BTreeMap<(Namespace, Uuid), MemoryEntry>>,
    versions: RwLock<Vec<MemoryVersion>>,
    backups: RwLock<BTreeMap<Uuid, BackupRecord>>,
    proposals: RwLock<BTreeMap<Uuid, IntegrationProposal>>,
    namespaces: RwLock<BTreeMap<Namespace, NamespaceInfo>>,
    grants: RwLock<Vec<NamespaceGrantRecord>>,
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

/// 密钥散列：sha256(agent_id:secret)（不落明文）
fn hash_secret(agent_id: &str, secret: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(agent_id.as_bytes());
    h.update(b":");
    h.update(secret.as_bytes());
    hex::encode(h.finalize())
}

impl InMemoryHubStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前条目总数（测试辅助）
    pub fn count_all(&self) -> usize {
        self.items.read().unwrap().len()
    }

    /// 应用过滤条件（共享辅助）
    fn matches_filter(e: &MemoryEntry, f: &MemoryFilter) -> bool {
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
        if let Some(text) = &f.text_contains {
            let needle = text.to_lowercase();
            if !e.content.to_lowercase().contains(&needle) {
                return false;
            }
        }
        if let Some((ts, id)) = &f.before {
            if (e.updated_at, e.id) >= (*ts, *id) {
                return false;
            }
        }
        true
    }
}

#[async_trait]
impl mem_core::MemoryStore for InMemoryHubStore {
    async fn insert(&self, entry: &MemoryEntry) -> Result<MemoryEntry> {
        let mut w = self.items.write().unwrap();
        let key = (entry.namespace.clone(), entry.id);
        if w.contains_key(&key) {
            return Err(HubError::Conflict);
        }
        w.insert(key, entry.clone());
        Ok(entry.clone())
    }

    async fn get(&self, namespace: &str, id: Uuid) -> Result<MemoryEntry> {
        self.items
            .read()
            .unwrap()
            .get(&(namespace.to_string(), id))
            .cloned()
            .ok_or(HubError::NotFound)
    }

    async fn get_by_key(&self, namespace: &str, key: &str) -> Result<Option<MemoryEntry>> {
        Ok(self
            .items
            .read()
            .unwrap()
            .values()
            .find(|e| e.namespace == namespace && e.key.as_deref() == Some(key))
            .cloned())
    }

    async fn save(&self, entry: &MemoryEntry) -> Result<MemoryEntry> {
        self.items
            .write()
            .unwrap()
            .insert((entry.namespace.clone(), entry.id), entry.clone());
        Ok(entry.clone())
    }

    async fn delete(&self, namespace: &str, id: Uuid) -> Result<()> {
        self.items
            .write()
            .unwrap()
            .remove(&(namespace.to_string(), id));
        Ok(())
    }

    async fn list(&self, filter: &MemoryFilter) -> Result<Vec<MemoryEntry>> {
        let mut out: Vec<MemoryEntry> = self
            .items
            .read()
            .unwrap()
            .values()
            .filter(|e| Self::matches_filter(e, filter))
            .cloned()
            .collect();
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(b.id.cmp(&a.id)));
        if let Some(limit) = filter.limit {
            out.truncate(limit);
        }
        Ok(out)
    }

    async fn count(&self, filter: &MemoryFilter) -> Result<usize> {
        let no_limit = MemoryFilter {
            limit: None,
            ..filter.clone()
        };
        Ok(self
            .items
            .read()
            .unwrap()
            .values()
            .filter(|e| Self::matches_filter(e, &no_limit))
            .count())
    }

    async fn similar(
        &self,
        namespace: &str,
        embedding: &[f32],
        k: usize,
        kinds: Option<&[MemoryKind]>,
    ) -> Result<Vec<Scored<MemoryEntry>>> {
        let mut scored: Vec<Scored<MemoryEntry>> = self
            .items
            .read()
            .unwrap()
            .values()
            .filter(|e| e.namespace == namespace && e.status == MemoryStatus::Active)
            .filter(|e| kinds.is_none_or(|ks| ks.contains(&e.kind)))
            .filter_map(|e| {
                let iv = e.embedding.as_ref()?;
                if iv.len() != embedding.len() {
                    return None;
                }
                let dot: f32 = iv.iter().zip(embedding.iter()).map(|(x, y)| x * y).sum();
                Some(Scored {
                    item: e.clone(),
                    score: dot.clamp(0.0, 1.0),
                })
            })
            .collect();
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(k);
        Ok(scored)
    }

    async fn all_in_namespace(
        &self,
        namespace: &str,
        include_merged: bool,
    ) -> Result<Vec<MemoryEntry>> {
        let mut out: Vec<MemoryEntry> = self
            .items
            .read()
            .unwrap()
            .values()
            .filter(|e| e.namespace == namespace)
            .filter(|e| include_merged || e.status != MemoryStatus::Merged)
            .cloned()
            .collect();
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        Ok(out)
    }

    async fn list_namespaces(&self) -> Result<Vec<Namespace>> {
        let mut ns: Vec<Namespace> = self
            .items
            .read()
            .unwrap()
            .values()
            .map(|e| e.namespace.clone())
            .collect();
        ns.sort();
        ns.dedup();
        Ok(ns)
    }

    async fn record_version(&self, v: &MemoryVersion) -> Result<()> {
        let mut w = self.versions.write().unwrap();
        // 同 (memory_id, version) 只保留首条（幂等重放保护）
        if w.iter()
            .any(|x| x.memory_id == v.memory_id && x.version == v.version)
        {
            return Ok(());
        }
        w.push(v.clone());
        Ok(())
    }

    async fn versions(&self, id: Uuid) -> Result<Vec<MemoryVersion>> {
        let mut out: Vec<MemoryVersion> = self
            .versions
            .read()
            .unwrap()
            .iter()
            .filter(|v| v.memory_id == id)
            .cloned()
            .collect();
        out.sort_by_key(|v| v.version);
        Ok(out)
    }

    async fn archive_expired(&self, now: DateTime<Utc>) -> Result<usize> {
        // 第一遍：收集待归档条目快照（避免持锁写版本表）
        let expired: Vec<MemoryEntry> = {
            let r = self.items.read().unwrap();
            r.values()
                .filter(|e| {
                    e.status == MemoryStatus::Active && e.expires_at.is_some_and(|exp| exp <= now)
                })
                .cloned()
                .collect()
        };

        // 第二遍：逐条归档并记录版本历史
        let mut count = 0usize;
        for mut e in expired {
            e.status = MemoryStatus::Archived;
            e.version += 1;
            e.updated_at = now;
            let version = MemoryVersion {
                memory_id: e.id,
                version: e.version,
                content: e.content.clone(),
                tags: e.tags.clone(),
                metadata: e.metadata.clone(),
                confidence: e.confidence,
                changed_by: "system".to_string(),
                change_type: VersionChange::Archived,
                changed_at: now,
            };
            self.items
                .write()
                .unwrap()
                .insert((e.namespace.clone(), e.id), e);
            self.record_version(&version).await?;
            count += 1;
        }
        Ok(count)
    }
}

#[async_trait]
impl mem_core::BackupStore for InMemoryHubStore {
    async fn save_backup(&self, backup: &BackupRecord) -> Result<()> {
        self.backups
            .write()
            .unwrap()
            .insert(backup.id, backup.clone());
        Ok(())
    }

    async fn get_backup(&self, id: Uuid) -> Result<BackupRecord> {
        self.backups
            .read()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or(HubError::NotFound)
    }

    async fn list_backups(&self, namespace: Option<&str>) -> Result<Vec<BackupRecord>> {
        let mut out: Vec<BackupRecord> = self
            .backups
            .read()
            .unwrap()
            .values()
            .filter(|b| namespace.is_none_or(|n| b.namespace == n))
            .cloned()
            .collect();
        out.sort_by_key(|b| std::cmp::Reverse(b.created_at));
        Ok(out)
    }

    async fn delete_backup(&self, id: Uuid) -> Result<()> {
        self.backups.write().unwrap().remove(&id);
        Ok(())
    }
}

#[async_trait]
impl mem_core::IntegrationStore for InMemoryHubStore {
    async fn save_proposal(&self, p: &IntegrationProposal) -> Result<()> {
        self.proposals.write().unwrap().insert(p.id, p.clone());
        Ok(())
    }

    async fn get_proposal(&self, id: Uuid) -> Result<IntegrationProposal> {
        self.proposals
            .read()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or(HubError::NotFound)
    }

    async fn list_proposals(
        &self,
        namespace: &str,
        status: Option<ProposalStatus>,
    ) -> Result<Vec<IntegrationProposal>> {
        let mut out: Vec<IntegrationProposal> = self
            .proposals
            .read()
            .unwrap()
            .values()
            .filter(|p| p.namespace == namespace)
            .filter(|p| status.is_none_or(|s| p.status == s))
            .cloned()
            .collect();
        out.sort_by_key(|b| std::cmp::Reverse(b.created_at));
        Ok(out)
    }

    async fn open_exists(&self, namespace: &str, members: &[(String, Vec<Uuid>)]) -> Result<bool> {
        let guard = self.proposals.read().unwrap();
        Ok(guard.values().any(|p| {
            p.status == ProposalStatus::Open
                && p.namespace == namespace
                && members
                    .iter()
                    .any(|(kind, ids)| kind == p.kind.as_str() && &p.member_ids == ids)
        }))
    }
}

#[async_trait]
impl mem_core::GrantStore for InMemoryHubStore {
    async fn ensure_namespace(
        &self,
        namespace: &str,
        owner: &AgentId,
    ) -> Result<(NamespaceInfo, bool)> {
        let mut w = self.namespaces.write().unwrap();
        let mut created = false;
        let info = w
            .entry(namespace.to_string())
            .or_insert_with(|| {
                created = true;
                NamespaceInfo {
                    namespace: namespace.to_string(),
                    owner_agent: owner.clone(),
                    created_at: Utc::now(),
                }
            })
            .clone();
        Ok((info, created))
    }

    async fn get_namespace(&self, namespace: &str) -> Result<Option<NamespaceInfo>> {
        Ok(self.namespaces.read().unwrap().get(namespace).cloned())
    }

    async fn list_namespace_infos(&self) -> Result<Vec<NamespaceInfo>> {
        Ok(self.namespaces.read().unwrap().values().cloned().collect())
    }

    async fn grant(&self, record: &NamespaceGrantRecord) -> Result<()> {
        let mut w = self.grants.write().unwrap();
        w.retain(|x| !(x.namespace == record.namespace && x.agent_id == record.agent_id));
        w.push(record.clone());
        Ok(())
    }

    async fn revoke(&self, namespace: &str, agent_id: &AgentId) -> Result<()> {
        self.grants
            .write()
            .unwrap()
            .retain(|x| !(x.namespace == namespace && x.agent_id == *agent_id));
        Ok(())
    }

    async fn grants_for(&self, agent_id: &AgentId) -> Result<Vec<NamespaceGrantRecord>> {
        Ok(self
            .grants
            .read()
            .unwrap()
            .iter()
            .filter(|x| &x.agent_id == agent_id)
            .cloned()
            .collect())
    }

    async fn can_read(&self, namespace: &str, agent_id: &AgentId) -> Result<bool> {
        // 沙箱语义：授权行必须精确匹配命名空间；`*` 通配授权已废除
        Ok(self
            .grants
            .read()
            .unwrap()
            .iter()
            .any(|x| x.namespace == namespace && x.agent_id == *agent_id && x.can_read))
    }

    async fn can_write(&self, namespace: &str, agent_id: &AgentId) -> Result<bool> {
        Ok(self
            .grants
            .read()
            .unwrap()
            .iter()
            .any(|x| x.namespace == namespace && x.agent_id == *agent_id && x.can_write))
    }

    async fn owned_namespaces(&self, agent_id: &AgentId) -> Result<Vec<Namespace>> {
        Ok(self
            .namespaces
            .read()
            .unwrap()
            .values()
            .filter(|n| &n.owner_agent == agent_id)
            .map(|n| n.namespace.clone())
            .collect())
    }
}

#[async_trait]
impl mem_core::AgentStore for InMemoryHubStore {
    async fn register_agent(
        &self,
        reg: &mem_core::AgentRegistration,
    ) -> Result<mem_core::AgentRecord> {
        let mut w = self.agents.write().unwrap();
        let entry = w
            .entry(reg.agent_id.clone())
            .or_insert_with(|| StoredAgent {
                name: reg.name.clone(),
                kind: reg.kind.clone(),
                status: "active".to_string(),
                owner_account_id: Some(reg.owner_account_id),
                secret_hash: None,
                redirect_uris: Vec::new(),
                max_scopes: Vec::new(),
                created_at: Some(Utc::now()),
            });
        // upsert 语义：名称/类型/账户归属/授权域以本次为准；密钥仅在提供时更新
        entry.name = reg.name.clone();
        entry.kind = reg.kind.clone();
        entry.owner_account_id = Some(reg.owner_account_id);
        entry.redirect_uris = reg.redirect_uris.clone();
        entry.max_scopes = reg.max_scopes.clone();
        if let Some(secret) = &reg.client_secret {
            entry.secret_hash = Some(hash_secret(&reg.agent_id, secret));
        }
        Ok(agent_record(&reg.agent_id, entry))
    }

    async fn get_agent(&self, agent_id: &AgentId) -> Result<Option<mem_core::AgentRecord>> {
        Ok(self
            .agents
            .read()
            .unwrap()
            .get(agent_id)
            .map(|a| agent_record(agent_id, a)))
    }

    async fn list_agents(&self) -> Result<Vec<mem_core::AgentRecord>> {
        Ok(self
            .agents
            .read()
            .unwrap()
            .iter()
            .map(|(id, a)| agent_record(id, a))
            .collect())
    }

    async fn set_agent_status(&self, agent_id: &AgentId, status: &str) -> Result<()> {
        let mut w = self.agents.write().unwrap();
        match w.get_mut(agent_id) {
            Some(a) => {
                a.status = status.to_string();
                Ok(())
            }
            None => Err(HubError::NotFound),
        }
    }

    async fn verify_agent_secret(&self, agent_id: &AgentId, secret: &str) -> Result<bool> {
        let w = self.agents.read().unwrap();
        Ok(match w.get(agent_id) {
            Some(a) => match &a.secret_hash {
                Some(h) => h == &hash_secret(agent_id, secret),
                None => false,
            },
            None => false,
        })
    }
}

/// StoredAgent → AgentRecord
fn agent_record(agent_id: &str, a: &StoredAgent) -> mem_core::AgentRecord {
    mem_core::AgentRecord {
        agent_id: agent_id.to_string(),
        name: a.name.clone(),
        kind: a.kind.clone(),
        status: a.status.clone(),
        owner_account_id: a.owner_account_id.unwrap_or(common::BOOTSTRAP_ACCOUNT_ID),
        redirect_uris: a.redirect_uris.clone(),
        max_scopes: a.max_scopes.clone(),
        confidential: a.secret_hash.is_some(),
        created_at: a.created_at.unwrap_or_else(Utc::now),
    }
}

/// StoredAccount → AccountRecord（不暴露口令散列本体）
fn account_record(id: &AccountId, a: &StoredAccount) -> AccountRecord {
    AccountRecord {
        id: *id,
        username: a.username.clone(),
        display_name: a.display_name.clone(),
        status: a.status.clone(),
        has_password: a.password_hash.is_some(),
        created_at: a.created_at.unwrap_or_else(Utc::now),
    }
}

#[async_trait]
impl mem_core::AccountStore for InMemoryHubStore {
    async fn create_account(
        &self,
        username: &str,
        password_hash: Option<&str>,
        display_name: &str,
    ) -> Result<AccountRecord> {
        let normalized = username.trim().to_ascii_lowercase();
        let mut w = self.accounts.write().unwrap();
        if w.values().any(|a| a.username == normalized) {
            return Err(HubError::InvalidArgument(format!(
                "用户名 {} 已被占用",
                normalized
            )));
        }
        let id = Uuid::new_v4();
        let record = StoredAccount {
            username: normalized,
            display_name: display_name.to_string(),
            status: "active".to_string(),
            password_hash: password_hash.map(str::to_string),
            created_at: Some(Utc::now()),
        };
        w.insert(id, record.clone());
        Ok(account_record(&id, &record))
    }

    async fn ensure_account(
        &self,
        id: &AccountId,
        username: &str,
        display_name: &str,
    ) -> Result<AccountRecord> {
        let normalized = username.trim().to_ascii_lowercase();
        let mut w = self.accounts.write().unwrap();
        // 用户名已被别的账户占用时，退回按 ID 查找（保持幂等）
        let existing = w
            .get(id)
            .map(|a| (id.to_owned(), a.clone()))
            .or_else(|| {
                w.iter()
                    .find(|(_, a)| a.username == normalized)
                    .map(|(k, a)| (*k, a.clone()))
            });
        if let Some((found_id, record)) = existing {
            return Ok(account_record(&found_id, &record));
        }
        let record = StoredAccount {
            username: normalized,
            display_name: display_name.to_string(),
            status: "active".to_string(),
            password_hash: None,
            created_at: Some(Utc::now()),
        };
        w.insert(*id, record.clone());
        Ok(account_record(id, &record))
    }

    async fn get_account(&self, id: &AccountId) -> Result<Option<AccountRecord>> {
        Ok(self.accounts.read().unwrap().get(id).map(|a| account_record(id, a)))
    }

    async fn get_account_by_username(&self, username: &str) -> Result<Option<AccountRecord>> {
        let normalized = username.trim().to_ascii_lowercase();
        Ok(self
            .accounts
            .read()
            .unwrap()
            .iter()
            .find(|(_, a)| a.username == normalized)
            .map(|(id, a)| account_record(id, a)))
    }

    async fn password_hash_of(&self, username: &str) -> Result<Option<String>> {
        let normalized = username.trim().to_ascii_lowercase();
        Ok(self
            .accounts
            .read()
            .unwrap()
            .values()
            .find(|a| a.username == normalized)
            .and_then(|a| a.password_hash.clone()))
    }

    async fn set_account_password(&self, id: &AccountId, password_hash: &str) -> Result<()> {
        let mut w = self.accounts.write().unwrap();
        match w.get_mut(id) {
            Some(a) => {
                a.password_hash = Some(password_hash.to_string());
                Ok(())
            }
            None => Err(HubError::NotFound),
        }
    }

    async fn set_account_status(&self, id: &AccountId, status: &str) -> Result<()> {
        let mut w = self.accounts.write().unwrap();
        match w.get_mut(id) {
            Some(a) => {
                a.status = status.to_string();
                Ok(())
            }
            None => Err(HubError::NotFound),
        }
    }

    async fn list_accounts(&self) -> Result<Vec<AccountRecord>> {
        let mut out: Vec<AccountRecord> = self
            .accounts
            .read()
            .unwrap()
            .iter()
            .map(|(id, a)| account_record(id, a))
            .collect();
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(out)
    }

    async fn create_session(
        &self,
        token_hash: &[u8],
        account_id: &AccountId,
        expires_at: DateTime<Utc>,
    ) -> Result<()> {
        self.sessions.write().unwrap().insert(
            token_hash.to_vec(),
            SessionRecord {
                account_id: *account_id,
                expires_at,
            },
        );
        Ok(())
    }

    async fn get_session(&self, token_hash: &[u8]) -> Result<Option<SessionRecord>> {
        Ok(self.sessions.read().unwrap().get(token_hash).cloned())
    }

    async fn delete_session(&self, token_hash: &[u8]) -> Result<()> {
        self.sessions.write().unwrap().remove(token_hash);
        Ok(())
    }

    async fn prune_sessions(&self, now: DateTime<Utc>) -> Result<usize> {
        let mut w = self.sessions.write().unwrap();
        let before = w.len();
        w.retain(|_, s| s.expires_at > now);
        Ok(before - w.len())
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
mod tests {
    use super::*;
    use mem_core::{
        AccountStore, BackupStore, GrantStore, IntegrationStore, MemoryHubStore, MemoryStore,
    };

    fn entry(ns: &str, content: &str, key: Option<&str>) -> MemoryEntry {
        MemoryEntry {
            id: Uuid::new_v4(),
            namespace: ns.to_string(),
            key: key.map(str::to_string),
            kind: MemoryKind::Fact,
            content: content.to_string(),
            tags: vec![],
            metadata: serde_json::json!({}),
            embedding: None,
            embedding_model: None,
            source_agent: "agent-a".to_string(),
            confidence: 1.0,
            status: MemoryStatus::Active,
            version: 1,
            expires_at: None,
            merged_into: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn test_namespace_isolation() {
        let store = InMemoryHubStore::new();
        let a = entry("ns-a", "A 的记忆", None);
        store.insert(&a).await.unwrap();

        // 命名空间不对 → NotFound
        assert!(store.get("ns-b", a.id).await.is_err());
        // 命名空间对 → 命中
        assert!(store.get("ns-a", a.id).await.is_ok());
    }

    #[tokio::test]
    async fn test_key_lookup_and_conflict() {
        let store = InMemoryHubStore::new();
        let a = entry("ns", "第一条", Some("k:1"));
        store.insert(&a).await.unwrap();

        assert!(store.get_by_key("ns", "k:1").await.unwrap().is_some());

        // 同 id 重复插入 → Conflict
        let err = store.insert(&a).await.unwrap_err();
        assert!(matches!(err, HubError::Conflict));
    }

    #[tokio::test]
    async fn test_cursor_pagination_and_limit() {
        let store = InMemoryHubStore::new();
        let mut ids = Vec::new();
        for i in 0..5 {
            let mut e = entry("ns", &format!("条目 {}", i), None);
            e.updated_at = Utc::now() + chrono::Duration::seconds(i);
            ids.push(e.id);
            store.insert(&e).await.unwrap();
        }

        // 第一页：取最新 2 条
        let page1 = store
            .list(&MemoryFilter {
                namespace: Some("ns".to_string()),
                limit: Some(2),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page1.len(), 2);
        // 最新在前
        assert!(page1[0].updated_at >= page1[1].updated_at);

        // 第二页：严格在游标之前
        let cursor_ts = page1[1].updated_at;
        let cursor_id = page1[1].id;
        let page2 = store
            .list(&MemoryFilter {
                namespace: Some("ns".to_string()),
                limit: Some(2),
                before: Some((cursor_ts, cursor_id)),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page2.len(), 2);
        assert!(page2.iter().all(|e| e.id != cursor_id));
    }

    #[tokio::test]
    async fn test_similar_filters_status_and_dim() {
        let store = InMemoryHubStore::new();
        let mut e = entry("ns", "有向量的条目", None);
        e.embedding = Some(vec![1.0, 0.0]);
        store.insert(&e).await.unwrap();

        let hit = store.similar("ns", &[1.0, 0.0], 5, None).await.unwrap();
        assert_eq!(hit.len(), 1);
        assert!((hit[0].score - 1.0).abs() < 1e-6);

        // 维度不匹配 → 无结果
        let miss = store
            .similar("ns", &[1.0, 0.0, 0.0], 5, None)
            .await
            .unwrap();
        assert!(miss.is_empty());

        // 归档后不再召回
        e.status = MemoryStatus::Archived;
        store.save(&e).await.unwrap();
        let empty = store.similar("ns", &[1.0, 0.0], 5, None).await.unwrap();
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn test_versions_idempotent_and_sorted() {
        let store = InMemoryHubStore::new();
        let e = entry("ns", "内容", None);
        store.insert(&e).await.unwrap();

        let mk = |v: i64| MemoryVersion {
            memory_id: e.id,
            version: v,
            content: format!("v{}", v),
            tags: vec![],
            metadata: serde_json::json!({}),
            confidence: 1.0,
            changed_by: "agent-a".to_string(),
            change_type: VersionChange::Updated,
            changed_at: Utc::now(),
        };
        store.record_version(&mk(1)).await.unwrap();
        store.record_version(&mk(2)).await.unwrap();
        // 重放 v1 不得重复入账
        store.record_version(&mk(1)).await.unwrap();

        let vs = store.versions(e.id).await.unwrap();
        assert_eq!(vs.len(), 2);
        assert_eq!(vs[0].version, 1);
        assert_eq!(vs[1].version, 2);
    }

    #[tokio::test]
    async fn test_archive_expired() {
        let store = InMemoryHubStore::new();
        let mut e = entry("ns", "将过期", None);
        e.expires_at = Some(Utc::now() - chrono::Duration::hours(1));
        store.insert(&e).await.unwrap();

        let archived = store.archive_expired(Utc::now()).await.unwrap();
        assert_eq!(archived, 1);
        assert_eq!(
            store.get("ns", e.id).await.unwrap().status,
            MemoryStatus::Archived
        );
        // 版本历史记录了归档事件
        let vs = store.versions(e.id).await.unwrap();
        assert!(vs.iter().any(|v| v.change_type == VersionChange::Archived));
    }

    #[tokio::test]
    async fn test_namespace_grants_and_wildcard_disabled() {
        let store = InMemoryHubStore::new();
        store
            .ensure_namespace("ns", &"agent-a".to_string())
            .await
            .unwrap();

        // owner 尚未被授权：不可读写
        assert!(!store.can_read("ns", &"agent-a".to_string()).await.unwrap());

        store
            .grant(&NamespaceGrantRecord {
                namespace: "ns".to_string(),
                agent_id: "agent-a".to_string(),
                can_read: true,
                can_write: true,
                granted_by: "agent-a".to_string(),
                created_at: Utc::now(),
            })
            .await
            .unwrap();
        assert!(store.can_read("ns", &"agent-a".to_string()).await.unwrap());
        assert!(store.can_write("ns", &"agent-a".to_string()).await.unwrap());

        // 沙箱语义：`*` 通配授权行不再对任何命名空间生效
        store
            .grant(&NamespaceGrantRecord {
                namespace: "*".to_string(),
                agent_id: "agent-b".to_string(),
                can_read: true,
                can_write: true,
                granted_by: "agent-a".to_string(),
                created_at: Utc::now(),
            })
            .await
            .unwrap();
        assert!(!store
            .can_read("any-ns", &"agent-b".to_string())
            .await
            .unwrap());
        assert!(!store
            .can_write("ns", &"agent-b".to_string())
            .await
            .unwrap());

        // 撤销后失效
        store.revoke("ns", &"agent-a".to_string()).await.unwrap();
        assert!(!store.can_write("ns", &"agent-a".to_string()).await.unwrap());

        let owned = store
            .owned_namespaces(&"agent-a".to_string())
            .await
            .unwrap();
        assert_eq!(owned, vec!["ns".to_string()]);
    }

    #[tokio::test]
    async fn test_accounts_and_sessions_roundtrip() {
        let store = InMemoryHubStore::new();

        let acct = store
            .create_account("Alice", Some("hash-a"), "爱丽丝")
            .await
            .unwrap();
        assert_eq!(acct.username, "alice", "用户名归一化为小写");
        assert!(acct.has_password);

        // 用户名冲突（大小写不敏感）
        assert!(store
            .create_account("ALICE", Some("x"), "")
            .await
            .is_err());

        // 按用户名查哈希（认证路径）
        assert_eq!(
            store.password_hash_of("aLiCe").await.unwrap().as_deref(),
            Some("hash-a")
        );

        // 引导账户幂等（按固定 ID）
        let boot1 = store
            .ensure_account(&common::BOOTSTRAP_ACCOUNT_ID, "bootstrap", "引导账户")
            .await
            .unwrap();
        let boot2 = store
            .ensure_account(&common::BOOTSTRAP_ACCOUNT_ID, "bootstrap", "引导账户")
            .await
            .unwrap();
        assert_eq!(boot1.id, boot2.id);
        assert!(!boot1.has_password, "引导账户初始无口令");

        // 会话：创建 → 读取 → 过期清理 → 删除
        let hash = vec![7u8; 32];
        let future = Utc::now() + chrono::Duration::hours(1);
        store
            .create_session(&hash, &acct.id, future)
            .await
            .unwrap();
        assert_eq!(
            store.get_session(&hash).await.unwrap().unwrap().account_id,
            acct.id
        );
        assert_eq!(store.prune_sessions(Utc::now()).await.unwrap(), 0);
        assert_eq!(
            store
                .prune_sessions(future + chrono::Duration::seconds(1))
                .await
                .unwrap(),
            1
        );
        assert!(store.get_session(&hash).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_backup_and_proposal_roundtrip() {
        let store = InMemoryHubStore::new();
        let backup = BackupRecord {
            id: Uuid::new_v4(),
            namespace: "ns".to_string(),
            created_by: "agent-a".to_string(),
            note: None,
            entry_count: 0,
            checksum: "abc".to_string(),
            payload: serde_json::json!({}),
            retain_until: None,
            created_at: Utc::now(),
        };
        store.save_backup(&backup).await.unwrap();
        assert_eq!(store.list_backups(None).await.unwrap().len(), 1);
        assert_eq!(store.list_backups(Some("ns")).await.unwrap().len(), 1);
        assert_eq!(store.list_backups(Some("other")).await.unwrap().len(), 0);
        store.delete_backup(backup.id).await.unwrap();
        assert!(store.list_backups(None).await.unwrap().is_empty());

        let proposal = IntegrationProposal {
            id: Uuid::new_v4(),
            namespace: "ns".to_string(),
            kind: mem_core::IntegrationKind::Duplicate,
            member_ids: vec![Uuid::new_v4(), Uuid::new_v4()],
            suggested_survivor: None,
            similarity: 0.99,
            status: ProposalStatus::Open,
            resolution_note: None,
            created_at: Utc::now(),
            resolved_at: None,
        };
        store.save_proposal(&proposal).await.unwrap();
        assert!(store
            .open_exists(
                "ns",
                &[("duplicate".to_string(), proposal.member_ids.clone())]
            )
            .await
            .unwrap());
        assert_eq!(
            store
                .list_proposals("ns", Some(ProposalStatus::Open))
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn test_hub_store_aggregation() {
        // 编译期即验证：InMemoryHubStore 实现全部四个 trait
        fn assert_impl<T: MemoryHubStore>() {}
        assert_impl::<InMemoryHubStore>();
    }
}
