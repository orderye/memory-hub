//! 内嵌作业编排：backup-worker 独立进程与 mcp-server 内嵌模式共用的装配与调度
//!
//! 此前作业装配与 30 秒 tick 主循环只存在于 `bins/backup-worker/src/main.rs`，
//! mcp-server 的内嵌模式（`HUB_EMBEDDED_WORKER=1`）无法复用。本模块把
//! "什么时候跑什么"收口到库层：
//!
//! - [`EmbeddedJobs::new`]：按 `AppConfig` 装配三类作业（备份 / 过期+证据 / 整合扫描）；
//! - [`EmbeddedJobs::run_once_cycle`]：单次手动轮次（过期 → 证据 → 扫描 → 备份），
//!   供 `backup-worker --once` 使用，**不占用周期租约**；
//! - [`EmbeddedJobs::run_periodic_loop`]：常驻周期调度（cron + 30 秒 tick），
//!   供 `backup-worker` 无参与 mcp-server 内嵌模式共用；
//! - [`EmbeddedJobs::with_lease_from`] / [`spawn_embedded`]：多副本租约与后台任务包装。
//!
//! 调度语义与抽出前逐行一致：到点判定基于 `CronSchedule::next_after`，
//! 租约跳过不推进游标，单作业失败只记录不退出。

use crate::lease::JobLease;
use crate::{BackupJob, EvidenceRetentionJob, IntegrationScanJob};
use common::cron::CronSchedule;
use common::{AppConfig, HubError, RateLimitStore, Result};
use mem_core::MemoryHubStore;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};

/// 作业执行者身份（日志与租约身份）
///
/// 内嵌模式下 mcp-server 进程跑的就是 backup-worker 的那套作业：
/// 保持同一身份可让租约桶在两种部署形态间切换时仍指向同一个窗口。
const WORKER_ID: &str = "backup-worker";

/// 常驻模式主循环 tick 间隔（秒）
const TICK_SECS: u64 = 30;

/// 后台作业编排：持有装配好的三类作业与调度计划
pub struct EmbeddedJobs {
    backup_job: BackupJob,
    integration_job: IntegrationScanJob,
    evidence_job: EvidenceRetentionJob,
    schedule: CronSchedule,
    expiry_interval: Option<Duration>,
    scan_max_entries: usize,
    profile_label: String,
}

impl EmbeddedJobs {
    /// 按配置装配作业（不附加租约；多副本部署请再调 [`Self::with_lease_from`]）
    ///
    /// 只接受 PostgreSQL：内存存储没有备份意义，装配期即拒绝（fail-fast）。
    pub fn new(store: Arc<dyn MemoryHubStore>, cfg: &AppConfig) -> Result<Self> {
        if !cfg.is_postgres() {
            return Err(HubError::InvalidArgument(
                "后台作业需要 PostgreSQL（HUB_DB_URL 未配置 postgres 连接串）；\
                 内存存储没有备份意义"
                    .to_string(),
            ));
        }
        // 整合扫描模式来自配置：默认纯本地规则整合（后台作业同样不默认调用向量服务）
        let consolidation_mode = common::ConsolidationMode::parse(&cfg.consolidation_mode)
            .map_err(|e| {
                HubError::InvalidArgument(format!("HUB_CONSOLIDATION_MODE 非法: {}", e))
            })?;
        let consolidation_fallback =
            common::ConsolidationFallback::parse(&cfg.consolidation_fallback).map_err(|e| {
                HubError::InvalidArgument(format!("HUB_CONSOLIDATION_FALLBACK 非法: {}", e))
            })?;
        let consolidation_limits = consolidation::CandidateLimits {
            max_embed_entries: cfg.consolidation_embed_top_k,
            ..consolidation::CandidateLimits::default()
        };
        let mut integration_job = IntegrationScanJob::new(
            store.clone(),
            cfg.integrate_sim_threshold,
            cfg.integrate_max_proposals,
            WORKER_ID,
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
            match embed::build_from_config(cfg) {
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
        let schedule = CronSchedule::parse(&cfg.backup_cron)
            .map_err(|e| HubError::InvalidArgument(format!("BACKUP_CRON 不合法: {}", e)))?;
        let expiry_interval = if cfg.expiry_sweep_interval_h == 0 {
            None
        } else {
            Some(Duration::from_secs(
                u64::from(cfg.expiry_sweep_interval_h) * 3600,
            ))
        };
        Ok(Self {
            backup_job: BackupJob::new(
                store.clone(),
                cfg.backup_keep_count,
                cfg.backup_retention_days,
                WORKER_ID,
            ),
            integration_job,
            evidence_job: EvidenceRetentionJob::new(store, WORKER_ID),
            schedule,
            expiry_interval,
            scan_max_entries: cfg.integration_scan_max_entries,
            profile_label: cfg.deployment_profile.as_str().to_string(),
        })
    }

    /// 附加跨进程作业租约（`rate_limit_buckets` 的"窗口内至多一次"）
    ///
    /// 常驻周期模式与内嵌模式必须启用（多副本互斥）；`--once` 人工触发不启用。
    /// `window_h = 0` 表示关闭租约（`from_hours` 返回 None，作业直接执行）。
    pub fn with_lease_from(mut self, rate_store: Arc<dyn RateLimitStore>, window_h: u32) -> Self {
        if let Some(lease) = JobLease::from_hours(rate_store, window_h) {
            self.backup_job = self.backup_job.with_lease(lease.clone());
            self.integration_job = self.integration_job.with_lease(lease.clone());
            self.evidence_job = self.evidence_job.with_lease(lease);
            info!(window_h, "多副本作业租约已启用");
        }
        self
    }

    /// 单次手动轮次：过期清理 → 证据清理 → 整合扫描 → 备份，返回 JSON 摘要
    pub async fn run_once_cycle(
        &self,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<serde_json::Value> {
        let archived = self.integration_job.expire_stale(now).await?;
        let purged = self.evidence_job.run_once(now).await?;
        let scan = self.integration_job.run_once(now).await?;
        let backup = self.backup_job.run_once(now).await?;
        Ok(serde_json::json!({
            "mode": "once",
            "profile": self.profile_label,
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
        }))
    }

    /// 常驻周期调度：30 秒 tick，到点执行到期的作业
    ///
    /// 正常情况永不返回（`loop`）；返回 `Err` 即调度循环本身不可恢复。
    pub async fn run_periodic_loop(self) -> Result<()> {
        let Self {
            backup_job,
            integration_job,
            evidence_job,
            schedule,
            expiry_interval,
            scan_max_entries,
            ..
        } = self;

        info!(
            cron = %schedule_debug(&schedule),
            expiry_h = expiry_interval.map(|d| d.as_secs() / 3600),
            "作业计划已就绪"
        );

        let mut last_backup = chrono::Utc::now() - chrono::Duration::days(365);
        let mut last_expiry = chrono::Utc::now() - chrono::Duration::days(365);
        let mut last_scan = chrono::Utc::now() - chrono::Duration::days(365);
        let mut last_evidence = chrono::Utc::now() - chrono::Duration::days(365);

        let mut ticker = tokio::time::interval(Duration::from_secs(TICK_SECS));
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
                                max_entries = scan_max_entries,
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
}

/// 构建内嵌作业并以后台任务启动常驻调度（供 mcp-server 调用）
///
/// 作业在独立任务里运行，进程退出即终止；调度循环不可恢复的失败只记录
/// error 日志（与 backup-worker 的"失败只记录不退出"纪律一致，不影响 API 服务）。
pub fn spawn_embedded(
    store: Arc<dyn MemoryHubStore>,
    rate_store: Arc<dyn RateLimitStore>,
    cfg: &AppConfig,
) -> Result<tokio::task::JoinHandle<()>> {
    let jobs = EmbeddedJobs::new(store, cfg)?.with_lease_from(rate_store, cfg.job_lease_h);
    info!(cron = %cfg.backup_cron, "内嵌后台作业调度已启动（HUB_EMBEDDED_WORKER=1）");
    Ok(tokio::spawn(async move {
        if let Err(e) = jobs.run_periodic_loop().await {
            error!(error = %e, "内嵌作业调度循环异常退出");
        }
    }))
}

/// cron 计划的调试展示（next_after 一次作为可读性摘要）
fn schedule_debug(schedule: &CronSchedule) -> String {
    match schedule.next_after(chrono::Utc::now()) {
        Some(next) => format!("next={}", next.to_rfc3339()),
        None => "next=none-within-a-year".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 通过 is_postgres() 检查的假配置（EmbeddedJobs::new 不真正建立连接）
    fn pg_cfg() -> AppConfig {
        AppConfig {
            db_url: "postgresql://u:p@localhost:5432/hub".to_string(),
            ..AppConfig::default()
        }
    }

    fn memory_store() -> Arc<dyn MemoryHubStore> {
        Arc::new(store::InMemoryHubStore::new())
    }

    /// EmbeddedJobs 未实现 Debug，unwrap_err() 不可用；手动解构取 Err
    fn expect_new_error(store: Arc<dyn MemoryHubStore>, cfg: &AppConfig) -> String {
        match EmbeddedJobs::new(store, cfg) {
            Ok(_) => panic!("预期装配失败，实际成功"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn new_rejects_non_postgres_store() {
        let cfg = AppConfig {
            db_url: "memory://local".to_string(),
            ..AppConfig::default()
        };
        let err = expect_new_error(memory_store(), &cfg);
        assert!(err.contains("PostgreSQL"), "{err}");
    }

    #[test]
    fn new_rejects_invalid_backup_cron() {
        let cfg = AppConfig {
            backup_cron: "not a cron".to_string(),
            ..pg_cfg()
        };
        let err = expect_new_error(memory_store(), &cfg);
        assert!(err.contains("BACKUP_CRON"), "{err}");
    }

    #[test]
    fn new_rejects_illegal_consolidation_mode() {
        let cfg = AppConfig {
            consolidation_mode: "bogus".to_string(),
            ..pg_cfg()
        };
        let err = expect_new_error(memory_store(), &cfg);
        assert!(err.contains("HUB_CONSOLIDATION_MODE"), "{err}");
    }

    #[tokio::test]
    async fn spawn_embedded_returns_live_handle_for_pg_config() {
        let cfg = pg_cfg();
        let rate_store: Arc<dyn RateLimitStore> = Arc::new(common::InMemoryRateLimitStore::new());
        let handle = spawn_embedded(memory_store(), rate_store, &cfg).expect("PG 配置必须可装配");
        // 立即中止：测试只验证装配与 spawn 成功，不真跑 30 秒 tick
        handle.abort();
    }
}
