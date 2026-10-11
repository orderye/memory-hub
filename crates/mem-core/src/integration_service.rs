//! 整合服务：跨 Agent 写入的去重与冲突归并
//!
//! 多 Agent 并发写入同一命名空间时，不可避免出现：
//! - **重复**（duplicate）：内容高度相似（余弦相似度 ≥ 阈值）的条目；
//! - **冲突**（conflict）：同一业务 key 下内容不一致的条目。
//!
//! 本服务只做"识别 + 提案 + 按提案归并"：
//! 扫描生成提案（幂等，不重复刷屏），由 Agent 显式裁决
//! （merge 合并 / dismiss 保留分立），所有被合并条目保留 `merged_into`
//! 溯源，绝不静默删数据。

use crate::account_scope::AccountScopedStore;
use crate::algorithm::cosine_similarity;
use crate::domain::{
    IntegrationKind, IntegrationProposal, MemoryEntry, MemoryStatus, ProposalStatus, VersionChange,
};
use crate::infra::{
    AccessChecker, Clock, IntegrationStore, MemoryHubStore, MemoryStore, SystemClock,
};
use async_trait::async_trait;
use common::{AuthCtx, HubError, Result, Scope};
use serde::{Deserialize, Serialize};
use std::collections::HashMap as StdHashMap;
use std::sync::Arc;
use uuid::Uuid;

/// 扫描请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanRequest {
    pub namespace: String,
    /// true = 只预演不落提案
    #[serde(default)]
    pub dry_run: bool,
    /// 覆盖默认阈值（None 用服务配置）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sim_threshold: Option<f32>,
}

/// 扫描报告
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanReport {
    pub namespace: String,
    pub scanned: usize,
    pub dry_run: bool,
    /// 本次新建的提案（dry_run 时为预演结果）
    pub proposals: Vec<IntegrationProposal>,
    /// 因已有 open 提案而跳过的组数
    pub skipped_existing: usize,
}

/// 提案裁决动作
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveAction {
    /// 合并：保留 survivor，其余成员标记 merged 并指向 survivor
    Merge,
    /// 驳回：保留分立（后续扫描仍可再次提案）
    Dismiss,
}

/// 裁决请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolveRequest {
    pub proposal_id: Uuid,
    pub action: ResolveAction,
    /// merge 时的幸存条目（None 用提案建议值；提案无建议值时取最新成员）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub survivor_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// 整合服务
#[async_trait]
pub trait IntegrationService: Send + Sync {
    async fn scan(&self, auth: &AuthCtx, req: ScanRequest) -> Result<ScanReport>;
    async fn list(&self, auth: &AuthCtx, namespace: &str) -> Result<Vec<IntegrationProposal>>;
    async fn resolve(&self, auth: &AuthCtx, req: ResolveRequest) -> Result<MemoryEntry>;
    /// 归档过期条目（worker 定期调用）
    async fn expire_stale(&self, now: chrono::DateTime<chrono::Utc>) -> Result<usize>;
}

/// 整合服务实现
pub struct DefaultIntegrationService {
    store: Arc<dyn MemoryHubStore>,
    access: Arc<dyn AccessChecker>,
    clock: Arc<dyn Clock>,
    sim_threshold: f32,
    max_proposals: usize,
}

impl DefaultIntegrationService {
    pub fn new(
        store: Arc<dyn MemoryHubStore>,
        access: Arc<dyn AccessChecker>,
        sim_threshold: f32,
        max_proposals: usize,
    ) -> Self {
        Self {
            store,
            access,
            clock: Arc::new(SystemClock),
            sim_threshold: sim_threshold.clamp(0.0, 1.0),
            max_proposals: max_proposals.max(1),
        }
    }

    #[cfg(test)]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// 本请求的账户作用域存储
    fn scoped(&self, auth: &AuthCtx) -> AccountScopedStore<dyn MemoryHubStore> {
        AccountScopedStore::new(self.store.clone(), auth.account_id)
    }

    async fn require_read(&self, auth: &AuthCtx, namespace: &str) -> Result<()> {
        if !auth.has_scope(&Scope::Read) {
            return Err(HubError::InsufficientScope);
        }
        if !self.access.can_read(auth, namespace).await? {
            return Err(HubError::InsufficientScope);
        }
        Ok(())
    }

    async fn require_write(&self, auth: &AuthCtx, namespace: &str) -> Result<()> {
        if !auth.has_scope(&Scope::Write) {
            return Err(HubError::InsufficientScope);
        }
        if !self.access.can_write(auth, namespace).await? {
            return Err(HubError::InsufficientScope);
        }
        Ok(())
    }

    /// 记录一条版本历史
    async fn record_version(
        &self,
        entry: &MemoryEntry,
        change: VersionChange,
        agent: &str,
    ) -> Result<()> {
        self.store
            .record_version(&crate::domain::MemoryVersion {
                memory_id: entry.id,
                version: entry.version,
                content: entry.content.clone(),
                tags: entry.tags.clone(),
                metadata: entry.metadata.clone(),
                confidence: entry.confidence,
                changed_by: agent.to_string(),
                change_type: change,
                changed_at: entry.updated_at,
            })
            .await
    }
}

/// 简易并查集（用于把高相似条目聚成组）
struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }

    fn find(&mut self, x: usize) -> usize {
        let mut root = x;
        while self.parent[root] != root {
            root = self.parent[root];
        }
        // 路径压缩
        let mut cur = x;
        while self.parent[cur] != root {
            let next = self.parent[cur];
            self.parent[cur] = root;
            cur = next;
        }
        root
    }

    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.parent[rb] = ra;
        }
    }
}

#[async_trait]
impl IntegrationService for DefaultIntegrationService {
    async fn scan(&self, auth: &AuthCtx, req: ScanRequest) -> Result<ScanReport> {
        // 扫描会写提案，按写权限校验
        self.require_write(auth, &req.namespace).await?;
        let threshold = req.sim_threshold.unwrap_or(self.sim_threshold);
        if !(0.0..=1.0).contains(&threshold) {
            return Err(HubError::InvalidArgument(
                "sim_threshold 必须落在 [0,1]".to_string(),
            ));
        }
        let store = self.scoped(auth);

        let entries: Vec<MemoryEntry> = store
            .all_in_namespace(&req.namespace, false)
            .await?
            .into_iter()
            .filter(|e| e.status == MemoryStatus::Active)
            .collect();

        let mut report = ScanReport {
            namespace: req.namespace.clone(),
            scanned: entries.len(),
            dry_run: req.dry_run,
            proposals: Vec::new(),
            skipped_existing: 0,
        };
        if entries.len() < 2 {
            return Ok(report);
        }

        // ===== 1. 重复聚类：同 kind 且嵌入余弦 ≥ 阈值 =====
        let mut uf = UnionFind::new(entries.len());
        let mut best_sim = StdHashMap::<(usize, usize), f32>::new();
        for i in 0..entries.len() {
            for j in (i + 1)..entries.len() {
                if entries[i].kind != entries[j].kind {
                    continue;
                }
                let (Some(a), Some(b)) =
                    (entries[i].embedding.as_ref(), entries[j].embedding.as_ref())
                else {
                    continue;
                };
                let sim = cosine_similarity(a, b);
                if sim >= threshold {
                    uf.union(i, j);
                    best_sim.insert((i, j), sim);
                }
            }
        }

        // 分组：root → 成员索引
        let mut groups: StdHashMap<usize, Vec<usize>> = StdHashMap::new();
        for i in 0..entries.len() {
            let root = uf.find(i);
            groups.entry(root).or_default().push(i);
        }

        // 组内最高相似度
        let group_max_sim = |members: &[usize]| -> f32 {
            let mut max = 0.0f32;
            for (a, b) in best_sim.keys() {
                if members.contains(a) && members.contains(b) {
                    max = max.max(best_sim[&(*a, *b)]);
                }
            }
            max
        };

        // ===== 2. 冲突识别：同 key 内容不同 =====
        let mut by_key: StdHashMap<&str, Vec<usize>> = StdHashMap::new();
        for (i, e) in entries.iter().enumerate() {
            if let Some(k) = e.key.as_deref() {
                by_key.entry(k).or_default().push(i);
            }
        }

        let mut new_proposals: Vec<IntegrationProposal> = Vec::new();
        let mut dedupe_keys: Vec<(String, Vec<Uuid>)> = Vec::new();

        // 重复组（组内 ≥ 2 个成员且不属于同一 key 的冲突组，避免重复提案）
        let mut conflict_members: std::collections::HashSet<usize> =
            std::collections::HashSet::new();
        for members in by_key.values() {
            if members.len() >= 2 {
                // 内容确实不同才算冲突
                let distinct: std::collections::HashSet<&str> = members
                    .iter()
                    .map(|&i| entries[i].content.as_str())
                    .collect();
                if distinct.len() >= 2 {
                    for &i in members {
                        conflict_members.insert(i);
                    }
                    let sorted = sorted_by_recency(members, &entries);
                    let ids: Vec<Uuid> = sorted.iter().map(|&i| entries[i].id).collect();
                    dedupe_keys.push(("conflict".to_string(), ids.clone()));
                    new_proposals.push(IntegrationProposal {
                        id: Uuid::new_v4(),
                        namespace: req.namespace.clone(),
                        kind: IntegrationKind::Conflict,
                        member_ids: ids,
                        suggested_survivor: Some(entries[sorted[0]].id),
                        similarity: 0.0,
                        status: ProposalStatus::Open,
                        resolution_note: None,
                        created_at: self.clock.now(),
                        resolved_at: None,
                    });
                }
            }
        }

        // 重复组（剔除已进入冲突组的成员）
        for (_root, members) in groups {
            let members: Vec<usize> = members
                .into_iter()
                .filter(|i| !conflict_members.contains(i))
                .collect();
            if members.len() < 2 {
                continue;
            }
            let sorted = sorted_by_recency(&members, &entries);
            let ids: Vec<Uuid> = sorted.iter().map(|&i| entries[i].id).collect();
            dedupe_keys.push(("duplicate".to_string(), ids.clone()));
            new_proposals.push(IntegrationProposal {
                id: Uuid::new_v4(),
                namespace: req.namespace.clone(),
                kind: IntegrationKind::Duplicate,
                member_ids: ids,
                suggested_survivor: Some(entries[sorted[0]].id),
                similarity: group_max_sim(&sorted),
                status: ProposalStatus::Open,
                resolution_note: None,
                created_at: self.clock.now(),
                resolved_at: None,
            });
        }

        // 按创建顺序截断到上限
        new_proposals.truncate(self.max_proposals);

        if req.dry_run {
            report.proposals = new_proposals;
            return Ok(report);
        }

        // 落库前查重：跳过已有 open 提案的组
        for proposal in new_proposals {
            let exists = store
                .open_exists(
                    &req.namespace,
                    std::slice::from_ref(&(
                        proposal.kind.as_str().to_string(),
                        proposal.member_ids.clone(),
                    )),
                )
                .await?;
            if exists {
                report.skipped_existing += 1;
                continue;
            }
            store.save_proposal(&proposal).await?;
            report.proposals.push(proposal);
        }

        Ok(report)
    }

    async fn list(&self, auth: &AuthCtx, namespace: &str) -> Result<Vec<IntegrationProposal>> {
        self.require_read(auth, namespace).await?;
        self.scoped(auth).list_proposals(namespace, None).await
    }

    async fn resolve(&self, auth: &AuthCtx, req: ResolveRequest) -> Result<MemoryEntry> {
        let store = self.scoped(auth);
        let mut proposal = store.get_proposal(req.proposal_id).await?;
        if proposal.status != ProposalStatus::Open {
            return Err(HubError::Conflict);
        }
        self.require_write(auth, &proposal.namespace).await?;

        // 载入全部成员（缺失成员跳过，不影响其余合并）
        let mut members = Vec::new();
        for id in &proposal.member_ids {
            if let Ok(e) = store.get(&proposal.namespace, *id).await {
                members.push(e);
            }
        }
        if members.is_empty() {
            return Err(HubError::NotFound);
        }

        let now = self.clock.now();

        // 确定幸存者
        let survivor_id = match req.action {
            ResolveAction::Merge => {
                let sid = req
                    .survivor_id
                    .or(proposal.suggested_survivor)
                    .ok_or_else(|| {
                        HubError::InvalidArgument(
                            "提案无建议幸存者，必须显式提供 survivor_id".to_string(),
                        )
                    })?;
                if !members.iter().any(|m| m.id == sid) {
                    return Err(HubError::InvalidArgument(
                        "survivor_id 必须是提案成员之一".to_string(),
                    ));
                }
                sid
            }
            ResolveAction::Dismiss => {
                proposal.status = ProposalStatus::Dismissed;
                proposal.resolution_note = req.note.clone();
                proposal.resolved_at = Some(now);
                store.save_proposal(&proposal).await?;
                return Ok(members.into_iter().next().unwrap());
            }
        };

        // 合并：幸存者吸收全部成员的标签（内容不变），其余成员标记 merged
        let mut survivor = members
            .iter()
            .find(|m| m.id == survivor_id)
            .cloned()
            .unwrap();

        // 标签取并集（保序去重）
        let mut union_tags = survivor.tags.clone();
        for m in &members {
            if m.id == survivor_id {
                continue;
            }
            for t in &m.tags {
                if !union_tags.contains(t) {
                    union_tags.push(t.clone());
                }
            }
        }
        survivor.tags = union_tags;
        survivor.version += 1;
        survivor.updated_at = now;
        survivor.source_agent = auth.agent_id.clone();
        let saved = store.save(&survivor).await?;
        self.record_version(&saved, VersionChange::Updated, &auth.agent_id)
            .await?;

        for m in members.iter().filter(|m| m.id != survivor_id) {
            let mut archived = m.clone();
            archived.status = MemoryStatus::Merged;
            archived.merged_into = Some(survivor_id);
            archived.version += 1;
            archived.updated_at = now;
            archived.source_agent = auth.agent_id.clone();
            let saved_archived = store.save(&archived).await?;
            self.record_version(&saved_archived, VersionChange::Merged, &auth.agent_id)
                .await?;
        }

        proposal.status = ProposalStatus::Merged;
        proposal.resolution_note = req.note.clone();
        proposal.resolved_at = Some(now);
        store.save_proposal(&proposal).await?;

        Ok(saved)
    }

    async fn expire_stale(&self, now: chrono::DateTime<chrono::Utc>) -> Result<usize> {
        // 系统级批处理（worker 调用，无请求账户）：对全库统一归档过期条目
        self.store.archive_expired(now).await
    }
}

/// 按 updated_at 降序排列成员索引（最新者优先作为建议幸存者）
fn sorted_by_recency(members: &[usize], entries: &[MemoryEntry]) -> Vec<usize> {
    let mut sorted = members.to_vec();
    sorted.sort_by(|&a, &b| entries[b].updated_at.cmp(&entries[a].updated_at));
    sorted
}

#[cfg(test)]
mod tests {
    use super::*;
    /// 测试账户作用域名：与 `auth()`/`writer()` 辅助中的账户一致
    fn scoped_ns(ns: &str) -> String {
        crate::account_scope::scope_name(&common::BOOTSTRAP_ACCOUNT_ID, ns)
    }
    use crate::access::GrantChecker;
    use crate::algorithm::l2_normalize;
    use crate::domain::{
        BackupRecord, MemoryFilter, MemoryKind, MemoryStatus, MemoryVersion, NamespaceGrantRecord,
        NamespaceInfo,
    };
    use crate::infra::{GrantStore, MemoryStore};
    use common::AgentId;
    use common::Scope;
    use std::collections::HashMap as StdHashMap;
    use std::sync::RwLock;

    /// 测试用全功能存储
    #[derive(Default)]
    struct TestHubStore {
        items: RwLock<StdHashMap<(String, Uuid), MemoryEntry>>,
        proposals: RwLock<StdHashMap<Uuid, IntegrationProposal>>,
        namespaces: RwLock<StdHashMap<String, NamespaceInfo>>,
        grants: RwLock<Vec<NamespaceGrantRecord>>,
        versions: RwLock<Vec<MemoryVersion>>,
    }

    impl TestHubStore {
        fn add(&self, entry: MemoryEntry) -> MemoryEntry {
            self.items
                .write()
                .unwrap()
                .insert((entry.namespace.clone(), entry.id), entry.clone());
            entry
        }

        fn make_entry(ns: &str, content: &str, key: Option<&str>) -> MemoryEntry {
            Self::make_entry_with_embedding(ns, content, key, None)
        }

        fn make_entry_with_embedding(
            ns: &str,
            content: &str,
            key: Option<&str>,
            embedding: Option<Vec<f32>>,
        ) -> MemoryEntry {
            MemoryEntry {
                id: Uuid::new_v4(),
                namespace: ns.to_string(),
                key: key.map(str::to_string),
                kind: MemoryKind::Fact,
                content: content.to_string(),
                tags: vec![],
                metadata: serde_json::json!({}),
                embedding,
                embedding_model: None,
                source_agent: "agent-a".to_string(),
                confidence: 1.0,
                status: MemoryStatus::Active,
                version: 1,
                expires_at: None,
                merged_into: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            }
        }
    }

    #[async_trait]
    impl MemoryStore for TestHubStore {
        async fn insert(&self, e: &MemoryEntry) -> Result<MemoryEntry> {
            Ok(self.add(e.clone()))
        }
        async fn get(&self, ns: &str, id: Uuid) -> Result<MemoryEntry> {
            self.items
                .read()
                .unwrap()
                .get(&(ns.to_string(), id))
                .cloned()
                .ok_or(HubError::NotFound)
        }
        async fn get_by_key(&self, ns: &str, key: &str) -> Result<Option<MemoryEntry>> {
            Ok(self
                .items
                .read()
                .unwrap()
                .values()
                .find(|e| e.namespace == ns && e.key.as_deref() == Some(key))
                .cloned())
        }
        async fn save(&self, e: &MemoryEntry) -> Result<MemoryEntry> {
            Ok(self.add(e.clone()))
        }
        async fn delete(&self, ns: &str, id: Uuid) -> Result<()> {
            self.items.write().unwrap().remove(&(ns.to_string(), id));
            Ok(())
        }
        async fn list(&self, _filter: &MemoryFilter) -> Result<Vec<MemoryEntry>> {
            Ok(self.items.read().unwrap().values().cloned().collect())
        }
        async fn count(&self, _filter: &MemoryFilter) -> Result<usize> {
            Ok(self.items.read().unwrap().len())
        }
        async fn similar(
            &self,
            _ns: &str,
            _qv: &[f32],
            _k: usize,
            _kinds: Option<&[MemoryKind]>,
        ) -> Result<Vec<crate::domain::Scored<MemoryEntry>>> {
            Ok(vec![])
        }
        async fn all_in_namespace(
            &self,
            ns: &str,
            include_merged: bool,
        ) -> Result<Vec<MemoryEntry>> {
            Ok(self
                .items
                .read()
                .unwrap()
                .values()
                .filter(|e| e.namespace == ns)
                .filter(|e| include_merged || e.status != MemoryStatus::Merged)
                .cloned()
                .collect())
        }
        async fn list_namespaces(&self) -> Result<Vec<String>> {
            Ok(vec![])
        }
        async fn record_version(&self, v: &MemoryVersion) -> Result<()> {
            self.versions.write().unwrap().push(v.clone());
            Ok(())
        }
        async fn versions(&self, id: Uuid) -> Result<Vec<MemoryVersion>> {
            Ok(self
                .versions
                .read()
                .unwrap()
                .iter()
                .filter(|v| v.memory_id == id)
                .cloned()
                .collect())
        }
        async fn archive_expired(&self, now: chrono::DateTime<chrono::Utc>) -> Result<usize> {
            let mut count = 0;
            let mut guard = self.items.write().unwrap();
            for e in guard.values_mut() {
                if e.status == MemoryStatus::Active {
                    if let Some(exp) = e.expires_at {
                        if exp <= now {
                            e.status = MemoryStatus::Archived;
                            count += 1;
                        }
                    }
                }
            }
            Ok(count)
        }
    }

    #[async_trait]
    impl crate::infra::BackupStore for TestHubStore {
        async fn save_backup(&self, _b: &BackupRecord) -> Result<()> {
            Ok(())
        }
        async fn get_backup(&self, _id: Uuid) -> Result<BackupRecord> {
            Err(HubError::NotFound)
        }
        async fn list_backups(&self, _ns: Option<&str>) -> Result<Vec<BackupRecord>> {
            Ok(vec![])
        }
        async fn delete_backup(&self, _id: Uuid) -> Result<()> {
            Ok(())
        }
    }

    #[async_trait]
    impl crate::infra::IntegrationStore for TestHubStore {
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
            ns: &str,
            status: Option<ProposalStatus>,
        ) -> Result<Vec<IntegrationProposal>> {
            Ok(self
                .proposals
                .read()
                .unwrap()
                .values()
                .filter(|p| p.namespace == ns)
                .filter(|p| status.is_none_or(|s| p.status == s))
                .cloned()
                .collect())
        }
        async fn open_exists(&self, _ns: &str, members: &[(String, Vec<Uuid>)]) -> Result<bool> {
            let guard = self.proposals.read().unwrap();
            Ok(guard.values().any(|p| {
                p.status == ProposalStatus::Open
                    && members
                        .iter()
                        .any(|(kind, ids)| kind == p.kind.as_str() && &p.member_ids == ids)
            }))
        }
    }

    #[async_trait]
    impl crate::infra::GrantStore for TestHubStore {
        async fn ensure_namespace(
            &self,
            ns: &str,
            owner: &AgentId,
        ) -> Result<(NamespaceInfo, bool)> {
            let mut created = false;
            let info = self
                .namespaces
                .write()
                .unwrap()
                .entry(ns.to_string())
                .or_insert_with(|| {
                    created = true;
                    NamespaceInfo {
                        namespace: ns.to_string(),
                        owner_agent: owner.to_string(),
                        created_at: chrono::Utc::now(),
                    }
                })
                .clone();
            Ok((info, created))
        }
        async fn get_namespace(&self, ns: &str) -> Result<Option<NamespaceInfo>> {
            Ok(self.namespaces.read().unwrap().get(ns).cloned())
        }
        async fn list_namespace_infos(&self) -> Result<Vec<NamespaceInfo>> {
            Ok(self.namespaces.read().unwrap().values().cloned().collect())
        }
        async fn grant(&self, r: &NamespaceGrantRecord) -> Result<()> {
            let mut g = self.grants.write().unwrap();
            g.retain(|x| !(x.namespace == r.namespace && x.agent_id == r.agent_id));
            g.push(r.clone());
            Ok(())
        }
        async fn revoke(&self, ns: &str, agent: &AgentId) -> Result<()> {
            self.grants
                .write()
                .unwrap()
                .retain(|x| !(x.namespace == ns && *agent == x.agent_id));
            Ok(())
        }
        async fn grants_for(&self, agent: &AgentId) -> Result<Vec<NamespaceGrantRecord>> {
            Ok(self
                .grants
                .read()
                .unwrap()
                .iter()
                .filter(|x| &x.agent_id == agent)
                .cloned()
                .collect())
        }
        async fn can_read(&self, ns: &str, agent: &AgentId) -> Result<bool> {
            Ok(self.grants.read().unwrap().iter().any(|x| {
                (x.namespace == ns || x.namespace == "*") && x.agent_id == *agent && x.can_read
            }))
        }
        async fn can_write(&self, ns: &str, agent: &AgentId) -> Result<bool> {
            Ok(self.grants.read().unwrap().iter().any(|x| {
                (x.namespace == ns || x.namespace == "*") && x.agent_id == *agent && x.can_write
            }))
        }
        async fn owned_namespaces(&self, agent: &AgentId) -> Result<Vec<String>> {
            Ok(self
                .namespaces
                .read()
                .unwrap()
                .values()
                .filter(|n| n.owner_agent == *agent)
                .map(|n| n.namespace.clone())
                .collect())
        }
    }

    #[async_trait]
    impl crate::infra::AgentStore for TestHubStore {
        async fn register_agent(
            &self,
            _reg: &crate::infra::AgentRegistration,
        ) -> Result<crate::infra::AgentRecord> {
            Err(HubError::Unavailable)
        }
        async fn get_agent(
            &self,
            _agent_id: &AgentId,
        ) -> Result<Option<crate::infra::AgentRecord>> {
            Ok(None)
        }
        async fn list_agents(&self) -> Result<Vec<crate::infra::AgentRecord>> {
            Ok(vec![])
        }
        async fn set_agent_status(&self, _agent_id: &AgentId, _status: &str) -> Result<()> {
            Err(HubError::Unavailable)
        }
        async fn verify_agent_secret(&self, _agent_id: &AgentId, _secret: &str) -> Result<bool> {
            Ok(false)
        }
    }

    fn writer() -> AuthCtx {
        AuthCtx {
            agent_id: "agent-a".to_string(),
            account_id: common::BOOTSTRAP_ACCOUNT_ID,
            scopes: vec![Scope::Read, Scope::Write],
            acl: Default::default(),
        }
    }

    async fn setup() -> (DefaultIntegrationService, Arc<TestHubStore>) {
        let store = Arc::new(TestHubStore::default());
        store
            .ensure_namespace(&scoped_ns("ns"), &"agent-a".to_string())
            .await
            .unwrap();
        store
            .grant(&NamespaceGrantRecord {
                namespace: scoped_ns("ns"),
                agent_id: "agent-a".to_string(),
                can_read: true,
                can_write: true,
                granted_by: "agent-a".to_string(),
                created_at: chrono::Utc::now(),
            })
            .await
            .unwrap();
        let svc = DefaultIntegrationService::new(
            store.clone(),
            Arc::new(GrantChecker::new(store.clone())),
            0.99,
            100,
        );
        (svc, store)
    }

    fn near_identical_vec(seed: &str, jitter: f32) -> Vec<f32> {
        // 以 seed 派生向量，再加微小扰动 → 余弦接近 1
        let mut v: Vec<f32> = (0..8)
            .map(|i| ((seed.len() as f32 + i as f32) * 0.37).sin())
            .collect();
        v[0] += jitter;
        l2_normalize(&mut v);
        v
    }

    #[tokio::test]
    async fn test_scan_detects_duplicates_and_dry_run_does_not_persist() {
        let (svc, store) = setup().await;
        store.add(TestHubStore::make_entry_with_embedding(
            &scoped_ns("ns"),
            "服务器部署在东京",
            None,
            Some(near_identical_vec("x", 0.0)),
        ));
        store.add(TestHubStore::make_entry_with_embedding(
            &scoped_ns("ns"),
            "服务器部署在东京区域",
            None,
            Some(near_identical_vec("x", 0.001)),
        ));

        let report = svc
            .scan(
                &writer(),
                ScanRequest {
                    namespace: "ns".to_string(),
                    dry_run: true,
                    sim_threshold: Some(0.99),
                },
            )
            .await
            .unwrap();
        assert_eq!(report.scanned, 2);
        assert_eq!(report.proposals.len(), 1);
        assert_eq!(report.proposals[0].kind, IntegrationKind::Duplicate);

        // dry_run 不落库
        let persisted = svc.list(&writer(), "ns").await.unwrap();
        assert!(persisted.is_empty());
    }

    #[tokio::test]
    async fn test_scan_detects_key_conflicts() {
        let (svc, store) = setup().await;
        store.add(TestHubStore::make_entry(
            &scoped_ns("ns"),
            "数据库用 MySQL",
            Some("db:engine"),
        ));
        store.add(TestHubStore::make_entry(
            &scoped_ns("ns"),
            "数据库用 PostgreSQL",
            Some("db:engine"),
        ));

        let report = svc
            .scan(
                &writer(),
                ScanRequest {
                    namespace: "ns".to_string(),
                    dry_run: false,
                    sim_threshold: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(report.proposals.len(), 1);
        assert_eq!(report.proposals[0].kind, IntegrationKind::Conflict);
        assert_eq!(report.proposals[0].member_ids.len(), 2);

        // 再次扫描：已存在 open 提案，必须跳过
        let report2 = svc
            .scan(
                &writer(),
                ScanRequest {
                    namespace: "ns".to_string(),
                    dry_run: false,
                    sim_threshold: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(report2.proposals.len(), 0);
        assert_eq!(report2.skipped_existing, 1);
    }

    #[tokio::test]
    async fn test_resolve_merge_archives_members_and_keeps_lineage() {
        let (svc, store) = setup().await;
        let e1 = store.add(TestHubStore::make_entry(
            &scoped_ns("ns"),
            "数据库用 MySQL",
            Some("db:engine"),
        ));
        let e2 = store.add(TestHubStore::make_entry(
            &scoped_ns("ns"),
            "数据库用 PostgreSQL",
            Some("db:engine"),
        ));

        let scan = svc
            .scan(
                &writer(),
                ScanRequest {
                    namespace: "ns".to_string(),
                    dry_run: false,
                    sim_threshold: None,
                },
            )
            .await
            .unwrap();
        let proposal_id = scan.proposals[0].id;

        let survivor = svc
            .resolve(
                &writer(),
                ResolveRequest {
                    proposal_id,
                    action: ResolveAction::Merge,
                    survivor_id: Some(e2.id),
                    note: Some("以新内容为准".to_string()),
                },
            )
            .await
            .unwrap();
        assert_eq!(survivor.id, e2.id);

        let archived = store.get(&scoped_ns("ns"), e1.id).await.unwrap();
        assert_eq!(archived.status, MemoryStatus::Merged);
        assert_eq!(archived.merged_into, Some(e2.id));

        // 提案已关闭
        let proposals = svc.list(&writer(), "ns").await.unwrap();
        assert_eq!(proposals[0].status, ProposalStatus::Merged);
        assert_eq!(
            proposals[0].resolution_note.as_deref(),
            Some("以新内容为准")
        );

        // 重复裁决必须报冲突
        let err = svc
            .resolve(
                &writer(),
                ResolveRequest {
                    proposal_id,
                    action: ResolveAction::Merge,
                    survivor_id: Some(e2.id),
                    note: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, HubError::Conflict));
    }

    #[tokio::test]
    async fn test_resolve_dismiss_keeps_members() {
        let (svc, store) = setup().await;
        let e1 = store.add(TestHubStore::make_entry(
            &scoped_ns("ns"),
            "数据库用 MySQL",
            Some("db:engine"),
        ));
        store.add(TestHubStore::make_entry(
            &scoped_ns("ns"),
            "数据库用 PostgreSQL",
            Some("db:engine"),
        ));

        let scan = svc
            .scan(
                &writer(),
                ScanRequest {
                    namespace: "ns".to_string(),
                    dry_run: false,
                    sim_threshold: None,
                },
            )
            .await
            .unwrap();

        svc.resolve(
            &writer(),
            ResolveRequest {
                proposal_id: scan.proposals[0].id,
                action: ResolveAction::Dismiss,
                survivor_id: None,
                note: Some("两者都要保留".to_string()),
            },
        )
        .await
        .unwrap();

        let e1_after = store.get(&scoped_ns("ns"), e1.id).await.unwrap();
        assert_eq!(e1_after.status, MemoryStatus::Active);
        let proposals = svc.list(&writer(), "ns").await.unwrap();
        assert_eq!(proposals[0].status, ProposalStatus::Dismissed);
    }

    #[tokio::test]
    async fn test_resolve_rejects_non_member_survivor() {
        let (svc, store) = setup().await;
        store.add(TestHubStore::make_entry(&scoped_ns("ns"), "A", Some("k:1")));
        store.add(TestHubStore::make_entry(&scoped_ns("ns"), "B", Some("k:1")));
        let outsider = store.add(TestHubStore::make_entry(&scoped_ns("ns"), "C", Some("k:2")));

        let scan = svc
            .scan(
                &writer(),
                ScanRequest {
                    namespace: "ns".to_string(),
                    dry_run: false,
                    sim_threshold: None,
                },
            )
            .await
            .unwrap();

        let err = svc
            .resolve(
                &writer(),
                ResolveRequest {
                    proposal_id: scan.proposals[0].id,
                    action: ResolveAction::Merge,
                    survivor_id: Some(outsider.id),
                    note: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, HubError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_merge_unions_tags() {
        let (svc, store) = setup().await;
        let mut e1 = TestHubStore::make_entry(&scoped_ns("ns"), "旧内容", Some("k:1"));
        e1.tags = vec!["db".to_string()];
        let e1 = store.add(e1);
        let mut e2 = TestHubStore::make_entry(&scoped_ns("ns"), "新内容", Some("k:1"));
        e2.tags = vec!["pg".to_string()];
        let e2 = store.add(e2);

        let scan = svc
            .scan(
                &writer(),
                ScanRequest {
                    namespace: "ns".to_string(),
                    dry_run: false,
                    sim_threshold: None,
                },
            )
            .await
            .unwrap();

        let survivor = svc
            .resolve(
                &writer(),
                ResolveRequest {
                    proposal_id: scan.proposals[0].id,
                    action: ResolveAction::Merge,
                    survivor_id: Some(e2.id),
                    note: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(survivor.tags, vec!["pg".to_string(), "db".to_string()]);
        let _ = e1; // e1 已被合并
    }

    #[tokio::test]
    async fn test_expire_stale_archives_expired() {
        let (svc, store) = setup().await;
        let mut e = TestHubStore::make_entry(&scoped_ns("ns"), "临时状态", None);
        e.expires_at = Some(chrono::Utc::now() - chrono::Duration::hours(1));
        let id = store.add(e).id;

        let count = svc.expire_stale(chrono::Utc::now()).await.unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            store.get(&scoped_ns("ns"), id).await.unwrap().status,
            MemoryStatus::Archived
        );
    }

    #[tokio::test]
    async fn test_scan_requires_write_scope() {
        let (svc, _store) = setup().await;
        let reader = AuthCtx {
            agent_id: "agent-a".to_string(),
            account_id: common::BOOTSTRAP_ACCOUNT_ID,
            scopes: vec![Scope::Read],
            acl: Default::default(),
        };
        let err = svc
            .scan(
                &reader,
                ScanRequest {
                    namespace: "ns".to_string(),
                    dry_run: false,
                    sim_threshold: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, HubError::InsufficientScope));
    }
}
