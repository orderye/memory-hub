//! 身份认证上下文与访问控制列表（基于设计规范 17.1 节）

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use uuid::Uuid;

/// 用户唯一标识
pub type UserId = Uuid;

/// OAuth 客户端标识
pub type ClientId = String;

/// 授权范围（Scope）
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Scope {
    #[serde(rename = "memory:context")]
    Context,
    #[serde(rename = "memory:write")]
    Write,
    #[serde(rename = "memory:manage")]
    Manage,
}

impl Scope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::Context => "memory:context",
            Scope::Write => "memory:write",
            Scope::Manage => "memory:manage",
        }
    }
}

/// 细粒度访问控制列表（用户授权的可读层、可写层、是否允许敏感层）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Acl {
    /// 允许读取的层（如 fact, preference 等）
    pub read_layers: HashSet<String>,
    /// 允许写入的层
    pub write_layers: HashSet<String>,
    /// 是否允许涉及敏感层
    pub allow_sensitive: bool,
}

/// 鉴权上下文（由 hub-authz 校验令牌后构造）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthCtx {
    pub user_id: UserId,
    pub client_id: ClientId,
    pub scopes: Vec<Scope>,
    pub acl: Acl,
}

impl AuthCtx {
    /// 检查是否拥有特定 Scope
    pub fn has_scope(&self, required: &Scope) -> bool {
        self.scopes.contains(required)
    }

    /// 检查是否具备读某层的权限
    pub fn can_read_layer(&self, layer: &str) -> bool {
        self.acl.read_layers.contains(layer)
    }

    /// 检查是否具备写某层的权限
    pub fn can_write_layer(&self, layer: &str) -> bool {
        self.acl.write_layers.contains(layer)
    }
}
