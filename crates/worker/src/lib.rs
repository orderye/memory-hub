//! 多智能体长期记忆服务后台作业库（worker）
//!
//! 三类周期作业，全部幂等、可安全重入：
//! - **定时备份**：按 cron 表达式为全部命名空间创建快照，并执行保留策略；
//! - **过期清理**：归档 `expires_at` 已到的条目（带版本历史）；
//! - **整合扫描**：定期识别重复/冲突并生成提案（不自动合并）；
//! - **证据清理**：物理删除超过保留期限的不可变证据行。
//!
//! 调度器只依赖 `Clock` 与 cron 解析（`common::cron`），可整体注入测试时钟。

pub mod backup_job;
pub mod cognitive_job;
pub mod evidence_job;
pub mod experience_job;
pub mod integration_job;
pub mod lease;
pub(crate) mod retry;
pub mod scheduler;

pub use backup_job::{BackupJob, BackupJobReport};
pub use cognitive_job::{ChatCognitiveJob, ChatCognitiveJobReport};
pub use evidence_job::EvidenceRetentionJob;
pub use experience_job::{ExperienceDistillJob, ExperienceDistillReport};
pub use integration_job::{IntegrationScanJob, IntegrationScanReport};
pub use lease::JobLease;
pub use scheduler::{JobScheduler, ScheduledTask};
