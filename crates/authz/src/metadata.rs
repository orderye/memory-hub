//! RFC 9728 受保护资源元数据公布（OAuth 2.0 Protected Resource Metadata）

use serde::{Deserialize, Serialize};

/// 受保护资源元数据（供第三方 MCP 客户端自动发现授权服务器）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtectedResourceMetadata {
    /// 资源标识符 URI（RFC 8707）
    pub resource: String,
    /// 授权服务器列表
    pub authorization_servers: Vec<String>,
    /// 支持的授权范围
    pub scopes_supported: Vec<String>,
    /// Bearer 令牌携带方式
    pub bearer_methods_supported: Vec<String>,
    /// 资源文档地址
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_documentation: Option<String>,
}

impl ProtectedResourceMetadata {
    pub fn default_for_hub(resource_uri: &str, auth_server_uri: &str) -> Self {
        Self {
            resource: resource_uri.to_string(),
            authorization_servers: vec![auth_server_uri.to_string()],
            scopes_supported: vec![
                "memory:read".to_string(),
                "memory:write".to_string(),
                "memory:admin".to_string(),
            ],
            bearer_methods_supported: vec!["header".to_string()],
            resource_documentation: Some("https://github.com/memoryhub/docs".to_string()),
        }
    }
}
