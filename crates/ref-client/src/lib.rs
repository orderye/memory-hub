#![forbid(unsafe_code)]

//! 记忆中枢标准参照客户端（ref-client）
//!
//! 提供：
//! - `ReferenceMcpClient`：标准 MCP 客户端模拟桩，用于端到端闭环验证
//! - `ClientMetrics`：调用率（100%）、遵循率与延迟自动化度量

pub mod client;

pub use client::{ClientMetrics, ReferenceMcpClient};
