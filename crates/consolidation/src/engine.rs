//! 整合中枢核心引擎：统一协调候选发现、关系分类、提案与策略调度
//!
//! 流水线（模式差异只在候选层）：
//!
//! ```text
//! 解析有效模式（requested → effective，含降级）
//!   → 读取候选空间（账户/命名空间/有效期/状态）
//!   → 候选发现（Rules 或 Hybrid）——每次扫描只算一次
//!   → 关系分类与冲突检查（共享决策层）
//!   → 自动合并安全门槛 / 待审提案
//!   → 证据、审计、统计与版本历史
//! ```

use crate::candidates::hybrid::HybridCandidateFinder;
use crate::candidates::rules::RulesCandidateFinder;
use crate::candidates::{CandidateContext, CandidateFinder, CandidateLimits, CandidateThresholds};
use crate::detectors::{ConflictDetector, DuplicateDetector, StalenessDetector};
use crate::merger::{ContentMerger, TieredMerger};
use crate::policy::AutoPolicy;
use crate::types::{ConsolidationReport, ConsolidationStats};
use chrono::Utc;
use common::{
    AgentId, ConsolidationFallback, ConsolidationMode, ConsolidationModeStatus, HubError, Result,
};
use mem_core::{
    Embedder, IntegrationKind, IntegrationProposal, MemoryEntry, MemoryHubStore, MemoryStatus,
    ProposalStatus, ResolveAction, ResolveRequest, VersionChange,
};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use uuid::Uuid;

/// 命名空间级互斥锁登记表
///
/// 同一进程内对同一命名空间的整合扫描串行化，避免并发扫描重复产出互相冲突的提案。
/// 跨进程并发（MCP 进程与 backup-worker 进程同时扫同一命名空间）无法用进程内锁覆盖，
/// 彻底方案是数据库侧的部分唯一索引，属于后续单独评估项。
static NAMESPACE_LOCKS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    OnceLock::new();

fn namespace_lock(namespace: &str) -> Arc<tokio::sync::Mutex<()>> {
    let registry = NAMESPACE_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = registry.lock().unwrap_or_else(|e| e.into_inner());
    map.entry(namespace.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// 一次扫描的分析结果（候选只算一次，供统计与提案共用）
struct Analysis {
    conflicts: Vec<crate::types::ConflictGroup>,
    duplicates: Vec<crate::types::DuplicateGroup>,
    status: ConsolidationModeStatus,
}

/// 整合中枢引擎
#[derive(Clone)]
pub struct ConsolidationEngine {
    store: Arc<dyn MemoryHubStore>,
    duplicate_detector: DuplicateDetector,
    conflict_detector: ConflictDetector,
    staleness_detector: StalenessDetector,
    merger: Arc<dyn ContentMerger>,
    policy: AutoPolicy,
    max_proposals: usize,
    /// 合并后重算幸存者向量用；未配置时合并会清空旧向量而不是留下与正文不符的旧向量
    embedder: Option<Arc<dyn Embedder>>,
    /// 候选发现模式（默认纯本地规则）
    mode: ConsolidationMode,
    /// 向量模式不可用时的降级策略
    fallback: ConsolidationFallback,
    /// 候选规模限制
    limits: CandidateLimits,
    /// 词法候选阈值
    lexical_min_similarity: f32,
    /// 向量候选的相似度下限（None = 未经真实模型标定）
    embed_min_similarity: Option<f32>,
    /// 单次扫描最多装载的活跃条目数（0 = 不限制）
    ///
    /// 整合是"把一个命名空间的条目全读进来做 O(n²) 级候选发现"的操作，
    /// 没有上限时一个大命名空间就能把进程内存顶满。默认由调用方按部署形态注入
    /// （`HUB_INTEGRATION_SCAN_MAX_ENTRIES`）；0 保留给既有单元测试与显式全量扫描。
    scan_limit: usize,
}

impl ConsolidationEngine {
    pub fn new(store: Arc<dyn MemoryHubStore>, sim_threshold: f32, max_proposals: usize) -> Self {
        Self {
            store,
            duplicate_detector: DuplicateDetector::new(sim_threshold),
            // 阈值梯度统一：语义冲突 0.82 < 去重/整合 sim_threshold < 自动合并 0.98
            conflict_detector: ConflictDetector::default().with_duplicate_threshold(sim_threshold),
            staleness_detector: StalenessDetector::default(),
            merger: Arc::new(TieredMerger::default()),
            policy: AutoPolicy::default(),
            max_proposals: max_proposals.max(1),
            embedder: None,
            mode: ConsolidationMode::Rules,
            fallback: ConsolidationFallback::Rules,
            limits: CandidateLimits::default(),
            lexical_min_similarity: CandidateThresholds::default().lexical_min_similarity,
            embed_min_similarity: None,
            scan_limit: 0,
        }
    }

    /// 设置单次扫描的活跃条目上限（0 = 不限制）
    ///
    /// 超限时只扫描**最近更新**的前 N 条，并在报告里置 `truncated=true`：
    /// 静默截断会让调用方误以为"整个命名空间都扫过了"。
    ///
    /// 上限 1 没有意义（永远凑不成一对候选），这里抬到 2，与配置层校验保持一致。
    pub fn with_scan_limit(mut self, max_entries: usize) -> Self {
        self.scan_limit = if max_entries == 1 { 2 } else { max_entries };
        self
    }

    /// 注入嵌入器：合并后重算向量
    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    /// 设置候选发现模式与降级策略
    pub fn with_mode(mut self, mode: ConsolidationMode, fallback: ConsolidationFallback) -> Self {
        self.mode = mode;
        self.fallback = fallback;
        self
    }

    /// 设置候选规模限制
    pub fn with_candidate_limits(mut self, limits: CandidateLimits) -> Self {
        self.limits = limits;
        self
    }

    /// 设置词法候选阈值
    pub fn with_lexical_min_similarity(mut self, threshold: f32) -> Self {
        self.lexical_min_similarity = threshold.clamp(0.0, 1.0);
        self
    }

    /// 设置向量候选的相似度下限
    ///
    /// `None` 表示**未经真实模型标定**：此时回落到去重/整合阈值，
    /// 并在运行状态里标记 `similarity_threshold_calibrated=false`。
    pub fn with_embed_min_similarity(mut self, threshold: Option<f32>) -> Self {
        self.embed_min_similarity = threshold.map(|t| t.clamp(0.0, 1.0));
        self
    }

    pub fn with_policy(mut self, policy: AutoPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_merger(mut self, merger: Arc<dyn ContentMerger>) -> Self {
        self.merger = merger;
        self
    }

    /// 切换底层存储（例如切换为特定请求的账户隔离存储 AccountScopedStore）
    pub fn with_store(&self, store: Arc<dyn MemoryHubStore>) -> Self {
        let mut cloned = self.clone();
        cloned.store = store;
        cloned
    }

    /// 当前配置的模式
    pub fn mode(&self) -> ConsolidationMode {
        self.mode
    }

    /// 本引擎是否允许调用嵌入服务
    ///
    /// **只有向量模式、嵌入已启用、且嵌入器提供真实语义信号时才允许**。
    /// 纯规则模式下这是结构性保证：整合链路一次嵌入调用都不会发出。
    ///
    /// 三个条件缺一不可：`is_enabled` 回答"要不要花一次调用"（`off` 时为 false），
    /// `provides_semantic_signal` 回答"这些向量能不能当语义用"（伪向量为 false）。
    /// 只用后者会导致"启用但无信号"的嵌入器在 off 场景下仍然发请求再失败。
    fn embedding_allowed(&self) -> bool {
        self.mode == ConsolidationMode::Hybrid
            && self
                .embedder
                .as_ref()
                .map(|e| e.is_enabled() && e.provides_semantic_signal())
                .unwrap_or(false)
    }

    /// 解析有效模式：请求模式 → 实际模式（含降级）
    fn resolve_mode(&self) -> Result<(Arc<dyn CandidateFinder>, ConsolidationModeStatus)> {
        match self.mode {
            ConsolidationMode::Rules => Ok((
                Arc::new(RulesCandidateFinder::new()),
                ConsolidationModeStatus::rules(),
            )),
            ConsolidationMode::Hybrid => {
                let Some(embedder) = self.embedder.clone() else {
                    return self.apply_fallback("embedding_provider_not_configured");
                };
                if !embedder.is_enabled() {
                    return self.apply_fallback("embedding_disabled");
                }
                if !embedder.provides_semantic_signal() {
                    return self.apply_fallback("embedding_provider_not_ready");
                }
                Ok((
                    Arc::new(HybridCandidateFinder::new(embedder)),
                    ConsolidationModeStatus::hybrid(0, 0),
                ))
            }
        }
    }

    /// 降级处理：`rules` 降级为规则模式；`error` 直接失败且不执行任何整合
    fn apply_fallback(
        &self,
        reason: &str,
    ) -> Result<(Arc<dyn CandidateFinder>, ConsolidationModeStatus)> {
        match self.fallback {
            ConsolidationFallback::Rules => {
                tracing::warn!(
                    requested_mode = "hybrid",
                    reason,
                    "向量整合不可用，按配置降级为纯规则整合"
                );
                Ok((
                    Arc::new(RulesCandidateFinder::new()),
                    ConsolidationModeStatus::fallback_to_rules(reason),
                ))
            }
            ConsolidationFallback::Error => Err(HubError::Internal(anyhow::anyhow!(
                "向量整合不可用（{}），按 HUB_CONSOLIDATION_FALLBACK=error 拒绝执行整合",
                reason
            ))),
        }
    }

    /// 构造候选阈值
    ///
    /// 阈值梯度必须单调：语义冲突下界 < 去重/整合阈值 < 自动合并阈值。
    /// 请求覆盖的去重阈值若低于语义冲突下界，会让"相似到 0.92 以上算重复、
    /// 而不是冲突"这条规则失效，因此这里强制抬到语义阈值之上。
    fn thresholds(&self, sim_threshold: Option<f32>) -> CandidateThresholds {
        let semantic = self.conflict_detector.semantic_sim_threshold;
        let duplicate = sim_threshold
            .unwrap_or(self.duplicate_detector.sim_threshold)
            .clamp(0.0, 1.0)
            .max(semantic + 0.01);
        CandidateThresholds {
            embed_min_similarity: self.embed_min_similarity,
            lexical_min_similarity: self.lexical_min_similarity,
            duplicate_sim_threshold: duplicate,
            semantic_sim_threshold: semantic,
            divergence_threshold: self.conflict_detector.divergence_threshold,
        }
    }

    /// 载入命名空间内的活跃条目，返回 `(条目, 是否被上限截断)`
    ///
    /// - `scan_limit == 0`：保持历史行为，走 `all_in_namespace` 全量读取
    ///   （内存峰值随命名空间线性增长，只有显式要求时才用）；
    /// - `scan_limit > 0`：按 `updated_at` 倒序取前 N+1 条，多取一条用于判断
    ///   "是否还有更多"，并在报告里如实标 `truncated`——只做一次存储调用，
    ///   不会把整表装进内存。
    async fn load_active(&self, namespace: &str) -> Result<(Vec<MemoryEntry>, bool)> {
        if self.scan_limit == 0 {
            let entries = self
                .store
                .all_in_namespace(namespace, false)
                .await?
                .into_iter()
                .filter(|e| e.status == MemoryStatus::Active)
                .collect::<Vec<_>>();
            return Ok((entries, false));
        }

        let filter = mem_core::MemoryFilter {
            namespace: Some(namespace.to_string()),
            statuses: Some(vec![MemoryStatus::Active]),
            limit: Some(self.scan_limit.saturating_add(1)),
            ..Default::default()
        };
        // 存储层过滤条件必须与服务层一致：账户作用域由 store 包装负责
        let mut entries = self.store.list(&filter).await?;
        let truncated = entries.len() > self.scan_limit;
        if truncated {
            entries.truncate(self.scan_limit);
        }
        entries.retain(|e| e.status == MemoryStatus::Active);
        Ok((entries, truncated))
    }

    /// 一次分析：候选只算一次，统计与提案共用
    async fn analyze(
        &self,
        namespace: &str,
        entries: &[MemoryEntry],
        sim_threshold: Option<f32>,
    ) -> Result<Analysis> {
        let (finder, mut status) = self.resolve_mode()?;
        let thresholds = self.thresholds(sim_threshold);
        status.similarity_threshold_calibrated = thresholds.embed_threshold_calibrated();

        if entries.len() < 2 {
            return Ok(Analysis {
                conflicts: Vec::new(),
                duplicates: Vec::new(),
                status,
            });
        }

        let ctx = CandidateContext {
            store: self.store.as_ref(),
            namespace,
            entries,
            thresholds,
            limits: self.limits,
        };
        let outcome = finder.find_candidates(&ctx).await?;
        status.embedding_calls = outcome.embedding_calls;
        status.embedded_entries = outcome.embedded_entries;
        if status.effective_mode == ConsolidationMode::Hybrid.as_str() {
            status.embedding_used = true;
            status.candidate_strategy = "local_rules+embedding".to_string();
        }

        let conflicts = self
            .conflict_detector
            .group_candidates(entries, &outcome.pairs);
        let duplicates = self
            .duplicate_detector
            .group_candidates(entries, &outcome.pairs);

        Ok(Analysis {
            conflicts,
            duplicates,
            status,
        })
    }

    /// 计算命名空间的记忆整合健康度统计报告
    pub async fn calculate_stats(&self, namespace: &str) -> Result<ConsolidationStats> {
        let (entries, truncated) = self.load_active(namespace).await?;
        let total_active = entries.len();
        if total_active == 0 {
            return Ok(ConsolidationStats {
                namespace: namespace.to_string(),
                health_score: 1.0,
                truncated,
                ..Default::default()
            });
        }

        let now = Utc::now();
        let analysis = self.analyze(namespace, &entries, None).await?;
        let stale = self.staleness_detector.detect_in_entries(&entries, now);
        let mut stats = self.build_stats(namespace, total_active, &analysis, &stale);
        // 被上限截断时，健康度只代表"最近 N 条"的抽样，必须如实标注
        stats.truncated = truncated;
        Ok(stats)
    }

    /// 汇总健康度指标
    fn build_stats(
        &self,
        namespace: &str,
        total_active: usize,
        analysis: &Analysis,
        stale: &[crate::types::StaleEntry],
    ) -> ConsolidationStats {
        let mut dup_entries = HashSet::new();
        for d in &analysis.duplicates {
            for id in &d.member_ids {
                dup_entries.insert(*id);
            }
        }

        let mut conf_entries = HashSet::new();
        for c in &analysis.conflicts {
            for id in &c.member_ids {
                conf_entries.insert(*id);
            }
        }

        let duplicate_entries_count = dup_entries.len();
        let conflict_entries_count = conf_entries.len();
        let stale_entries_count = stale.len();
        // 达到"自动归档阈值"的陈旧条目数：把策略里此前未被使用的
        // auto_archive_stale_score 接进报告（只做建议，不静默归档）
        let stale_archive_candidates = stale
            .iter()
            .filter(|s| s.staleness_score >= self.policy.auto_archive_stale_score)
            .count();

        let redundancy_ratio =
            (duplicate_entries_count as f32 / total_active as f32).clamp(0.0, 1.0);
        let conflict_ratio = (conflict_entries_count as f32 / total_active as f32).clamp(0.0, 1.0);

        // 健康度：1.0 减去冗余惩罚、冲突惩罚和陈旧惩罚
        let health_score = (1.0
            - (0.45 * redundancy_ratio
                + 0.40 * conflict_ratio
                + 0.15 * (stale_entries_count as f32 / total_active as f32)))
            .clamp(0.0, 1.0);

        ConsolidationStats {
            namespace: namespace.to_string(),
            total_active,
            duplicate_entries_count,
            duplicate_groups_count: analysis.duplicates.len(),
            conflict_entries_count,
            conflict_groups_count: analysis.conflicts.len(),
            stale_entries_count,
            stale_archive_candidates,
            redundancy_ratio,
            conflict_ratio,
            health_score,
            // 是否被条目上限截断由调用方（calculate_stats）按实际装载结果覆盖
            truncated: false,
        }
    }

    /// 执行一键整合扫描与提案建立（根据策略处理自动合并与提案持久化）
    pub async fn consolidate_namespace(
        &self,
        namespace: &str,
        dry_run: bool,
        sim_threshold: Option<f32>,
        operator_agent: &AgentId,
    ) -> Result<ConsolidationReport> {
        // 同命名空间并发扫描串行化，避免重复产出互相冲突的提案
        let scan_lock = namespace_lock(namespace);
        let _guard = scan_lock.lock().await;

        let (entries, truncated) = self.load_active(namespace).await?;
        let analysis = self.analyze(namespace, &entries, sim_threshold).await?;

        let now = Utc::now();
        let stale = self.staleness_detector.detect_in_entries(&entries, now);
        let mut stats = self.build_stats(namespace, entries.len(), &analysis, &stale);
        // 报告级与统计级必须一致：截断时 stats 也是抽样结果，不能一个是 true 一个是 false
        stats.truncated = truncated;

        let mut report = ConsolidationReport {
            namespace: namespace.to_string(),
            scanned: entries.len(),
            dry_run,
            proposals_created: 0,
            skipped_existing: 0,
            auto_merged: 0,
            proposals: Vec::new(),
            stats,
            mode: analysis.status.clone(),
            truncated,
        };

        if entries.len() < 2 {
            return Ok(report);
        }

        // 1. 冲突组
        let mut conflict_members: HashSet<Uuid> = HashSet::new();
        let mut new_proposals = Vec::new();

        for c in &analysis.conflicts {
            for id in &c.member_ids {
                conflict_members.insert(*id);
            }
            let mut ids = c.member_ids.clone();
            ids.sort(); // 规范化有序
            new_proposals.push(IntegrationProposal {
                id: Uuid::new_v4(),
                namespace: namespace.to_string(),
                kind: IntegrationKind::Conflict,
                member_ids: ids,
                suggested_survivor: Some(c.suggested_survivor),
                similarity: 0.0,
                status: ProposalStatus::Open,
                resolution_note: Some(c.description.clone()),
                created_at: Utc::now(),
                resolved_at: None,
                metadata: proposal_metadata(
                    c.relation,
                    c.source,
                    &c.reason_code,
                    &analysis.status,
                    Some(c.divergence),
                ),
            });
        }

        // 2. 重复组（剔除已被包含进冲突的成员，避免同一个条目双重提案）
        for d in &analysis.duplicates {
            let filtered_members: Vec<Uuid> = d
                .member_ids
                .iter()
                .copied()
                .filter(|id| !conflict_members.contains(id))
                .collect();

            if filtered_members.len() < 2 {
                continue;
            }

            // 自动合并门槛：关系类型 + 相似度 + 成员数（语义候选永远不自动合并）
            let eligibility = crate::policy::merge_eligibility(d.relation);
            if !dry_run
                && self.policy.should_auto_merge(
                    eligibility,
                    d.max_similarity,
                    filtered_members.len(),
                )
                && self
                    .execute_merge(
                        namespace,
                        d.suggested_survivor,
                        &filtered_members,
                        Some("整合中枢自动合并策略触发"),
                        operator_agent,
                    )
                    .await
                    .is_ok()
            {
                report.auto_merged += 1;
                continue;
            }

            let mut ids = filtered_members;
            ids.sort();
            new_proposals.push(IntegrationProposal {
                id: Uuid::new_v4(),
                namespace: namespace.to_string(),
                kind: IntegrationKind::Duplicate,
                member_ids: ids,
                suggested_survivor: Some(d.suggested_survivor),
                similarity: d.max_similarity,
                status: ProposalStatus::Open,
                resolution_note: None,
                created_at: Utc::now(),
                resolved_at: None,
                metadata: proposal_metadata(
                    d.relation,
                    d.source,
                    &d.reason_code,
                    &analysis.status,
                    Some(d.max_similarity),
                ),
            });
        }

        new_proposals.truncate(self.max_proposals);

        if dry_run {
            report.proposals = new_proposals;
            return Ok(report);
        }

        // 幂等查重：一次拉回该命名空间全部 open 提案，在内存按 (kind, 成员集合) 比对。
        // 旧实现每个提案单独发一次 open_exists 查询（N+1）；与 `open_exists` 的
        // SQL 语义（kind 相同 + cardinality 相等 + 超集）等价于集合相等。
        // 本批刚保存的提案同样进入签名集——同批出现相同签名时第二个也要跳过。
        let mut open_signatures: std::collections::HashSet<(String, Vec<Uuid>)> = self
            .store
            .list_proposals(namespace, Some(ProposalStatus::Open))
            .await?
            .into_iter()
            .map(|p| {
                let mut members = p.member_ids;
                members.sort();
                (p.kind.as_str().to_string(), members)
            })
            .collect();
        for proposal in new_proposals {
            let mut members = proposal.member_ids.clone();
            members.sort();
            let signature = (proposal.kind.as_str().to_string(), members);
            if !open_signatures.insert(signature) {
                report.skipped_existing += 1;
                continue;
            }
            self.store.save_proposal(&proposal).await?;
            report.proposals_created += 1;
            report.proposals.push(proposal);
        }

        Ok(report)
    }

    /// 裁决整合提案
    pub async fn resolve_proposal(
        &self,
        req: ResolveRequest,
        auth_agent: &AgentId,
    ) -> Result<MemoryEntry> {
        // 先读一次只为定位命名空间（确定互斥锁的键），锁内**重读**再做状态判定：
        // 状态检查若发生在锁外，两个并发裁决都能读到 Open，随后串行进锁各合并一次
        // （幸存者被二次 revise、版本号翻倍）。锁内重读让第二个裁决看到 Merged。
        let namespace = self.store.get_proposal(req.proposal_id).await?.namespace;
        let resolve_lock = namespace_lock(&namespace);
        let _guard = resolve_lock.lock().await;

        let mut proposal = self.store.get_proposal(req.proposal_id).await?;
        if proposal.status != ProposalStatus::Open {
            return Err(HubError::Conflict);
        }

        let now = Utc::now();
        match req.action {
            ResolveAction::Dismiss => {
                proposal.status = ProposalStatus::Dismissed;
                proposal.resolution_note = req.note.clone();
                proposal.resolved_at = Some(now);
                self.store.save_proposal(&proposal).await?;

                let first_id = proposal.member_ids.first().ok_or(HubError::NotFound)?;
                self.store.get(&proposal.namespace, *first_id).await
            }
            ResolveAction::Merge => {
                let survivor_id =
                    req.survivor_id
                        .or(proposal.suggested_survivor)
                        .ok_or_else(|| {
                            HubError::InvalidArgument(
                                "提案无建议幸存者，必须显式提供 survivor_id".to_string(),
                            )
                        })?;

                if !proposal.member_ids.contains(&survivor_id) {
                    return Err(HubError::InvalidArgument(
                        "survivor_id 必须是提案成员之一".to_string(),
                    ));
                }

                let survivor = self
                    .execute_merge(
                        &proposal.namespace,
                        survivor_id,
                        &proposal.member_ids,
                        req.note.as_deref(),
                        auth_agent,
                    )
                    .await?;

                proposal.status = ProposalStatus::Merged;
                proposal.resolution_note = req.note;
                proposal.resolved_at = Some(now);
                self.store.save_proposal(&proposal).await?;

                Ok(survivor)
            }
        }
    }

    /// 核心执行合并动作
    async fn execute_merge(
        &self,
        namespace: &str,
        survivor_id: Uuid,
        member_ids: &[Uuid],
        _note: Option<&str>,
        operator_agent: &AgentId,
    ) -> Result<MemoryEntry> {
        let mut members = Vec::new();
        for id in member_ids {
            if let Ok(e) = self.store.get(namespace, *id).await {
                if e.status == MemoryStatus::Active {
                    members.push(e);
                }
            }
        }

        let survivor_entry = members
            .iter()
            .find(|m| m.id == survivor_id)
            .cloned()
            .ok_or(HubError::NotFound)?;

        let now = Utc::now();
        let merged_output = self.merger.merge(&survivor_entry, &members).await?;

        // 1. 更新幸存者
        let mut updated_survivor = survivor_entry;
        let content_changed = merged_output.content != updated_survivor.content;
        updated_survivor.content = merged_output.content;
        updated_survivor.tags = merged_output.tags;
        updated_survivor.metadata = merged_output.metadata;
        updated_survivor.confidence = merged_output.confidence;

        // 融合后正文已变，幸存者身上的旧向量不再代表新内容：留着它会污染
        // 检索的相似度维度，并让下一轮扫描把同一批条目继续判成重复。
        // 配了嵌入器就重算，没配则清空（宁可少一路召回，也不留错的向量）。
        //
        // **纯规则模式下的处理**：正文未变时保留原向量（仍然有效，一次嵌入调用都不需要）；
        // 正文已变时清空向量——规则模式不得为了"补齐向量"调用嵌入服务
        // （这会让 B 模式的 embedding 调用数不再是 0）。该条目此后退化为词法可检索，
        // 直到它被再次写入时重建向量。
        if content_changed {
            if self.embedding_allowed() {
                let em = self
                    .embedder
                    .as_ref()
                    .expect("embedding_allowed 已保证嵌入器存在");
                match em.embed(&[updated_survivor.content.clone()]).await {
                    Ok(mut vectors) if !vectors.is_empty() => {
                        let mut v = vectors.remove(0);
                        mem_core::l2_normalize(&mut v);
                        updated_survivor.embedding = Some(v);
                        updated_survivor.embedding_model = Some(em.model_id().to_string());
                    }
                    other => {
                        tracing::warn!(
                            memory_id = %updated_survivor.id,
                            embedding_empty = other.is_ok(),
                            "合并后重嵌入失败，已清空该条目向量（下次写入时重建）"
                        );
                        updated_survivor.embedding = None;
                    }
                }
            } else {
                tracing::info!(
                    memory_id = %updated_survivor.id,
                    "纯规则模式合并：正文已变且不调用嵌入服务，已清空该条目向量（下次写入时重建）"
                );
                updated_survivor.embedding = None;
            }
        }

        let expected_survivor = updated_survivor.version;
        updated_survivor.version += 1;
        updated_survivor.updated_at = now;
        updated_survivor.source_agent = operator_agent.clone();

        let saved = self
            .store
            .revise(&updated_survivor, expected_survivor, VersionChange::Updated)
            .await?;

        // 2. 归并其余成员
        for m in members.into_iter().filter(|m| m.id != survivor_id) {
            let mut archived = m;
            archived.status = MemoryStatus::Merged;
            archived.merged_into = Some(survivor_id);
            let expected_member = archived.version;
            archived.version += 1;
            archived.updated_at = now;
            archived.source_agent = operator_agent.clone();

            // 若尚未标记失效时间，将其有效截止时间限制为合并裁决时间，支撑时间有效性过滤
            if archived.valid_until.is_none() {
                archived.valid_until = Some(now);
            }

            // 建立事实演化关系 (survivor supersedes archived)。
            //
            // 失败不上抛（warn 可观测）：此关系是合并的溯源补充，不是合并本身；
            // 幸存者 revise 已在上方成功提交，若因关系写失败整体报错，
            // 提案会保持 Open 等待重试，而重试会把已合并的正文再走一遍合并
            // （版本号再度递增），代价大于缺失一条溯源边。此权衡与
            // mem-core::chat_cognitive_service 的 supersedes 处理保持一致。
            if let Err(e) = self
                .store
                .add_relation(
                    namespace,
                    &mem_core::NewMemoryRelation {
                        from_memory_id: survivor_id,
                        to_memory_id: archived.id,
                        relation_kind: mem_core::MemoryRelationKind::Supersedes,
                        confidence: 1.0,
                        source_evidence_id: None,
                    },
                )
                .await
            {
                tracing::warn!(
                    namespace,
                    survivor_id = %survivor_id,
                    archived_id = %archived.id,
                    error = %e,
                    "合并时 Supersedes 关系写入失败：溯源链缺失，归档继续"
                );
            }

            self.store
                .revise(&archived, expected_member, VersionChange::Merged)
                .await?;
        }

        Ok(saved)
    }
}

/// 构造提案溯源元数据
///
/// 只记录关系类型、来源、原因码与模式，**不记录任何原始记忆文本、令牌或向量输入**。
fn proposal_metadata(
    relation: crate::candidates::CandidateRelation,
    source: crate::candidates::CandidateSource,
    reason_code: &str,
    status: &ConsolidationModeStatus,
    similarity: Option<f32>,
) -> serde_json::Value {
    serde_json::json!({
        "relation": relation.as_str(),
        "candidate_source": source.as_str(),
        "reason_code": reason_code,
        "mode": status.effective_mode,
        "requested_mode": status.requested_mode,
        "strategy_version": status.strategy_version,
        "fallback_applied": status.fallback_applied,
        "similarity": similarity,
    })
}
