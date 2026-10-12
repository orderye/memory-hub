#![forbid(unsafe_code)]

//! 多智能体长期记忆服务 MCP 适配层（mcp）
//!
//! 提供：
//! - `ToolDispatcher`：16 个记忆工具调用分发与结构化/文本双模输出
//! - `handle_discover`, `handle_tools_list`：发现与工具清单契约
//! - `get_tool_definitions`：JSON Schema 2020-12 严格定义
//! - `transport`：Streamable HTTP 与 stdio 传输层、JSON-RPC 2.0 帧处理

pub mod adapter;
pub mod protocol;
pub mod schema;
pub mod tools;
pub mod transport;

pub use adapter::ToolInterceptor;
#[cfg(feature = "adapter-cjs")]
pub use adapter::cjs::{CjsAdapter, CjsAdapterConfig};

pub use protocol::{handle_discover, handle_tools_list, SERVER_INSTRUCTIONS};
pub use schema::{get_tool_definitions, ToolDefinition};
pub use tools::{McpContentItem, ToolCallResult, ToolDispatcher};
pub use transport::{
    extract_bearer_token, first_text, handle_jsonrpc, handle_jsonrpc_ext, handle_raw_frame,
    hub_error_to_jsonrpc, mcp_router, required_scope_for_tool, run_stdio, tool_result_to_http,
    JsonRpcError, JsonRpcRequest, JsonRpcResponse, McpTransportState, RateLimiter, RateVerdict,
    PROTOCOL_VERSION_FALLBACK, PROTOCOL_VERSION_PRIMARY,
};
