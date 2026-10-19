//! MCP 协议层与能力声明
//!
//! 服务定位：多智能体长期记忆的存储 / 备份 / 整合基础设施。
//! 协议面完全遵循 MCP 规范（initialize / tools/list / tools/call / ping）。

use crate::schema::get_tool_definitions;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// MCP 服务端发现与能力声明响应
///
/// 字段名遵循 MCP 规范的 camelCase 约定（`protocolVersion` / `serverInfo`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerDiscoverResponse {
    pub protocol_version: String,
    pub supported_versions: Vec<String>,
    pub server_info: ServerInfo,
    pub capabilities: ServerCapabilities,
    pub instructions: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilities {
    pub tools: ToolsCapability,
    pub resources: Option<Value>,
    pub prompts: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsCapability {
    pub list_changed: bool,
}

/// 服务使用说明（下发给调用方 Agent）
pub const SERVER_INSTRUCTIONS: &str = "多智能体长期记忆服务。所有记忆按命名空间（namespace）隔离，\
首次写入某命名空间的 Agent 自动成为其 owner。调用流程：写入用 memory_put（同 namespace+key 为 upsert）；\
读取用 memory_get / memory_search / memory_list；修订用 memory_update；停止使用用 memory_delete（归档）。\
管理操作（memory_backup_create / memory_restore / memory_grant / memory_revoke）需要 memory:admin 作用域。\
多 Agent 并发写入产生的重复与冲突由 memory_integrate_scan 识别、memory_integration_resolve 裁决。";

/// 处理 `server/discover` 或 `initialize`
pub fn handle_discover() -> ServerDiscoverResponse {
    ServerDiscoverResponse {
        protocol_version: "2026-07-28".to_string(),
        supported_versions: vec!["2026-07-28".to_string(), "2025-11-25".to_string()],
        server_info: ServerInfo {
            name: "memory-hub".to_string(),
            version: "2.0.0".to_string(),
        },
        capabilities: ServerCapabilities {
            tools: ToolsCapability {
                list_changed: false,
            },
            resources: Some(json!({ "subscribe": false })),
            prompts: Some(json!({ "list_changed": false })),
        },
        instructions: SERVER_INSTRUCTIONS.to_string(),
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
        let list = tools["tools"].as_array().expect("tools 应为数组");
        assert_eq!(
            list.len(),
            28,
            "应暴露全部 28 个记忆工具（18 个核心基础工具 + 10 个认知增强工具）"
        );
        assert!(list.iter().any(|t| t["name"] == "memory_put"));
        assert!(list.iter().any(|t| t["name"] == "memory_search"));
        assert!(list.iter().any(|t| t["name"] == "memory_backup_create"));
        assert!(list.iter().any(|t| t["name"] == "memory_integrate_scan"));
        assert!(list.iter().any(|t| t["name"] == "cognitive_chat_propose"));
        assert!(list.iter().any(|t| t["name"] == "working_task_start"));
        assert!(list.iter().any(|t| t["name"] == "coding_thread_create"));
    }

    #[test]
    fn test_all_tools_carry_annotations() {
        let tools = handle_tools_list();
        for t in tools["tools"].as_array().unwrap() {
            assert!(
                t["annotations"]["readOnly"].is_boolean(),
                "工具 {} 缺少 readOnly 标注",
                t["name"]
            );
            assert!(t["annotations"]["destructive"].is_boolean());
            assert!(t["annotations"]["idempotent"].is_boolean());
            assert!(t["description"].as_str().is_some_and(|d| !d.is_empty()));
        }
    }
}
