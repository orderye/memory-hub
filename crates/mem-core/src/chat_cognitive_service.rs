//! Chat 认知记忆流服务：提炼、校验、冲突检测、治理与版本提交流程 (P2)
//!
//! 核心保障：
//! 1. 先写原始证据存根（append-only），模型失败或超时均不丢失证据；
//! 2. 规则校验：置信度范围、内容上限、意图含糊推断阻断；
//! 3. 意图生命周期：必须具备时间有效期或到期过滤，过期意图不返回在当前计划中；
//! 4. 事实演化：识别 supersedes 取代关系，建立实体关系网络并封堵旧事实有效期；
//! 5. 治理纪律：Manual 模式绝不自动入库，通过候选提案状态机管理审查；
//! 6. 幂等执行：同一会话及内容重试不重复生成条目与版本。

use crate::account_scope::AccountScopedStore;
use crate::domain::{
    indicates_fact_evolution, is_intention_active, validate_cognitive_candidate,
    ChatCognitiveInput, CognitiveCandidate, CognitiveGovernanceMode, CognitiveLayer,
    CognitivePolicy, CognitiveProposalRecord, CognitiveProposalStatus, EvidenceSourceKind,
    MemoryEntry, MemoryKind, MemoryRelationKind, MemoryScene, MemoryStatus, NewEvidence,
    NewMemoryRelation, VersionChange,
};
use crate::infra::{ChatCognitiveExtractor, MemoryHubStore};
use chrono::{DateTime, Utc};
use common::{AccountId, AgentId, AuthCtx, HubError, Result, Scope};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use uuid::Uuid;

/// Chat 认知处理结果
#[derive(Debug, Clone)]
pub struct ChatCognitiveResult {
    /// 持久化或查重命中的证据 ID
    pub evidence_id: Uuid,
    /// 自动入库的有效记忆条目
    pub committed_entries: Vec<MemoryEntry>,
    /// 产生的候选提案（人工待审或被拒绝）；均已持久化，ID 可直接用于裁决
    pub proposals: Vec<CognitiveProposalRecord>,
}

/// 认知提案裁决动作
#[derive(Debug, Clone)]
pub enum CognitiveReviewDecision {
    /// 驳回：提案标记 rejected，候选不入库
    Reject {
        /// 驳回理由（保留审计轨迹）
        reason: Option<String>,
    },
    /// 采纳：候选入库为记忆条目；`modified_content` 提供时以编辑后正文入库
    Accept { modified_content: Option<String> },
}

/// 认知查重最大扫描条目数：避免命名空间条目爆炸时全量加载到内存
///（整合引擎的 scan_limit 类似约束，这里同步加上）
const SCAN_LIMIT: usize = 256;

/// Chat 认知提炼与管理服务
pub struct ChatCognitiveService {
    store: Arc<dyn MemoryHubStore>,
    extractor: Arc<dyn ChatCognitiveExtractor>,
    policy: CognitivePolicy,
}

impl ChatCognitiveService {
    pub fn new(
        store: Arc<dyn MemoryHubStore>,
        extractor: Arc<dyn ChatCognitiveExtractor>,
        policy: CognitivePolicy,
    ) -> Self {
        Self {
            store,
            extractor,
            policy,
        }
    }

    /// 切换作用域底层存储（账户沙箱化）
    pub fn scoped(&self, account_id: AccountId) -> Self {
        Self {
            store: Arc::new(AccountScopedStore::new(self.store.clone(), account_id)),
            extractor: self.extractor.clone(),
            policy: self.policy.clone(),
        }
    }

    /// 提取会话内容签名哈希
    pub fn compute_messages_hash(input: &ChatCognitiveInput) -> String {
        let mut hasher = Sha256::new();
        for m in &input.messages {
            hasher.update(m.role.as_str().as_bytes());
            hasher.update(b":");
            hasher.update(m.content.as_bytes());
            hasher.update(b"\n");
        }
        hex::encode(hasher.finalize())
    }

    /// 构造脱敏/最小化摘要片段（默认取首条或前几条消息摘要）
    ///
    /// 注意：入库前必经 [`crate::redaction`] 脱敏。
    pub fn build_excerpt(input: &ChatCognitiveInput, max_chars: usize) -> String {
        Self::build_excerpt_report(input, max_chars).text
    }

    /// 同 [`Self::build_excerpt`]，但额外返回脱敏命中统计（供日志与审计）
    ///
    /// 这是**证据持久化前的最后一道闸门**：凭证与个人信息在入库前被替换为
    /// 占位符，完整对话永不落库（只留定长摘要）。
    /// 顺序不可颠倒——先截断再脱敏会漏掉被截断掉的尾部凭证。
    pub fn build_excerpt_report(
        input: &ChatCognitiveInput,
        max_chars: usize,
    ) -> crate::redaction::RedactionReport {
        // 先拼**整段**会话：脱敏必须覆盖全文，否则摘要窗口之外的凭证
        // 既不会被替换、也不会被计数（运维看不到"这次会话里有凭证"）。
        let mut raw = String::new();
        for m in &input.messages {
            raw.push_str(m.role.as_str());
            raw.push_str(": ");
            raw.push_str(m.content.trim());
            raw.push('\n');
        }
        // 再脱敏（覆盖全文）→ 最后最小化（定长截断）
        crate::redaction::sanitize_evidence_excerpt(&raw, max_chars)
    }

    /// 证据保留期限到期时刻（None = 永久保留）
    ///
    /// 证据虽然已脱敏且只存摘要，但它仍是原始会话的片段。
    /// 默认 90 天后由清理任务物理删除，避免长期留存的不可变副本。
    fn evidence_expiry(
        &self,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Option<chrono::DateTime<chrono::Utc>> {
        if self.policy.evidence_retention_days == 0 {
            return None;
        }
        Some(now + chrono::Duration::days(self.policy.evidence_retention_days as i64))
    }

    /// 构造并持久化一条认知提案，返回落库后的记录。
    ///
    /// 提案是 Manual/SemiAuto 治理下人工审查的唯一凭据：不落库等于
    /// 提炼结果直接丢失（这是链路一要修复的核心问题）。落库失败时上抛，
    /// 不静默降级为"只存在于返回值里"。
    async fn persist_proposal(
        &self,
        namespace: &str,
        candidate: CognitiveCandidate,
        evidence_id: Uuid,
        status: CognitiveProposalStatus,
        rejection_reason: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<CognitiveProposalRecord> {
        let record = CognitiveProposalRecord {
            id: Uuid::new_v4(),
            namespace: namespace.to_string(),
            candidate,
            source_evidence_id: evidence_id,
            status,
            rejection_reason,
            resulting_memory_id: None,
            resolved_by: None,
            created_at: now,
            updated_at: now,
        };
        self.store.save_cognitive_proposal(&record).await?;
        Ok(record)
    }

    /// 执行 Chat 会话认知提炼主流程
    pub async fn process_chat_session(
        &self,
        auth: &AuthCtx,
        input: ChatCognitiveInput,
    ) -> Result<ChatCognitiveResult> {
        if !auth.has_scope(&Scope::Write) {
            return Err(HubError::InsufficientScope);
        }

        let content_hash = Self::compute_messages_hash(&input);
        let now = Utc::now();

        // 步骤 1：持久化原始证据（不可变存根，优先落库，模型超时失败不丢失证据；相同会话与哈希幂等复用）
        let existing_evidences = self
            .store
            .list_evidence(
                &input.namespace,
                Some(EvidenceSourceKind::Session),
                Some(&input.session_id),
                100,
            )
            .await?;
        let existing_evidence = existing_evidences
            .into_iter()
            .find(|e| e.content_hash == content_hash);

        let evidence = match existing_evidence {
            Some(ev) => ev,
            None => {
                // 入库前脱敏 + 最小化：不可变表一旦写入就无法追回泄露的凭证
                let excerpt_report = Self::build_excerpt_report(
                    &input,
                    crate::redaction::EVIDENCE_EXCERPT_MAX_CHARS,
                );
                if excerpt_report.has_findings() {
                    // 只记录类别与数量，绝不把命中内容写进日志
                    let kinds = excerpt_report
                        .findings
                        .iter()
                        .map(|f| format!("{}×{}", f.kind.label(), f.count))
                        .collect::<Vec<_>>()
                        .join(",");
                    tracing::warn!(
                        namespace = %input.namespace,
                        redacted = %kinds,
                        "证据入库前检测到敏感内容并已脱敏"
                    );
                }
                self.store
                    .append_evidence(&NewEvidence {
                        id: None,
                        namespace: input.namespace.clone(),
                        source_kind: EvidenceSourceKind::Session,
                        source_ref: Some(input.session_id.clone()),
                        content_hash: content_hash.clone(),
                        excerpt: Some(excerpt_report.text),
                        occurred_at: input.occurred_at.or(Some(now)),
                        expires_at: self.evidence_expiry(now),
                    })
                    .await?
            }
        };

        // 步骤 2：调用注入的模型抽取器提炼结构化候选
        let raw_candidates = self.extractor.extract_candidates(&input).await?;

        // 步骤 3：规则预处理与校验
        let mut committed_entries = Vec::new();
        let mut proposals = Vec::new();

        // 查询当前活跃事实用于冲突/重复与演化比对
        let existing_memories = self
            .store
            .all_in_namespace_limited(&input.namespace, false, SCAN_LIMIT)
            .await?
            .into_iter()
            .filter(|e| e.status == MemoryStatus::Active)
            .collect::<Vec<_>>();

        for candidate in raw_candidates {
            let validation = validate_cognitive_candidate(&candidate);
            if let Err(e) = validation {
                // 校验未通过：创建 Rejected 提案记录，保留原始证据溯源，不直接崩溃
                let record = self
                    .persist_proposal(
                        &input.namespace,
                        candidate,
                        evidence.id,
                        CognitiveProposalStatus::Rejected,
                        Some(format!("候选校验失败: {}", e)),
                        now,
                    )
                    .await?;
                proposals.push(record);
                continue;
            }

            // 置信度过低过滤 (< 0.5)
            if candidate.confidence < 0.5 {
                let record = self
                    .persist_proposal(
                        &input.namespace,
                        candidate,
                        evidence.id,
                        CognitiveProposalStatus::Rejected,
                        Some("置信度过低 (< 0.5)".into()),
                        now,
                    )
                    .await?;
                proposals.push(record);
                continue;
            }

            // 查重：若命名空间内已存在完全相同内容的活跃条目，幂等跳过以杜绝无限复制
            if existing_memories.iter().any(|e| {
                e.content.trim() == candidate.content.trim() && e.scene == Some(candidate.scene)
            }) {
                continue;
            }

            // 事实演化检测：检查候选是否标记或构成对已有事实的 supersedes
            let supersedes_target = self.detect_supersedes_target(&candidate, &existing_memories);

            // 步骤 4：根据治理策略决定是自动接受还是生成待审提案
            match self.policy.mode {
                CognitiveGovernanceMode::Manual => {
                    // Manual 模式下绝不自动合并：待审提案必须落库，
                    // 否则默认治理模式下所有提炼结果都会丢失
                    let record = self
                        .persist_proposal(
                            &input.namespace,
                            candidate,
                            evidence.id,
                            CognitiveProposalStatus::PendingReview,
                            None,
                            now,
                        )
                        .await?;
                    proposals.push(record);
                }
                CognitiveGovernanceMode::SemiAuto | CognitiveGovernanceMode::FullAuto => {
                    // 两种自动模式都必须遵守 auto_accept_threshold：
                    // 此前 FullAuto 无条件自动入库，使阈值形同虚设（只剩 <0.5 的底线过滤）
                    let should_auto_accept =
                        candidate.confidence >= self.policy.auto_accept_threshold;

                    if should_auto_accept {
                        let entry = self
                            .commit_candidate(
                                &auth.agent_id,
                                &input.namespace,
                                &candidate,
                                evidence.id,
                                supersedes_target,
                                now,
                            )
                            .await?;
                        committed_entries.push(entry);
                    } else {
                        // 低于阈值的处置按模式区分：
                        // - SemiAuto：进入待审提案队列（人工可救回），同样必须落库
                        // - FullAuto：不静默入库，落一条 Rejected 记录保留审计轨迹
                        let full_auto = self.policy.mode == CognitiveGovernanceMode::FullAuto;
                        let rejection_reason = full_auto.then(|| {
                            format!(
                                "FullAuto 模式下置信度低于自动接受阈值 {}，未入库（保留审计记录）",
                                self.policy.auto_accept_threshold
                            )
                        });
                        let status = if full_auto {
                            CognitiveProposalStatus::Rejected
                        } else {
                            CognitiveProposalStatus::PendingReview
                        };
                        let record = self
                            .persist_proposal(
                                &input.namespace,
                                candidate,
                                evidence.id,
                                status,
                                rejection_reason,
                                now,
                            )
                            .await?;
                        proposals.push(record);
                    }
                }
            }
        }

        Ok(ChatCognitiveResult {
            evidence_id: evidence.id,
            committed_entries,
            proposals,
        })
    }

    /// 人工裁决/审核认知提案（基于持久化记录，按 ID 裁决）
    ///
    /// 裁决路径与存储状态机联动：
    /// - 驳回：`pending_review → rejected`，提案保留审计轨迹；
    /// - 采纳（可直接改写候选正文）：先校验修改后的候选合法，再入库为
    ///   记忆条目，最后 `pending_review → accepted` 回写 resulting_memory_id。
    ///
    /// 状态推进带乐观前置（expected = pending_review）：提案已被裁决过或
    /// 并发裁决时返回 `Conflict`，同一提案不会写入两条记忆。
    /// 入库成功而状态回写失败时保留警告日志（记忆已写入，回滚需要人工介入）。
    pub async fn review_persisted_proposal(
        &self,
        auth: &AuthCtx,
        proposal_id: Uuid,
        decision: CognitiveReviewDecision,
    ) -> Result<Option<MemoryEntry>> {
        if !auth.has_scope(&Scope::Write) {
            return Err(HubError::InsufficientScope);
        }

        let proposal = self.store.get_cognitive_proposal(proposal_id).await?;
        // 只有待审提案可裁决：已裁决（accepted/rejected）的提案再次裁决
        // 返回 Conflict，杜绝重复采纳写入重复记忆
        if proposal.status != CognitiveProposalStatus::PendingReview {
            return Err(HubError::Conflict);
        }
        let now = Utc::now();

        match decision {
            CognitiveReviewDecision::Reject { reason } => {
                self.store
                    .update_cognitive_proposal_status(
                        proposal_id,
                        CognitiveProposalStatus::PendingReview,
                        CognitiveProposalStatus::Rejected,
                        reason.as_deref(),
                        None,
                        Some(&auth.agent_id),
                    )
                    .await?;
                Ok(None)
            }
            CognitiveReviewDecision::Accept { modified_content } => {
                // 编辑后合并：修改内容必须先过候选校验（长度/敏感凭证闸门），
                // 校验失败时提案仍停留在 pending_review，可修正后重试
                let mut candidate = proposal.candidate.clone();
                if let Some(content) = modified_content {
                    candidate.content = content;
                    validate_cognitive_candidate(&candidate)?;
                }

                let existing_memories = self
                    .store
                    .all_in_namespace_limited(&proposal.namespace, false, SCAN_LIMIT)
                    .await?
                    .into_iter()
                    .filter(|e| e.status == MemoryStatus::Active)
                    .collect::<Vec<_>>();

                let supersedes_target =
                    self.detect_supersedes_target(&candidate, &existing_memories);

                let entry = self
                    .commit_candidate(
                        &auth.agent_id,
                        &proposal.namespace,
                        &candidate,
                        proposal.source_evidence_id,
                        supersedes_target,
                        now,
                    )
                    .await?;

                // 入库成功后推进状态；失败不回滚已写入的记忆，但要可观测
                if let Err(e) = self
                    .store
                    .update_cognitive_proposal_status(
                        proposal_id,
                        CognitiveProposalStatus::PendingReview,
                        CognitiveProposalStatus::Accepted,
                        None,
                        Some(entry.id),
                        Some(&auth.agent_id),
                    )
                    .await
                {
                    tracing::warn!(
                        proposal_id = %proposal_id,
                        memory_id = %entry.id,
                        error = %e,
                        "认知提案采纳后状态回写失败（记忆已入库，提案状态未推进）"
                    );
                    return Err(e);
                }
                Ok(Some(entry))
            }
        }
    }

    /// 列出认知提案（人工审查队列；status 为 None 时列出全部）
    pub async fn list_proposals(
        &self,
        auth: &AuthCtx,
        namespace: &str,
        status: Option<CognitiveProposalStatus>,
    ) -> Result<Vec<CognitiveProposalRecord>> {
        if !auth.has_scope(&Scope::Read) {
            return Err(HubError::InsufficientScope);
        }
        self.store.list_cognitive_proposals(namespace, status).await
    }

    /// 人工裁决/审核认知提案（按内存中的提案对象裁决；不推进持久化状态）
    ///
    /// 兼容入口：早期提案只存在于 `process_chat_session` 返回值中时使用。
    /// 新代码应使用 [`Self::review_persisted_proposal`]——那才是带乐观锁、
    /// 状态可追溯的裁决路径。
    pub async fn review_proposal(
        &self,
        auth: &AuthCtx,
        proposal: &mut CognitiveProposalRecord,
        accept: bool,
        rejection_reason: Option<String>,
    ) -> Result<Option<MemoryEntry>> {
        if !auth.has_scope(&Scope::Write) {
            return Err(HubError::InsufficientScope);
        }

        let now = Utc::now();
        if !accept {
            proposal.status = CognitiveProposalStatus::Rejected;
            proposal.rejection_reason = rejection_reason;
            proposal.resolved_by = Some(auth.agent_id.clone());
            proposal.updated_at = now;
            return Ok(None);
        }

        let existing_memories = self
            .store
            .all_in_namespace_limited(&proposal.namespace, false, SCAN_LIMIT)
            .await?
            .into_iter()
            .filter(|e| e.status == MemoryStatus::Active)
            .collect::<Vec<_>>();

        let supersedes_target =
            self.detect_supersedes_target(&proposal.candidate, &existing_memories);

        let entry = self
            .commit_candidate(
                &auth.agent_id,
                &proposal.namespace,
                &proposal.candidate,
                proposal.source_evidence_id,
                supersedes_target,
                now,
            )
            .await?;

        proposal.status = CognitiveProposalStatus::Accepted;
        proposal.resulting_memory_id = Some(entry.id);
        proposal.resolved_by = Some(auth.agent_id.clone());
        proposal.updated_at = now;

        Ok(Some(entry))
    }

    /// 查询当前有效意图（自动排除已过期意图）
    pub async fn list_active_intentions(
        &self,
        auth: &AuthCtx,
        namespace: &str,
    ) -> Result<Vec<MemoryEntry>> {
        if !auth.has_scope(&Scope::Read) {
            return Err(HubError::InsufficientScope);
        }

        let now = Utc::now();
        let items = self
            .store
            .all_in_namespace_limited(namespace, false, SCAN_LIMIT)
            .await?;
        let active = items
            .into_iter()
            .filter(|e| e.status == MemoryStatus::Active)
            .filter(|e| {
                e.scene == Some(MemoryScene::Chat) && e.layer == Some(CognitiveLayer::Intention)
            })
            .filter(|e| is_intention_active(CognitiveLayer::Intention, e.valid_until, now))
            .collect();

        Ok(active)
    }

    /// 提交候选并形成正式 MemoryEntry 及关联网络
    async fn commit_candidate(
        &self,
        agent: &AgentId,
        namespace: &str,
        candidate: &CognitiveCandidate,
        evidence_id: Uuid,
        supersedes_target: Option<Uuid>,
        now: DateTime<Utc>,
    ) -> Result<MemoryEntry> {
        let entry_id = Uuid::new_v4();
        let kind = match candidate.layer {
            CognitiveLayer::Fact => MemoryKind::Fact,
            CognitiveLayer::Summary => MemoryKind::Note,
            CognitiveLayer::Identity => MemoryKind::Fact,
            CognitiveLayer::Knowledge => MemoryKind::Skill,
            CognitiveLayer::Schema => MemoryKind::Procedure,
            CognitiveLayer::Intention => MemoryKind::Preference,
            CognitiveLayer::Raw => MemoryKind::Episode,
        };

        let entry = MemoryEntry {
            id: entry_id,
            namespace: namespace.to_string(),
            key: None,
            kind,
            content: candidate.content.clone(),
            tags: candidate.tags.clone(),
            metadata: candidate.metadata.clone(),
            embedding: None,
            embedding_model: None,
            source_agent: agent.clone(),
            confidence: candidate.confidence,
            status: MemoryStatus::Active,
            version: 1,
            expires_at: candidate.valid_until,
            merged_into: None,
            created_at: now,
            updated_at: now,
            scene: Some(candidate.scene),
            layer: Some(candidate.layer),
            valid_from: candidate.valid_from.or(candidate.occurred_at),
            valid_until: candidate.valid_until,
            occurred_at: candidate.occurred_at.or(Some(now)),
        };

        // 1. 原子写入记忆条目与初始版本历史
        let saved = self.store.insert_initial(&entry).await?;

        // 2. 处理事实演化（supersedes）：如果取代了旧事实
        if let Some(old_id) = supersedes_target {
            if old_id != saved.id {
                // 建立 Supersedes 关系 (new -> old)。
                //
                // 失败不上抛：新事实已落库，主流程因次要写失败整体回滚/报错
                // 会让"新事实入库"这件主要的事也失败；这里选择可观测性优先——
                // warn 带上命名空间与新旧 id，运维可据此人工补链。
                if let Err(e) = self
                    .store
                    .add_relation(
                        namespace,
                        &NewMemoryRelation {
                            from_memory_id: saved.id,
                            to_memory_id: old_id,
                            relation_kind: MemoryRelationKind::Supersedes,
                            confidence: 1.0,
                            source_evidence_id: Some(evidence_id),
                        },
                    )
                    .await
                {
                    tracing::warn!(
                        namespace,
                        new_memory_id = %saved.id,
                        old_memory_id = %old_id,
                        error = %e,
                        "supersedes 关系写入失败：溯源链缺失，新事实已入库"
                    );
                }

                // 限制旧事实的时间有效期上限为新事实发生时间，防止旧事实在当前时刻继续被作为有效事实召回。
                //
                // 失败不上抛（理由同上）：旧事实会暂时继续被召回，与该 warn 一起
                // 构成可观测的中间态；上抛则连新事实都不可用，损失更大。
                if let Ok(mut old_entry) = self.store.get(namespace, old_id).await {
                    let old_version = old_entry.version;
                    old_entry.valid_until = candidate.occurred_at.or(Some(now));
                    old_entry.version += 1;
                    old_entry.updated_at = now;
                    if let Err(e) = self
                        .store
                        .revise(&old_entry, old_version, VersionChange::Updated)
                        .await
                    {
                        tracing::warn!(
                            namespace,
                            new_memory_id = %saved.id,
                            old_memory_id = %old_id,
                            error = %e,
                            "旧事实 valid_until 封堵失败：旧事实暂仍作为有效事实召回"
                        );
                    }
                }
            }
        }

        Ok(saved)
    }

    /// 事实演化检测：判断候选是否指向要取代的既有条目
    fn detect_supersedes_target(
        &self,
        candidate: &CognitiveCandidate,
        existing: &[MemoryEntry],
    ) -> Option<Uuid> {
        // 显式指定
        if candidate.relation_kind == Some(MemoryRelationKind::Supersedes) {
            if let Some(target_id) = candidate.target_memory_id {
                if existing.iter().any(|e| e.id == target_id) {
                    return Some(target_id);
                }
            }
        }

        // 语义演化启发式识别（如新事实描述 "改用/不再使用/决定使用"，匹配相同主体旧条目）
        let indicates_evolution = indicates_fact_evolution(&candidate.content);

        if indicates_evolution {
            // 确定性地取"最近的相关旧事实"：按 occurred_at（其次 updated_at、created_at）
            // 降序。all_in_namespace 不保证顺序，旧实现取"最后一个"导致目标选择不确定。
            let mut candidates: Vec<&MemoryEntry> = existing
                .iter()
                .filter(|old| old.layer == Some(candidate.layer))
                .filter(|old| old.tags.iter().any(|t| candidate.tags.contains(t)))
                .collect();
            candidates.sort_by(|a, b| {
                b.occurred_at
                    .cmp(&a.occurred_at)
                    .then(b.updated_at.cmp(&a.updated_at))
                    .then(b.created_at.cmp(&a.created_at))
            });
            return candidates.first().map(|e| e.id);
        }

        None
    }
}

#[cfg(test)]
mod tests;
