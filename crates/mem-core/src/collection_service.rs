//! Agent 记忆集合服务（任务书 4.4）
//!
//! 设计约束：
//! 1. **只收窄、不授权**：集合只给出候选范围；实际读取仍由各服务的
//!    `AccessChecker` 做实时授权判定。绑定不是权限，解绑也不是删除。
//! 2. **fail closed**：未绑定集合的 Agent 直接 `InsufficientScope`；
//!    集合含通配符或非法命名空间在写入时就拒绝。
//! 3. **删除边界**：删除集合级联删除绑定，但绝不触碰底层记忆。

use crate::domain::{
    CollectionBinding, CollectionFilter, MemoryCollection, ResolvedCollection,
    UpsertCollectionRequest, MAX_COLLECTION_NAMESPACES,
};
use crate::infra::MemoryHubStore;
use chrono::Utc;
use common::{AuthCtx, HubError, Result, Scope};
use std::sync::Arc;
use uuid::Uuid;

/// 集合名称约束：小写字母/数字/`-`/`_`/`.`，长度 1–64
pub fn validate_collection_name(name: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() || name.len() > 64 {
        return Err(HubError::InvalidArgument(
            "集合名称长度必须在 1..=64 之间".into(),
        ));
    }
    let ok = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'));
    if !ok {
        return Err(HubError::InvalidArgument(
            "集合名称只允许小写字母/数字/-/_/.，且不能包含空格".into(),
        ));
    }
    Ok(())
}

/// Agent 记忆集合服务
pub struct CollectionService {
    store: Arc<dyn MemoryHubStore>,
}

impl CollectionService {
    pub fn new(store: Arc<dyn MemoryHubStore>) -> Self {
        Self { store }
    }

    fn require_admin(auth: &AuthCtx) -> Result<()> {
        if auth.has_scope(&Scope::Admin) {
            Ok(())
        } else {
            Err(HubError::InsufficientScope)
        }
    }

    /// 新建或更新集合（按名称唯一；更新保留原 id）
    pub async fn upsert(
        &self,
        auth: &AuthCtx,
        req: UpsertCollectionRequest,
    ) -> Result<MemoryCollection> {
        Self::require_admin(auth)?;
        validate_collection_name(&req.name)?;
        req.filter.validate()?;
        if req.filter.namespaces.len() > MAX_COLLECTION_NAMESPACES {
            return Err(HubError::InvalidArgument(format!(
                "单个集合最多包含 {} 个命名空间",
                MAX_COLLECTION_NAMESPACES
            )));
        }

        let now = Utc::now();
        let id = match req.id {
            Some(id) => {
                // 显式 id：必须已属于当前账户，不允许借 upsert 改写他人集合
                self.store
                    .get_collection(auth.account_id, id)
                    .await?
                    .ok_or(HubError::NotFound)?;
                // 同名但不同 id：拒绝，避免“改名撞车”静默覆盖另一条集合
                if let Some(by_name) = self
                    .store
                    .get_collection_by_name(auth.account_id, req.name.trim())
                    .await?
                {
                    if by_name.id != id {
                        return Err(HubError::Conflict);
                    }
                }
                id
            }
            None => Uuid::new_v4(),
        };

        let collection = MemoryCollection {
            id,
            account_id: auth.account_id,
            name: req.name.trim().to_string(),
            description: req.description,
            filter: req.filter,
            created_by: auth.agent_id.clone(),
            created_at: now,
            updated_at: now,
        };

        self.store.upsert_collection(&collection).await
    }

    /// 列出当前账户的全部集合（含过滤器）
    pub async fn list(&self, auth: &AuthCtx) -> Result<Vec<MemoryCollection>> {
        Self::require_admin(auth)?;
        self.store.list_collections(auth.account_id).await
    }

    /// 删除集合（级联删除绑定，不删除记忆）
    pub async fn delete(&self, auth: &AuthCtx, collection_id: Uuid) -> Result<()> {
        Self::require_admin(auth)?;
        self.store
            .delete_collection(auth.account_id, collection_id)
            .await
    }

    /// 绑定 Agent 到集合
    ///
    /// 目标 Agent 必须属于当前账户：集合是账户级配置，不能成为跨账户旁路的跳板。
    pub async fn bind(&self, auth: &AuthCtx, collection_id: Uuid, agent_id: &str) -> Result<()> {
        Self::require_admin(auth)?;
        let collection = self
            .store
            .get_collection(auth.account_id, collection_id)
            .await?
            .ok_or(HubError::NotFound)?;

        let agent_key = agent_id.to_string();
        let agent = self
            .store
            .get_agent(&agent_key)
            .await?
            .ok_or(HubError::NotFound)?;
        if agent.owner_account_id != auth.account_id {
            return Err(HubError::InsufficientScope);
        }

        self.store
            .bind_collection(&CollectionBinding {
                collection_id: collection.id,
                agent_id: agent_id.to_string(),
                created_by: auth.agent_id.clone(),
                created_at: Utc::now(),
            })
            .await
    }

    /// 解除绑定（立即生效，不删除任何记忆）
    pub async fn unbind(&self, auth: &AuthCtx, collection_id: Uuid, agent_id: &str) -> Result<()> {
        Self::require_admin(auth)?;
        self.store
            .get_collection(auth.account_id, collection_id)
            .await?
            .ok_or(HubError::NotFound)?;
        let agent_key = agent_id.to_string();
        self.store
            .unbind_collection(collection_id, &agent_key)
            .await
    }

    /// 列出集合的绑定
    pub async fn list_bindings(
        &self,
        auth: &AuthCtx,
        collection_id: Uuid,
    ) -> Result<Vec<CollectionBinding>> {
        Self::require_admin(auth)?;
        self.store
            .get_collection(auth.account_id, collection_id)
            .await?
            .ok_or(HubError::NotFound)?;
        self.store.list_bindings(collection_id).await
    }

    /// 解析集合为候选命名空间（供检索使用）
    ///
    /// 未绑定走 fail closed；返回的 `resolved_namespaces` 仅是候选，
    /// 实际读取时各服务仍会做实时 ACL 判定。
    pub async fn resolve(&self, auth: &AuthCtx, name: &str) -> Result<ResolvedCollection> {
        if !auth.has_scope(&Scope::Read) {
            return Err(HubError::InsufficientScope);
        }
        let collection = self
            .store
            .get_collection_by_name(auth.account_id, name)
            .await?
            .ok_or(HubError::NotFound)?;

        if !self.store.is_bound(collection.id, &auth.agent_id).await? {
            return Err(HubError::InsufficientScope);
        }

        Ok(ResolvedCollection {
            resolved_namespaces: collection.filter.namespaces.clone(),
            skipped_namespaces: Vec::new(),
            collection,
        })
    }

    /// 解析并立即按调用方 ACL 预筛（返回 `(可用, 无权限)`）
    ///
    /// 与 [`Self::resolve`] 的区别：这里用令牌 ACL 做一次快速预筛，
    /// 避免对必然拒绝的命名空间发起检索；最终判定仍由读取路径负责。
    pub async fn resolve_for_agent(
        &self,
        auth: &AuthCtx,
        name: &str,
        can_read: impl Fn(&str) -> bool,
    ) -> Result<ResolvedCollection> {
        let mut resolved = self.resolve(auth, name).await?;
        let (ok, denied): (Vec<_>, Vec<_>) = resolved
            .resolved_namespaces
            .drain(..)
            .partition(|ns| can_read(ns));
        resolved.resolved_namespaces = ok;
        resolved.skipped_namespaces = denied;
        Ok(resolved)
    }

    /// 读取集合的过滤器（供检索工具构造查询）
    pub fn filter_of(collection: &MemoryCollection) -> &CollectionFilter {
        &collection.filter
    }
}
