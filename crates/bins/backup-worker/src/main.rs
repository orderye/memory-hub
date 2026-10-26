#![forbid(unsafe_code)]

//! 多智能体长期记忆服务备份作业主入口
//!
//! 两种运行形态：
//!
//! 1. **常驻周期模式**（默认，Production）：`BACKUP_CRON` 到点备份 + `EXPIRY_SWEEP_INTERVAL_H`
//!    小时过期清理与证据清理 + 每 24 小时一次整合扫描；所有作业幂等，失败只记录不退出。
//! 2. **单次手动模式**（`--once`，Lite 与运维手工触发）：按"过期清理 → 证据清理 → 整合扫描 →
//!    备份"顺序各执行一次，打印 JSON 摘要后退出；**完全不走 cron、不占用周期租约**。
//!
//! 部署模式的差异只有一处，且是硬约束：`HUB_DEPLOYMENT_PROFILE=lite` 时**不允许常驻**
//! （个人部署不该养一个空转的定时进程），必须带 `--once` 手动触发，否则启动即失败。
//!
//! 多副本部署时，同一作业通过 `common::RateLimitStore` 的"窗口内至多一次"实现跨进程互斥
//! （`HUB_JOB_LEASE_H`，复用 `rate_limit_buckets` 表，无需额外迁移）。

use anyhow::{anyhow, Result};
use common::init_tracing;
use mem_core::MemoryHubStore;
use std::sync::Arc;
use tracing::info;
use worker::embedded::EmbeddedJobs;

#[cfg(all(feature = "jemalloc", not(target_env = "msvc")))]
use tikv_jemallocator::Jemalloc;

#[cfg(all(feature = "jemalloc", not(target_env = "msvc")))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

/// 运行形态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunMode {
    /// 常驻周期调度（Production 默认）
    Periodic,
    /// 手动执行一轮后退出
    Once,
    /// 打印用法
    Help,
}

/// 解析命令行参数（不引入 clap：只有一个开关，副作用不值得多一个依赖）
fn parse_args(args: impl IntoIterator<Item = String>) -> Result<RunMode> {
    let mut mode = RunMode::Periodic;
    for arg in args {
        match arg.as_str() {
            "--once" => mode = RunMode::Once,
            "-h" | "--help" => return Ok(RunMode::Help),
            other => return Err(anyhow!("未知参数: {other}（支持 --once | --help）")),
        }
    }
    Ok(mode)
}

fn print_usage() {
    println!(
        "用法: backup-worker [--once]\n\n  （无参数）周期调度：按 BACKUP_CRON / EXPIRY_SWEEP_INTERVAL_H 常驻运行\n  --once     手动执行一轮（过期清理 → 证据清理 → 整合扫描 → 备份）后退出\n  --help     显示本帮助\n\n环境变量见 .env.example；HUB_DEPLOYMENT_PROFILE=lite 时只允许 --once。"
    );
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rt = common::build_runtime()?;
    rt.block_on(async_main())
}

async fn async_main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();

    let run_mode = parse_args(std::env::args().skip(1))?;
    if run_mode == RunMode::Help {
        print_usage();
        return Ok(());
    }

    let cfg = common::AppConfig::from_env()?;
    info!(
        config = %cfg.redacted_summary(),
        profile = %cfg.deployment_profile.as_str(),
        mode = if run_mode == RunMode::Once { "once" } else { "periodic" },
        "记忆中枢 Backup Worker 启动中"
    );

    // Lite 不允许常驻：个人部署的周期清扫应改为手动触发，避免空转进程与重复扫描
    if cfg.deployment_profile.is_lite() && run_mode == RunMode::Periodic {
        eprintln!(
            "[backup-worker] HUB_DEPLOYMENT_PROFILE=lite 不支持常驻周期调度：\
             请使用 `backup-worker --once` 手动执行，或改用 production 模式。"
        );
        std::process::exit(2);
    }

    // 备份作业必须落在持久化存储上（EmbeddedJobs::new 内部同样强制）
    let (store, rate_store) = build_store(&cfg).await?;

    let jobs = EmbeddedJobs::new(store, &cfg)?;

    // ===== 单次手动执行（--once 是人工显式触发，不占用周期租约）=====
    if run_mode == RunMode::Once {
        let summary = jobs.run_once_cycle(chrono::Utc::now()).await?;
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(());
    }

    // ===== 常驻周期调度 =====
    jobs.with_lease_from(rate_store, cfg.job_lease_h)
        .run_periodic_loop()
        .await?;
    Ok(())
}

/// 构建存储与作业租约存储
///
/// 备份作业只接受 PostgreSQL（内存存储没有备份意义）；租约存储复用同一个连接池。
async fn build_store(
    cfg: &common::AppConfig,
) -> Result<(Arc<dyn MemoryHubStore>, Arc<dyn common::RateLimitStore>)> {
    if !cfg.is_postgres() {
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
    let rate_store: Arc<dyn common::RateLimitStore> =
        Arc::new(store::PostgresRateLimitStore::from_pool(pg.pool()));
    Ok((Arc::new(pg), rate_store))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mem_core::MemoryStore;
    use std::time::Duration;

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

    #[test]
    fn test_parse_args_defaults_to_periodic() {
        assert_eq!(parse_args(Vec::<String>::new()).unwrap(), RunMode::Periodic);
    }

    #[test]
    fn test_parse_args_once() {
        assert_eq!(
            parse_args(vec!["--once".to_string()]).unwrap(),
            RunMode::Once
        );
        assert_eq!(
            parse_args(vec!["--help".to_string()]).unwrap(),
            RunMode::Help
        );
    }

    #[test]
    fn test_parse_args_rejects_unknown() {
        let err = parse_args(vec!["--daemon".to_string()]).unwrap_err();
        assert!(err.to_string().contains("--once"), "{err}");
    }
}
