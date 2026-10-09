#![forbid(unsafe_code)]

//! MCP 标准服务反腐层（hub-mcp）
//!
//! 提供：
//! - `ToolDispatcher`：工具调用分发与结构化/文本双模输出
//! - `handle_discover`, `handle_tools_list`：发现与工具清单契约
//! - `get_tool_definitions`：JSON Schema 2020-12 严格定义

pub mod protocol;
pub mod schema;
pub mod tools;

pub use protocol::{handle_discover, handle_tools_list};
pub use schema::{get_tool_definitions, ToolDefinition};
pub use tools::{McpContentItem, ToolCallResult, ToolDispatcher};
