//! 账户沙箱作用域：命名空间在存储层按 `{account_id}/{namespace}` 隔离
//!
//! 服务层与 MCP 工具看到的一直是"用户可见"的命名空间名（如 `personal`）；
//! 所有落库/查库的键都携带账户前缀（如 `00000000-…-0001/personal`）。
//! 沙箱由两条不变量保证：
//!
//! 1. 前缀拼接是**单射**：一个账户的令牌永远拼不出另一个账户的存储键
//!    （用户命名空间不允许出现 `/`，见 `access::validate_namespace`）；
//! 2. 授权表内不存在跨账户行：命名空间 owner 与授权目标都取自同账户 Agent。
//!
//! `AccountScopedStore` 是这两条不变量在代码上的唯一落点：服务层为每次
//! 请求构造它，全部数据访问经其代理。前缀拼接对"已带前缀"的名称是幂等的，
//! 因此备份负载（内部存用户可见名）恢复往返不会二次加前缀。

use crate::domain::{
    BackupRecord, IntegrationProposal, MemoryEntry, MemoryFilter, MemoryKind, MemoryVersion,
    Namespace, NamespaceGrantRecord, NamespaceInfo, ProposalStatus, Scored,
};
use crate::infra::{
    AgentRecord, AgentRegistration, AgentStore, BackupStore, GrantStore, IntegrationStore,
    MemoryStore,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use common::{AccountId, AgentId, Result};
use std::sync::Arc;
use uuid::Uuid;

/// 账户前缀分隔符。用户命名空间不允许 `/`（校验器拒绝），
/// 因此含 `/` 的内部键永远是引擎拼接产物，不可能与用户自造名冲突。
pub const SCOPE_SEPARATOR: char = '/';

/// 组合内部存储键：`{account}/{namespace}`
pub fn scope_name(account: &AccountId, namespace: &str) -> String {
    format!("{}{}{}", account, SCOPE_SEPARATOR, namespace)
}

/// 剥离账户前缀（不属于该账户或本就无前缀时原样返回）
pub fn unscope_name(account: &AccountId, name: &str) -> String {
    let prefix = format!("{}{}", account, SCOPE_SEPARATOR);
    name.strip_prefix(&prefix).unwrap_or(name).to_string()
}

/// 存储键是否属于该账户
pub fn in_scope(account: &AccountId, name: &str) -> bool {
    let prefix = format!("{}{}", account, SCOPE_SEPARATOR);
    name.starts_with(&prefix)
}

/// 幂等作用域化：已带本账户前缀则原样返回
fn scope_idempotent(account: &AccountId, name: &str) -> String {
    if in_scope(account, name) {
        name.to_string()
    } else {
        scope_name(account, name)
    }
}

/// 账户作用域存储包装器：输入命名空间加账户前缀，输出剥离前缀，
/// 集合查询只返回本账户数据。系统级维护操作（worker）请使用原始存储。
///
/// 泛型参数 `S` 允许包装任意粒度的存储抽象：
/// `AccountScopedStore<dyn MemoryHubStore>`（备份/整合服务）或
/// `AccountScopedStore<dyn MemoryStore>`（记忆服务）。
pub struct AccountScopedStore<S: ?Sized> {
    inner: Arc<S>,
    account: AccountId,
}

impl<S: ?Sized> AccountScopedStore<S> {
    pub fn new(inner: Arc<S>, account: AccountId) -> Self {
        Self { inner, account }
    }

    /// 输入侧：用户可见名 → 存储键（幂等）
    fn scoped(&self, ns: &str) -> String {
        scope_idempotent(&self.account, ns)
    }

    /// 输出侧：存储键 → 用户可见名
    fn unscoped(&self, ns: &str) -> String {
        unscope_name(&self.account, ns)
    }

    fn entry_out(&self, mut e: MemoryEntry) -> MemoryEntry {
        e.namespace = self.unscoped(&e.namespace);
        e
    }

    fn scoped_filter(&self, filter: &MemoryFilter) -> MemoryFilter {
        let mut f = filter.clone();
        f.namespace = f.namespace.as_deref().map(|ns| self.scoped(ns));
        f
    }

    fn namespace_info_out(&self, mut info: NamespaceInfo) -> NamespaceInfo {
        info.namespace = self.unscoped(&info.namespace);
        info
    }
}

// ===== MemoryStore =====

#[async_trait]
impl<S: MemoryStore + ?Sized> MemoryStore for AccountScopedStore<S> {
    async fn insert(&self, entry: &MemoryEntry) -> Result<MemoryEntry> {
        let mut scoped = entry.clone();
        scoped.namespace = self.scoped(&entry.namespace);
        let saved = self.inner.insert(&scoped).await?;
        Ok(self.entry_out(saved))
    }

    async fn get(&self, namespace: &str, id: Uuid) -> Result<MemoryEntry> {
        let e = self.inner.get(&self.scoped(namespace), id).await?;
        Ok(self.entry_out(e))
    }

    async fn get_by_key(&self, namespace: &str, key: &str) -> Result<Option<MemoryEntry>> {
        let e = self.inner.get_by_key(&self.scoped(namespace), key).await?;
        Ok(e.map(|e| self.entry_out(e)))
    }

    async fn save(&self, entry: &MemoryEntry) -> Result<MemoryEntry> {
        let mut scoped = entry.clone();
        scoped.namespace = self.scoped(&entry.namespace);
        let saved = self.inner.save(&scoped).await?;
        Ok(self.entry_out(saved))
    }

    async fn delete(&self, namespace: &str, id: Uuid) -> Result<()> {
        self.inner.delete(&self.scoped(namespace), id).await
    }

    async fn list(&self, filter: &MemoryFilter) -> Result<Vec<MemoryEntry>> {
        let rows = self.inner.list(&self.scoped_filter(filter)).await?;
        Ok(rows.into_iter().map(|e| self.entry_out(e)).collect())
    }

    async fn count(&self, filter: &MemoryFilter) -> Result<usize> {
        self.inner.count(&self.scoped_filter(filter)).await
    }

    async fn similar(
        &self,
        namespace: &str,
        embedding: &[f32],
        k: usize,
        kinds: Option<&[MemoryKind]>,
    ) -> Result<Vec<Scored<MemoryEntry>>> {
        let rows = self
            .inner
            .similar(&self.scoped(namespace), embedding, k, kinds)
            .await?;
        Ok(rows
            .into_iter()
            .map(|s| Scored {
                item: self.entry_out(s.item),
                score: s.score,
            })
            .collect())
    }

    async fn all_in_namespace(
        &self,
        namespace: &str,
        include_merged: bool,
    ) -> Result<Vec<MemoryEntry>> {
        let rows = self
            .inner
            .all_in_namespace(&self.scoped(namespace), include_merged)
            .await?;
        Ok(rows.into_iter().map(|e| self.entry_out(e)).collect())
    }

    async fn list_namespaces(&self) -> Result<Vec<Namespace>> {
        let all = self.inner.list_namespaces().await?;
        let mut out: Vec<Namespace> = all
            .into_iter()
            .filter(|ns| in_scope(&self.account, ns))
            .map(|ns| self.unscoped(&ns))
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    }

    async fn record_version(&self, v: &MemoryVersion) -> Result<()> {
        // 版本历史按 memory_id 关联，无命名空间字段，直接透传
        self.inner.record_version(v).await
    }

    async fn versions(&self, id: Uuid) -> Result<Vec<MemoryVersion>> {
        self.inner.versions(id).await
    }

    async fn archive_expired(&self, now: DateTime<Utc>) -> Result<usize> {
        // 系统级批处理（worker 用原始存储调用）；经本包装器调用会作用于全库，
        // 服务层不得依赖此方法做账户内归档。
        self.inner.archive_expired(now).await
    }
}

// ===== BackupStore =====

#[async_trait]
impl<S: BackupStore + ?Sized> BackupStore for AccountScopedStore<S> {
    async fn save_backup(&self, backup: &BackupRecord) -> Result<()> {
        let mut scoped = backup.clone();
        scoped.namespace = self.scoped(&backup.namespace);
        self.inner.save_backup(&scoped).await
    }

    async fn get_backup(&self, id: Uuid) -> Result<BackupRecord> {
        let mut b = self.inner.get_backup(id).await?;
        b.namespace = self.unscoped(&b.namespace);
        Ok(b)
    }

    async fn list_backups(&self, namespace: Option<&str>) -> Result<Vec<BackupRecord>> {
        let rows = match namespace {
            Some(ns) => self.inner.list_backups(Some(&self.scoped(ns))).await?,
            None => {
                let all = self.inner.list_backups(None).await?;
                all.into_iter()
                    .filter(|b| in_scope(&self.account, &b.namespace))
                    .collect()
            }
        };
        Ok(rows
            .into_iter()
            .map(|mut b| {
                b.namespace = self.unscoped(&b.namespace);
                b
            })
            .collect())
    }

    async fn delete_backup(&self, id: Uuid) -> Result<()> {
        // 备份按 id 删除，保留策略由 worker 用原始存储执行；服务层调用前
        // 必须先经 get_backup 校验归属，故此处透传。
        self.inner.delete_backup(id).await
    }
}

// ===== IntegrationStore =====

#[async_trait]
impl<S: IntegrationStore + ?Sized> IntegrationStore for AccountScopedStore<S> {
    async fn save_proposal(&self, p: &IntegrationProposal) -> Result<()> {
        let mut scoped = p.clone();
        scoped.namespace = self.scoped(&p.namespace);
        self.inner.save_proposal(&scoped).await
    }

    async fn get_proposal(&self, id: Uuid) -> Result<IntegrationProposal> {
        let mut p = self.inner.get_proposal(id).await?;
        p.namespace = self.unscoped(&p.namespace);
        Ok(p)
    }

    async fn list_proposals(
        &self,
        namespace: &str,
        status: Option<ProposalStatus>,
    ) -> Result<Vec<IntegrationProposal>> {
        let rows = self
            .inner
            .list_proposals(&self.scoped(namespace), status)
            .await?;
        Ok(rows
            .into_iter()
            .map(|mut p| {
                p.namespace = self.unscoped(&p.namespace);
                p
            })
            .collect())
    }

    async fn open_exists(
        &self,
        namespace: &str,
        kind_member: &[(String, Vec<Uuid>)],
    ) -> Result<bool> {
        self.inner
            .open_exists(&self.scoped(namespace), kind_member)
            .await
    }
}

// ===== GrantStore =====

#[async_trait]
impl<S: GrantStore + ?Sized> GrantStore for AccountScopedStore<S> {
    async fn ensure_namespace(
        &self,
        namespace: &str,
        owner: &AgentId,
    ) -> Result<(NamespaceInfo, bool)> {
        let (info, created) = self
            .inner
            .ensure_namespace(&self.scoped(namespace), owner)
            .await?;
        Ok((self.namespace_info_out(info), created))
    }

    async fn get_namespace(&self, namespace: &str) -> Result<Option<NamespaceInfo>> {
        let info = self.inner.get_namespace(&self.scoped(namespace)).await?;
        Ok(info.map(|i| self.namespace_info_out(i)))
    }

    async fn list_namespace_infos(&self) -> Result<Vec<NamespaceInfo>> {
        let all = self.inner.list_namespace_infos().await?;
        Ok(all
            .into_iter()
            .filter(|i| in_scope(&self.account, &i.namespace))
            .map(|i| self.namespace_info_out(i))
            .collect())
    }

    async fn grant(&self, record: &NamespaceGrantRecord) -> Result<()> {
        let mut scoped = record.clone();
        scoped.namespace = self.scoped(&record.namespace);
        self.inner.grant(&scoped).await
    }

    async fn revoke(&self, namespace: &str, agent_id: &AgentId) -> Result<()> {
        self.inner.revoke(&self.scoped(namespace), agent_id).await
    }

    async fn grants_for(&self, agent_id: &AgentId) -> Result<Vec<NamespaceGrantRecord>> {
        let rows = self.inner.grants_for(agent_id).await?;
        Ok(rows
            .into_iter()
            .filter(|r| in_scope(&self.account, &r.namespace))
            .map(|mut r| {
                r.namespace = self.unscoped(&r.namespace);
                r
            })
            .collect())
    }

    async fn can_read(&self, namespace: &str, agent_id: &AgentId) -> Result<bool> {
        self.inner.can_read(&self.scoped(namespace), agent_id).await
    }

    async fn can_write(&self, namespace: &str, agent_id: &AgentId) -> Result<bool> {
        self.inner.can_write(&self.scoped(namespace), agent_id).await
    }

    async fn owned_namespaces(&self, agent_id: &AgentId) -> Result<Vec<Namespace>> {
        let all = self.inner.owned_namespaces(agent_id).await?;
        Ok(all
            .into_iter()
            .filter(|ns| in_scope(&self.account, ns))
            .map(|ns| self.unscoped(&ns))
            .collect())
    }
}

// ===== AgentStore =====
//
// Agent 归属关系由 `owner_account_id` 显式表达（不做命名空间前缀），
// 此处全部透传；调用方需自行按 owner_account_id 过滤（如控制台）。

#[async_trait]
impl<S: AgentStore + ?Sized> AgentStore for AccountScopedStore<S> {
    async fn register_agent(&self, reg: &AgentRegistration) -> Result<AgentRecord> {
        self.inner.register_agent(reg).await
    }

    async fn get_agent(&self, agent_id: &AgentId) -> Result<Option<AgentRecord>> {
        self.inner.get_agent(agent_id).await
    }

    async fn list_agents(&self) -> Result<Vec<AgentRecord>> {
        self.inner.list_agents().await
    }

    async fn set_agent_status(&self, agent_id: &AgentId, status: &str) -> Result<()> {
        self.inner.set_agent_status(agent_id, status).await
    }

    async fn verify_agent_secret(&self, agent_id: &AgentId, secret: &str) -> Result<bool> {
        self.inner.verify_agent_secret(agent_id, secret).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(n: u128) -> AccountId {
        Uuid::from_u128(n)
    }

    #[test]
    fn test_scope_helpers_idempotent_and_injective() {
        let a = account(1);
        let b = account(2);
        assert_eq!(scope_name(&a, "personal"), format!("{}/personal", a));
        // 幂等：已带前缀不二次拼接
        assert_eq!(
            scope_idempotent(&a, &scope_name(&a, "personal")),
            scope_name(&a, "personal")
        );
        // 单射：不同账户的同名空间拼出不同键
        assert_ne!(scope_name(&a, "personal"), scope_name(&b, "personal"));
        // 剥离：属于本账户才剥
        assert_eq!(unscope_name(&a, &scope_name(&a, "personal")), "personal");
        assert_eq!(
            unscope_name(&b, &scope_name(&a, "personal")),
            scope_name(&a, "personal")
        );
        // 用户可见名不含 `/`，永远不可能"看起来已作用域化"
        assert!(!in_scope(&a, "personal"));
        assert!(in_scope(&a, &scope_name(&a, "personal")));
    }
}

// 完整虚拟化行为（双账户隔离 / 授权 / 备份 / 前缀幂等）在 store crate 的
// `account_scope_tests` 中验证——那里可以直接使用 InMemoryHubStore。