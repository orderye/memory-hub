//! 整合服务：跨 Agent 写入的去重与冲突归并契约
//!
//! 多 Agent 并发写入同一命名空间时，不可避免出现：
//! - **重复**（duplicate）：内容高度相似（余弦相似度 ≥ 阈值）的条目；
//! - **冲突**（conflict）：同一业务 key 下内容不一致的条目。
//!
//! 本模块定义"识别 + 提案 + 按提案归并"的契约接口。
//! 具体实现统一由 `consolidation::DefaultIntegrationService` 委托 `ConsolidationEngine` 完成。

use crate::domain::{IntegrationProposal, MemoryEntry};
use async_trait::async_trait;
use common::{AuthCtx, ConsolidationMode, ConsolidationModeStatus, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 扫描请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanRequest {
    pub namespace: String,
    /// true = 只预演不落提案
    #[serde(default)]
    pub dry_run: bool,
    /// 覆盖默认阈值（None 用服务配置）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sim_threshold: Option<f32>,
    /// 覆盖候选发现模式（None 用服务配置）。
    ///
    /// 该字段只有在服务端开启 `HUB_CONSOLIDATION_ALLOW_MODE_OVERRIDE=true` 时才会被采纳；
    /// 否则请求中的显式模式会被拒绝，调用方应改用部署级配置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<ConsolidationMode>,
}

/// 扫描报告
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanReport {
    pub namespace: String,
    pub scanned: usize,
    pub dry_run: bool,
    /// 本次新建的提案（dry_run 时为预演结果）
    pub proposals: Vec<IntegrationProposal>,
    /// 因已有 open 提案而跳过的组数
    pub skipped_existing: usize,
    /// 本次运行的候选策略状态（请求模式 / 实际模式 / 降级 / embedding 调用计数）
    pub mode: ConsolidationModeStatus,
}

/// 提案裁决动作
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveAction {
    /// 合并：保留 survivor，其余成员标记 merged 并指向 survivor
    Merge,
    /// 驳回：保留分立（后续扫描仍可再次提案）
    Dismiss,
}

/// 裁决请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolveRequest {
    pub proposal_id: Uuid,
    pub action: ResolveAction,
    /// merge 时的幸存条目（None 用提案建议值；提案无建议值时取最新成员）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub survivor_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// 整合服务契约
#[async_trait]
pub trait IntegrationService: Send + Sync {
    async fn scan(&self, auth: &AuthCtx, req: ScanRequest) -> Result<ScanReport>;
    async fn list(&self, auth: &AuthCtx, namespace: &str) -> Result<Vec<IntegrationProposal>>;
    async fn resolve(&self, auth: &AuthCtx, req: ResolveRequest) -> Result<MemoryEntry>;
    /// 归档过期条目（worker 定期调用）
    async fn expire_stale(&self, now: chrono::DateTime<chrono::Utc>) -> Result<usize>;
}
