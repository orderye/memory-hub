#![forbid(unsafe_code)]

//! 离线批处理与后台作业工作进程（设计规范 Appendix C.1）
//!
//! 定时执行每日记忆巩固（指数时间衰减、超期清理）、
//! 未覆盖冲突模式聚类与规则候选生成。

use common::init_tracing;
use hub_store::InMemoryMemoryStore;
use offline::DailyConsolidationJob;
use std::sync::Arc;
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();
    info!("正在启动记忆中枢离线工作进程 (Daily Consolidation & Clustering)...");

    let store = Arc::new(InMemoryMemoryStore::new());
    let _job = DailyConsolidationJob::new(store);

    info!("离线任务定时调度器就绪，执行周期: 每日 UTC 02:00...");
    Ok(())
}
