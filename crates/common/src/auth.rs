//! 多智能体记忆服务的身份认证上下文与授权范围
//!
//! 本服务的两级主体模型：
//! - **Account（账户）**：终端用户，沙箱边界。每个账户拥有独立的命名空间
//!   空间与自己的 Agent 集合，跨账户不可见、不可访问；账户通过控制台注册/登录。
//! - **Agent**：调用方（AI 助手、自动化 Worker、批处理任务），归属于某个账户，
//!   以自己的 `agent_id` 身份访问本账户内的记忆命名空间，通过 OAuth 2.1 获取令牌。

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use uuid::Uuid;

/// Agent 唯一标识（与 OAuth client_id 对应）
pub type AgentId = String;

/// 账户唯一标识（沙箱边界）
pub type AccountId = Uuid;

/// 引导账户 ID：固定值，SQL 迁移据此把存量数据归属到该账户
pub const BOOTSTRAP_ACCOUNT_ID: Uuid = Uuid::from_u128(1);

/// 授权范围（Scope）
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Scope {
    /// 读取记忆（get / search / list / stats / 备份列表）
    #[serde(rename = "memory:read")]
    Read,
    /// 写入与整合记忆（put / update / delete / integrate）
    #[serde(rename = "memory:write")]
    Write,
    /// 管理操作（备份 / 恢复 / 跨 Agent 共享授权）
    #[serde(rename = "memory:admin")]
    Admin,
}

impl Scope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::Read => "memory:read",
            Scope::Write => "memory:write",
            Scope::Admin => "memory:admin",
        }
    }

    /// 从字符串解析（未知值返回 None，调用方应拒绝）
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "memory:read" => Some(Scope::Read),
            "memory:write" => Some(Scope::Write),
            "memory:admin" => Some(Scope::Admin),
            _ => None,
        }
    }

    /// 全部作用域（注册客户端时的上限）
    pub fn all() -> Vec<Scope> {
        vec![Scope::Read, Scope::Write, Scope::Admin]
    }
}

/// 命名空间授权条目（随访问令牌下发，供资源服务器离线构造 ACL）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceGrant {
    /// 命名空间（`*` 表示全部）
    pub namespace: String,
    /// 是否可读
    #[serde(default)]
    pub read: bool,
    /// 是否可写
    #[serde(default)]
    pub write: bool,
}

impl NamespaceGrant {
    pub fn read_only(ns: impl Into<String>) -> Self {
        Self {
            namespace: ns.into(),
            read: true,
            write: false,
        }
    }

    pub fn read_write(ns: impl Into<String>) -> Self {
        Self {
            namespace: ns.into(),
            read: true,
            write: true,
        }
    }
}

/// 命名空间访问策略（由令牌中的授权条目解析而来）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Acl {
    /// 被显式授权可读的命名空间（含通配 `*`）
    pub read_namespaces: HashSet<String>,
    /// 被显式授权可写的命名空间（含通配 `*`）
    pub write_namespaces: HashSet<String>,
}

impl Acl {
    /// 从授权条目构造 ACL
    pub fn from_grants(grants: &[NamespaceGrant]) -> Self {
        let mut acl = Self::default();
        for g in grants {
            if g.read {
                acl.read_namespaces.insert(g.namespace.clone());
            }
            if g.write {
                acl.write_namespaces.insert(g.namespace.clone());
            }
        }
        acl
    }

    /// 是否允许读该命名空间（通配 `*` 匹配一切）
    pub fn can_read(&self, namespace: &str) -> bool {
        self.read_namespaces.contains("*") || self.read_namespaces.contains(namespace)
    }

    /// 是否允许写该命名空间
    pub fn can_write(&self, namespace: &str) -> bool {
        self.write_namespaces.contains("*") || self.write_namespaces.contains(namespace)
    }
}

/// 鉴权上下文（由 authz 校验访问令牌后构造）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthCtx {
    /// 调用方 Agent 标识
    pub agent_id: AgentId,
    /// 所属账户（沙箱边界；所有命名空间访问都在该账户内解析）
    pub account_id: AccountId,
    /// 令牌携带的作用域
    pub scopes: Vec<Scope>,
    /// 命名空间级访问控制
    pub acl: Acl,
}

impl AuthCtx {
    /// 构造鉴权上下文（测试与内部装配用；令牌路径请走 authz 校验）
    pub fn new(
        agent_id: impl Into<AgentId>,
        account_id: AccountId,
        scopes: Vec<Scope>,
        acl: Acl,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            account_id,
            scopes,
            acl,
        }
    }
    /// 检查是否拥有特定 Scope
    pub fn has_scope(&self, required: &Scope) -> bool {
        self.scopes.contains(required)
    }

    /// 读取命名空间的前置校验（scope + ACL 双重判定）
    pub fn can_read_namespace(&self, namespace: &str) -> bool {
        self.has_scope(&Scope::Read) && self.acl.can_read(namespace)
    }

    /// 写入命名空间的前置校验
    pub fn can_write_namespace(&self, namespace: &str) -> bool {
        self.has_scope(&Scope::Write) && self.acl.can_write(namespace)
    }

    /// 管理操作前置校验
    pub fn is_admin(&self) -> bool {
        self.has_scope(&Scope::Admin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with(acl: Acl, scopes: Vec<Scope>) -> AuthCtx {
        AuthCtx {
            agent_id: "agent-a".to_string(),
            account_id: Uuid::from_u128(7),
            scopes,
            acl,
        }
    }

    #[test]
    fn test_scope_parse_roundtrip() {
        for s in Scope::all() {
            assert_eq!(Scope::parse(s.as_str()), Some(s));
        }
        assert_eq!(Scope::parse("memory:root"), None);
    }

    #[test]
    fn test_scope_serde_uses_wire_names() {
        let v = serde_json::to_value(&Scope::Admin).unwrap();
        assert_eq!(v, serde_json::json!("memory:admin"));
    }

    #[test]
    fn test_acl_from_grants() {
        let acl = Acl::from_grants(&[
            NamespaceGrant::read_only("team-x"),
            NamespaceGrant::read_write("team-y"),
        ]);
        assert!(acl.can_read("team-x"));
        assert!(!acl.can_write("team-x"));
        assert!(acl.can_read("team-y"));
        assert!(acl.can_write("team-y"));
        assert!(!acl.can_read("team-z"));
    }

    #[test]
    fn test_acl_wildcard() {
        let acl = Acl::from_grants(&[NamespaceGrant::read_write("*")]);
        assert!(acl.can_read("anything"));
        assert!(acl.can_write("anything"));
    }

    #[test]
    fn test_auth_ctx_requires_both_scope_and_acl() {
        // 有 read scope 但没有 ACL：不得读
        let c = ctx_with(Acl::default(), vec![Scope::Read]);
        assert!(!c.can_read_namespace("ns"));

        // 有 ACL 但没有 scope：同样不得读
        let c = ctx_with(
            Acl::from_grants(&[NamespaceGrant::read_only("ns")]),
            vec![Scope::Write],
        );
        assert!(!c.can_read_namespace("ns"));

        // 两者齐备：放行
        let c = ctx_with(
            Acl::from_grants(&[NamespaceGrant::read_write("ns")]),
            vec![Scope::Read, Scope::Write],
        );
        assert!(c.can_read_namespace("ns"));
        assert!(c.can_write_namespace("ns"));
    }
}
