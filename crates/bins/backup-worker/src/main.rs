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
use common::cron::CronSchedule;
use common::init_tracing;
use mem_core::MemoryHubStore;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};
use worker::{BackupJob, EvidenceRetentionJob, IntegrationScanJob, JobLease};

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

    // 备份作业必须落在持久化存储上
    let (store, rate_store) = build_store(&cfg).await?;

    // 跨进程租约：--once 是人工显式触发，不占用周期租约
    let lease = if run_mode == RunMode::Once {
        None
    } else {
        JobLease::from_hours(rate_store.clone(), cfg.job_lease_h)
    };

    // ===== 作业装配 =====
    let mut backup_job = BackupJob::new(
        store.clone(),
        cfg.backup_keep_count,
        cfg.backup_retention_days,
        "backup-worker",
    );
    // 整合扫描模式来自配置：默认纯本地规则整合（后台作业同样不默认调用向量服务）
    let consolidation_mode = common::ConsolidationMode::parse(&cfg.consolidation_mode)
        .map_err(|e| anyhow!("HUB_CONSOLIDATION_MODE 非法: {}", e))?;
    let consolidation_fallback = common::ConsolidationFallback::parse(&cfg.consolidation_fallback)
        .map_err(|e| anyhow!("HUB_CONSOLIDATION_FALLBACK 非法: {}", e))?;
    let consolidation_limits = consolidation::CandidateLimits {
        max_embed_entries: cfg.consolidation_embed_top_k,
        ..consolidation::CandidateLimits::default()
    };
    let mut integration_job = IntegrationScanJob::new(
        store.clone(),
        cfg.integrate_sim_threshold,
        cfg.integrate_max_proposals,
        "backup-worker",
    )
    .with_mode(
        consolidation_mode,
        consolidation_fallback,
        consolidation_limits,
        cfg.consolidation_lexical_min_similarity,
    )
    // 有界扫描：单命名空间一次最多装载 N 条，报告里标 truncated
    .with_scan_limit(cfg.integration_scan_max_entries);
    // 向量模式才注入嵌入器：纯规则模式下后台作业一次嵌入调用都不会发出。
    // 嵌入被显式关闭（HUB_EMBED_PROVIDER=off）时同样不注入。
    if consolidation_mode == common::ConsolidationMode::Hybrid && cfg.embedding_enabled() {
        match embed::build_from_config(&cfg) {
            Ok(embedder) => {
                info!("向量整合模式：后台扫描已装配嵌入器");
                integration_job = integration_job.with_embedder(embedder);
            }
            Err(err) => {
                warn!(
                    error = %err,
                    "向量整合模式已开启但嵌入器不可用；扫描将按 HUB_CONSOLIDATION_FALLBACK 处理"
                );
            }
        }
    }
    // 证据保留期限：不可变证据行到期即物理删除
    let mut evidence_job = EvidenceRetentionJob::new(store.clone(), "backup-worker");

    if let Some(lease) = &lease {
        backup_job = backup_job.with_lease(lease.clone());
        integration_job = integration_job.with_lease(lease.clone());
        evidence_job = evidence_job.with_lease(lease.clone());
        info!(window_h = cfg.job_lease_h, "多副本作业租约已启用");
    }

    // ===== 单次手动执行 =====
    if run_mode == RunMode::Once {
        let now = chrono::Utc::now();
        let archived = integration_job.expire_stale(now).await?;
        let purged = evidence_job.run_once(now).await?;
        let scan = integration_job.run_once(now).await?;
        let backup = backup_job.run_once(now).await?;
        let summary = serde_json::json!({
            "mode": "once",
            "profile": cfg.deployment_profile.as_str(),
            "executed_at": now.to_rfc3339(),
            "expired_archived": archived,
            "evidence_purged": purged,
            "integration": {
                "scanned": scan.scanned,
                "proposals": scan.proposals,
                "skipped_leased": scan.skipped_leased,
                "truncated": scan.truncated_namespaces,
                "failed": scan.failed_namespaces,
            },
            "backup": {
                "created": backup.created.len(),
                "pruned": backup.pruned.len(),
                "skipped_leased": backup.skipped_leased,
            },
        });
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(());
    }

    // ===== 常驻周期调度 =====
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
        scan_max_entries = cfg.integration_scan_max_entries,
        "作业计划已就绪"
    );

    let mut last_backup = chrono::Utc::now() - chrono::Duration::days(365);
    let mut last_expiry = chrono::Utc::now() - chrono::Duration::days(365);
    let mut last_scan = chrono::Utc::now() - chrono::Duration::days(365);
    let mut last_evidence = chrono::Utc::now() - chrono::Duration::days(365);

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
                        if report.skipped_leased {
                            // **不推进 last_backup**：推进等于"本轮已处理"，而实际什么都没做。
                            // 租约窗口是连续补充的（cap/window），下次到点若差几毫秒没补满令牌
                            // 就会被拒绝——推进游标会让单副本部署稳定隔天备份一次。
                            // 保持到点状态，下个 tick 重试，令牌补满后立即执行。
                            info!("定时备份被租约跳过（其它副本已执行），下个 tick 重试");
                        } else {
                            info!(
                                snapshots = report.created.len(),
                                pruned = report.pruned.len(),
                                "定时备份完成"
                            );
                            last_backup = now;
                        }
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

        // 3. 证据保留期限清理（跟随过期清理节奏）
        if expiry_interval.is_some()
            && (now - last_evidence)
                >= chrono::Duration::from_std(expiry_interval.unwrap())
                    .unwrap_or(chrono::Duration::hours(6))
        {
            match evidence_job.run_once(now).await {
                Ok(purged) => {
                    if purged > 0 {
                        info!(purged, "过期证据清理完成");
                    }
                    last_evidence = now;
                }
                Err(e) => error!(error = %e, "过期证据清理失败（下个 tick 重试）"),
            }
        }

        // 4. 整合扫描（跟随备份节奏：每天最多一次）
        if (now - last_scan) >= chrono::Duration::hours(24) {
            match integration_job.run_once(now).await {
                Ok(report) => {
                    let total: usize = report.proposals.values().sum();
                    if total > 0 {
                        info!(proposals = total, "整合扫描发现新提案");
                    }
                    if !report.skipped_leased.is_empty() {
                        info!(
                            skipped = report.skipped_leased.len(),
                            "部分命名空间因租约被其它副本扫描，本轮跳过"
                        );
                    }
                    if !report.failed_namespaces.is_empty() {
                        warn!(
                            failed = report.failed_namespaces.len(),
                            "部分命名空间整合扫描失败（下个窗口重试）"
                        );
                    }
                    if !report.truncated_namespaces.is_empty() {
                        warn!(
                            truncated = report.truncated_namespaces.len(),
                            max_entries = cfg.integration_scan_max_entries,
                            "部分命名空间达到扫描条目上限，只覆盖最近更新的条目"
                        );
                    }
                    last_scan = now;
                }
                Err(e) => error!(error = %e, "整合扫描失败（下个 tick 重试）"),
            }
        }
    }
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
