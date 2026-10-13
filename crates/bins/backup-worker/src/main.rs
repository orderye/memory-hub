#![forbid(unsafe_code)]

//! 多智能体长期记忆服务备份作业主入口
//!
//! 三类周期作业（全部幂等）：
//! 1. `BACKUP_CRON`（5 段 cron）到点：为全部命名空间创建快照 + 保留策略；
//! 2. 过期清理：按 `EXPIRY_SWEEP_INTERVAL_H` 小时归档到期条目；
//! 3. 整合扫描：识别同 key 冲突并生成提案（不自动合并）。
//!
//! 单 tick 调度：每 30 秒评估一次是否到点；作业执行失败只记录不退出，
//! 保证长寿命进程可用。

use anyhow::{anyhow, Result};
use common::cron::CronSchedule;
use common::init_tracing;
use mem_core::MemoryHubStore;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info};
use worker::{BackupJob, IntegrationScanJob};

#[cfg(all(feature = "jemalloc", not(target_env = "msvc")))]
use tikv_jemallocator::Jemalloc;

#[cfg(all(feature = "jemalloc", not(target_env = "msvc")))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rt = common::build_runtime()?;
    rt.block_on(async_main())
}

async fn async_main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();

    let cfg = common::AppConfig::from_env()?;
    info!(config = %cfg.redacted_summary(), "记忆中枢 Backup Worker 启动中");

    // 备份作业必须落在持久化存储上
    let store: Arc<dyn MemoryHubStore> = build_store(&cfg).await?;

    // ===== 作业装配 =====
    let backup_job = Arc::new(BackupJob::new(
        store.clone(),
        cfg.backup_keep_count,
        cfg.backup_retention_days,
        "backup-worker",
    ));
    let integration_job = Arc::new(IntegrationScanJob::new(
        store.clone(),
        cfg.integrate_sim_threshold,
        cfg.integrate_max_proposals,
        "backup-worker",
    ));

    // 备份调度表（启动期 fail-fast 已由配置校验完成，这里再解析一次拿到句柄）
    let schedule =
        CronSchedule::parse(&cfg.backup_cron).map_err(|e| anyhow!("BACKUP_CRON 不合法: {}", e))?;
    let expiry_interval = if cfg.expiry_sweep_interval_h == 0 {
        None
    } else {
        Some(Duration::from_secs(
            u64::from(cfg.expiry_sweep_interval_h) * 3600,
        ))
    };

    info!(
        cron = %cfg.backup_cron,
        keep = cfg.backup_keep_count,
        retention_days = cfg.backup_retention_days,
        expiry_h = cfg.expiry_sweep_interval_h,
        "作业计划已就绪"
    );

    let mut last_backup = chrono::Utc::now() - chrono::Duration::days(365);
    let mut last_expiry = chrono::Utc::now() - chrono::Duration::days(365);
    let mut last_scan = chrono::Utc::now() - chrono::Duration::days(365);

    // 主循环：30 秒 tick；到点判定基于 cron next_after 与心跳间隔
    let mut ticker = tokio::time::interval(Duration::from_secs(30));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        ticker.tick().await;
        let now = chrono::Utc::now();

        // 1. 定时备份
        if let Some(next) = schedule.next_after(last_backup) {
            if next <= now {
                match backup_job.run_once(now).await {
                    Ok(report) => {
                        info!(
                            snapshots = report.created.len(),
                            pruned = report.pruned.len(),
                            "定时备份完成"
                        );
                        last_backup = now;
                    }
                    Err(e) => error!(error = %e, "定时备份失败（下个 tick 重试）"),
                }
            }
        }

        // 2. 过期清理
        if expiry_interval.is_some()
            && (now - last_expiry)
                >= chrono::Duration::from_std(expiry_interval.unwrap())
                    .unwrap_or(chrono::Duration::hours(6))
        {
            match integration_job.expire_stale(now).await {
                Ok(archived) => {
                    if archived > 0 {
                        info!(archived, "过期记忆归档完成");
                    }
                    last_expiry = now;
                }
                Err(e) => error!(error = %e, "过期清理失败（下个 tick 重试）"),
            }
        }

        // 3. 整合扫描（跟随备份节奏：每天最多一次）
        if (now - last_scan) >= chrono::Duration::hours(24) {
            match integration_job.run_once(now).await {
                Ok(report) => {
                    let total: usize = report.proposals.values().sum();
                    if total > 0 {
                        info!(proposals = total, "整合扫描发现新提案");
                    }
                    last_scan = now;
                }
                Err(e) => error!(error = %e, "整合扫描失败（下个 tick 重试）"),
            }
        }
    }
}

/// 构建存储：备份作业只接受 PostgreSQL（内存存储没有备份意义）
async fn build_store(cfg: &common::AppConfig) -> Result<Arc<dyn MemoryHubStore>> {
    let is_pg = cfg.db_url.starts_with("postgres://") || cfg.db_url.starts_with("postgresql://");
    if !is_pg {
        return Err(anyhow!(
            "backup-worker 需要 PostgreSQL（HUB_DB_URL 未配置 postgres 连接串）"
        ));
    }
    let pg = store::PostgresHubStore::connect(
        &cfg.db_url,
        cfg.db_max_connections,
        cfg.db_statement_timeout_ms,
    )
    .await
    .map_err(|e| anyhow!("PostgreSQL 连接失败: {}", e))?;
    info!("PostgreSQL 存储连接成功（含迁移执行）");
    Ok(Arc::new(pg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mem_core::MemoryStore;

    #[test]
    fn test_tick_arithmetic_is_sane() {
        // 到期清理间隔换算
        let interval = Duration::from_secs(u64::from(6u32) * 3600);
        assert_eq!(interval.as_secs(), 21600);
        let as_chrono = chrono::Duration::from_std(interval).unwrap();
        assert_eq!(as_chrono.num_hours(), 6);
    }

    #[tokio::test]
    async fn test_memory_store_alias_compiles() {
        // 确保 MemoryStore 导入在主模块中被使用（archive_expired 路径）
        fn assert_trait<T: MemoryStore>() {}
        assert_trait::<store::InMemoryHubStore>();
    }
}
