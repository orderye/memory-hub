//! MCP 协议层与能力声明（基于设计规范 5.2 与 5.3 节）

use crate::schema::get_tool_definitions;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// MCP 服务端发现与能力声明响应
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerDiscoverResponse {
    pub protocol_version: String,
    pub supported_versions: Vec<String>,
    pub server_info: ServerInfo,
    pub capabilities: ServerCapabilities,
    pub instructions: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerCapabilities {
    pub tools: ToolsCapability,
    pub resources: Option<Value>,
    pub prompts: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsCapability {
    pub list_changed: bool,
}

/// 处理 `server/discover` 或 `initialize`
pub fn handle_discover() -> ServerDiscoverResponse {
    ServerDiscoverResponse {
        protocol_version: "2026-07-28".to_string(),
        supported_versions: vec!["2026-07-28".to_string(), "2025-11-25".to_string()],
        server_info: ServerInfo {
            name: "memory-hub".to_string(),
            version: "1.0.0".to_string(),
        },
        capabilities: ServerCapabilities {
            tools: ToolsCapability {
                list_changed: false,
            },
            resources: Some(json!({ "subscribe": false })),
            prompts: Some(json!({ "list_changed": false })),
        },
        instructions: "生成回答之前先调用 hub_context；回答后回传 hub_check 与 hub_observe"
            .to_string(),
    }
}

/// 处理 `tools/list`
pub fn handle_tools_list() -> Value {
    let defs = get_tool_definitions();
    let tools_json: Vec<Value> = defs
        .into_iter()
        .map(|d| {
            json!({
                "name": d.name,
                "description": d.description,
                "inputSchema": d.input_schema,
                "annotations": {
                    "readOnly": d.read_only,
                    "destructive": d.destructive,
                    "idempotent": d.idempotent,
                }
            })
        })
        .collect();

    json!({
        "tools": tools_json,
        "ttlMs": 3600000,
        "cacheScope": "shared"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_discover_and_tools_list() {
        let discover = handle_discover();
        assert_eq!(discover.protocol_version, "2026-07-28");
        assert!(discover
            .supported_versions
            .contains(&"2025-11-25".to_string()));

        let tools = handle_tools_list();
        let tools_arr = tools.get("tools").unwrap().as_array().unwrap();
        assert_eq!(tools_arr.len(), 7); // 7 个核心工具
    }
}
