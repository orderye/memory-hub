//! 命名空间访问控制执行器
//!
//! 规则：
//! - token scope 决定"能不能做这类操作"（read/write/admin）；
//! - GrantStore 决定"能对哪个命名空间做"（owner / 显式授权）；
//! - 命名空间先按 `auth.account_id` 作用域化再查库：跨账户永远无匹配行；
//! - 首次写入新命名空间时，写入方自动成为 owner 并获得读写授权。

use crate::account_scope::scope_name;
use crate::infra::{AccessChecker, GrantStore};
use async_trait::async_trait;
use common::{AuthCtx, Result};
use std::sync::Arc;

/// 基于 GrantStore 的访问判定（账户沙箱内）
pub struct GrantChecker {
    store: Arc<dyn GrantStore>,
}

impl GrantChecker {
    pub fn new(store: Arc<dyn GrantStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl AccessChecker for GrantChecker {
    async fn can_read(&self, auth: &AuthCtx, namespace: &str) -> Result<bool> {
        if namespace.contains(crate::account_scope::SCOPE_SEPARATOR) {
            // 用户可见命名空间不含 `/`；带分隔符的输入只会来自引擎内部键，
            // 一律拒绝，防止绕过作用域直接指定内部键。
            return Ok(false);
        }
        self.store
            .can_read(&scope_name(&auth.account_id, namespace), &auth.agent_id)
            .await
    }

    async fn can_write(&self, auth: &AuthCtx, namespace: &str) -> Result<bool> {
        if namespace.contains(crate::account_scope::SCOPE_SEPARATOR) {
            return Ok(false);
        }
        self.store
            .can_write(&scope_name(&auth.account_id, namespace), &auth.agent_id)
            .await
    }

    async fn is_owner(&self, auth: &AuthCtx, namespace: &str) -> Result<bool> {
        if namespace.contains(crate::account_scope::SCOPE_SEPARATOR) {
            return Ok(false);
        }
        Ok(self
            .store
            .get_namespace(&scope_name(&auth.account_id, namespace))
            .await?
            .map(|info| info.owner_agent == auth.agent_id)
            .unwrap_or(false))
    }

    async fn ensure_namespace(&self, auth: &AuthCtx, namespace: &str) -> Result<()> {
        let scoped = scope_name(&auth.account_id, namespace);
        let (_info, created) = self.store.ensure_namespace(&scoped, &auth.agent_id).await?;
        if !created {
            // 命名空间已存在：不得借 ensure 之名给后来者放权
            return Ok(());
        }
        // owner 隐式获得读写授权（仅新建时）
        let now = chrono::Utc::now();
        self.store
            .grant(&crate::domain::NamespaceGrantRecord {
                namespace: scoped,
                agent_id: auth.agent_id.clone(),
                can_read: true,
                can_write: true,
                granted_by: auth.agent_id.clone(),
                created_at: now,
            })
            .await
    }
}

/// 命名空间名校验：非空、≤128 字符、仅小写字母/数字/`-`/`_`/`.`/`*`
pub fn validate_namespace(ns: &str) -> Result<()> {
    if ns.is_empty() {
        return Err(common::HubError::InvalidArgument(
            "namespace 不能为空".to_string(),
        ));
    }
    if ns.len() > 128 {
        return Err(common::HubError::InvalidArgument(
            "namespace 长度不得超过 128 字符".to_string(),
        ));
    }
    let ok = ns.chars().all(|c| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.' | '*')
    });
    if !ok {
        return Err(common::HubError::InvalidArgument(format!(
            "namespace 只允许小写字母/数字/-/_/./* ，当前为 {}",
            ns
        )));
    }
    Ok(())
}

/// key 校验：非空、≤128 字符
pub fn validate_key(key: &str) -> Result<()> {
    if key.trim().is_empty() {
        return Err(common::HubError::InvalidArgument(
            "key 不能为空字符串".to_string(),
        ));
    }
    if key.len() > 128 {
        return Err(common::HubError::InvalidArgument(
            "key 长度不得超过 128 字符".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_namespace() {
        assert!(validate_namespace("default").is_ok());
        assert!(validate_namespace("team-x_1.y").is_ok());
        assert!(validate_namespace("*").is_ok());
        assert!(validate_namespace("").is_err());
        assert!(validate_namespace("Team X").is_err());
        assert!(validate_namespace("团队").is_err());
        assert!(validate_namespace(&"a".repeat(129)).is_err());
    }

    #[test]
    fn test_validate_key() {
        assert!(validate_key("user:pref:lang").is_ok());
        assert!(validate_key("").is_err());
        assert!(validate_key("  ").is_err());
        assert!(validate_key(&"k".repeat(129)).is_err());
    }
}
