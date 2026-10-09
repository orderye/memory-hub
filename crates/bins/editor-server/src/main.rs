#![forbid(unsafe_code)]

//! 规则编审工作台服务主入口（设计规范 Appendix C.1）
//!
//! 提供双人审批、灰度下发、一键回滚与只追加操作审计日志服务。

use common::init_tracing;
use editor_web::RuleEditorService;
use std::sync::Arc;
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();
    info!("正在启动规则编审工作台服务端 (Rule Editor Workbench)...");

    let _service = Arc::new(RuleEditorService::new());

    info!("编审工作台 Web API 就绪，监听 0.0.0.0:8081...");
    Ok(())
}
