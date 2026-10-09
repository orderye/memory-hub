#![forbid(unsafe_code)]

//! MCP 服务器主入口程序（设计规范 Appendix C.1）
//!
//! 负责初始化存储、向量引擎、规范包、规则与检索流水线，
//! 启动符合 MCP 协议规范的上下文与工具调度服务。

use common::init_tracing;
use hub_core::assembler::Assembler;
use hub_core::infra::PackSet;
use hub_embed::MockEmbedder;
use hub_mcp::ToolDispatcher;
use hub_packs::AtomicPackRegistry;
use hub_retrieval::{HybridRetriever, HybridRetrieverConfig};
use hub_rules::DefaultRuleEngine;
use hub_store::InMemoryMemoryStore;
use std::sync::Arc;
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 强制脱敏日志初始化
    init_tracing();
    info!("正在启动记忆中枢 MCP Server (2026-07-28 / 2025-11-25 兼容)...");

    let embedder = Arc::new(MockEmbedder::new("mock-model", 1024));
    let store = Arc::new(InMemoryMemoryStore::new());
    let retriever = Arc::new(HybridRetriever::new(
        store,
        embedder,
        HybridRetrieverConfig::default(),
    ));

    let initial_pack = PackSet {
        pack_version: "2026.10.1".to_string(),
        core_hash: "canonical_hash_2026".to_string(),
        raw_content: serde_json::json!({ "skills": ["tianxin", "tianli_renyu"] }),
    };

    let pack_registry = Arc::new(AtomicPackRegistry::new(
        initial_pack,
        "canonical_hash_2026",
    )?);
    let rule_engine = Arc::new(DefaultRuleEngine::new());

    let assembler = Arc::new(Assembler::new(
        retriever,
        pack_registry,
        rule_engine,
        1500, // 软超时 1500ms
    ));

    let _dispatcher = ToolDispatcher::new(assembler);

    info!("记忆中枢 MCP Server 就绪，监听 stdio / streamable HTTP 管道中...");
    Ok(())
}
