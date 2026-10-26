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
    is_intention_active, validate_cognitive_candidate, ChatCognitiveInput, CognitiveCandidate,
    CognitiveGovernanceMode, CognitiveLayer, CognitivePolicy, CognitiveProposalRecord,
    CognitiveProposalStatus, EvidenceSourceKind, MemoryEntry, MemoryKind, MemoryRelationKind,
    MemoryScene, MemoryStatus, NewEvidence, NewMemoryRelation, VersionChange,
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
            .all_in_namespace(&input.namespace, false)
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
                    .all_in_namespace(&proposal.namespace, false)
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
            .all_in_namespace(&proposal.namespace, false)
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
        let items = self.store.all_in_namespace(namespace, false).await?;
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
        let content_lower = candidate.content.to_lowercase();
        let evolution_markers = [
            "决定改用",
            "改用",
            "切换至",
            "不再使用",
            "supersede",
            "replace",
        ];
        let indicates_evolution = evolution_markers.iter().any(|m| content_lower.contains(m));

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
mod tests {
    use super::*;
    use crate::domain::{
        ChatMessage, ChatRole, CodingRevision, CodingThread, ExperienceStatus, MemoryEvidence,
        MemoryExperience, MemoryRelation, MemoryTask, MemoryTaskExperience, MemoryVersion,
        NewCodingRevision, NewCodingThread, NewMemoryExperience, NewMemoryTask, TaskStatus,
    };
    use crate::infra::*;
    use crate::testing::MockChatCognitiveExtractor;
    use async_trait::async_trait;
    use common::{AgentId, Scope};
    use std::collections::HashMap;
    use std::sync::RwLock;

    #[derive(Default)]
    struct TestHubStore {
        items: RwLock<HashMap<(String, Uuid), MemoryEntry>>,
        versions: RwLock<Vec<MemoryVersion>>,
        evidences: RwLock<Vec<MemoryEvidence>>,
        relations: RwLock<Vec<MemoryRelation>>,
        tasks: RwLock<HashMap<(String, Uuid), MemoryTask>>,
        experiences: RwLock<HashMap<(String, Uuid), MemoryExperience>>,
        task_experiences: RwLock<Vec<MemoryTaskExperience>>,
        cognitive_proposals: RwLock<HashMap<Uuid, CognitiveProposalRecord>>,
        /// 错误注入：置位后 revise 一律失败（验证 supersedes 封堵失败不拖垮主流程）
        fail_revise: std::sync::atomic::AtomicBool,
        /// 错误注入：置位后 add_relation 一律失败
        fail_add_relation: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl MemoryStore for TestHubStore {
        async fn insert(&self, e: &MemoryEntry) -> Result<MemoryEntry> {
            self.items
                .write()
                .unwrap()
                .insert((e.namespace.clone(), e.id), e.clone());
            Ok(e.clone())
        }
        async fn insert_initial(&self, entry: &MemoryEntry) -> Result<MemoryEntry> {
            self.items
                .write()
                .unwrap()
                .insert((entry.namespace.clone(), entry.id), entry.clone());
            self.versions.write().unwrap().push(MemoryVersion {
                memory_id: entry.id,
                version: 1,
                content: entry.content.clone(),
                tags: entry.tags.clone(),
                metadata: entry.metadata.clone(),
                confidence: entry.confidence,
                changed_by: entry.source_agent.clone(),
                change_type: VersionChange::Created,
                changed_at: entry.created_at,
                scene: entry.scene,
                layer: entry.layer,
                valid_from: entry.valid_from,
                valid_until: entry.valid_until,
                occurred_at: entry.occurred_at,
            });
            Ok(entry.clone())
        }
        async fn get(&self, ns: &str, id: Uuid) -> Result<MemoryEntry> {
            self.items
                .read()
                .unwrap()
                .get(&(ns.to_string(), id))
                .cloned()
                .ok_or(HubError::NotFound)
        }
        async fn get_by_key(&self, _ns: &str, _key: &str) -> Result<Option<MemoryEntry>> {
            Ok(None)
        }
        async fn save(&self, entry: &MemoryEntry) -> Result<MemoryEntry> {
            self.items
                .write()
                .unwrap()
                .insert((entry.namespace.clone(), entry.id), entry.clone());
            Ok(entry.clone())
        }
        async fn revise(
            &self,
            entry: &MemoryEntry,
            _expected_version: i64,
            change: VersionChange,
        ) -> Result<MemoryEntry> {
            if self.fail_revise.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(HubError::Unavailable);
            }
            self.items
                .write()
                .unwrap()
                .insert((entry.namespace.clone(), entry.id), entry.clone());
            self.versions.write().unwrap().push(MemoryVersion {
                memory_id: entry.id,
                version: entry.version,
                content: entry.content.clone(),
                tags: entry.tags.clone(),
                metadata: entry.metadata.clone(),
                confidence: entry.confidence,
                changed_by: entry.source_agent.clone(),
                change_type: change,
                changed_at: entry.updated_at,
                scene: entry.scene,
                layer: entry.layer,
                valid_from: entry.valid_from,
                valid_until: entry.valid_until,
                occurred_at: entry.occurred_at,
            });
            Ok(entry.clone())
        }
        async fn delete(&self, ns: &str, id: Uuid) -> Result<()> {
            self.items.write().unwrap().remove(&(ns.to_string(), id));
            Ok(())
        }
        async fn list(&self, _f: &crate::domain::MemoryFilter) -> Result<Vec<MemoryEntry>> {
            Ok(vec![])
        }
        async fn count(&self, _f: &crate::domain::MemoryFilter) -> Result<usize> {
            Ok(0)
        }
        async fn similar(
            &self,
            _filter: &crate::domain::MemoryFilter,
            _emb: &[f32],
            _k: usize,
        ) -> Result<Vec<crate::domain::Scored<MemoryEntry>>> {
            Ok(vec![])
        }
        async fn all_in_namespace(&self, ns: &str, _inc: bool) -> Result<Vec<MemoryEntry>> {
            Ok(self
                .items
                .read()
                .unwrap()
                .values()
                .filter(|e| e.namespace == ns)
                .cloned()
                .collect())
        }
        async fn list_namespaces(&self) -> Result<Vec<String>> {
            Ok(vec![])
        }
        async fn record_version(&self, v: &MemoryVersion) -> Result<()> {
            self.versions.write().unwrap().push(v.clone());
            Ok(())
        }
        async fn versions(&self, id: Uuid) -> Result<Vec<MemoryVersion>> {
            Ok(self
                .versions
                .read()
                .unwrap()
                .iter()
                .filter(|v| v.memory_id == id)
                .cloned()
                .collect())
        }
        async fn archive_expired(&self, _now: DateTime<Utc>) -> Result<usize> {
            Ok(0)
        }
    }

    #[async_trait]
    impl EvidenceStore for TestHubStore {
        async fn append_evidence(&self, ev: &NewEvidence) -> Result<MemoryEvidence> {
            let record = MemoryEvidence {
                id: ev.id.unwrap_or_else(Uuid::new_v4),
                namespace: ev.namespace.clone(),
                source_kind: ev.source_kind,
                source_ref: ev.source_ref.clone(),
                content_hash: ev.content_hash.clone(),
                excerpt: ev.excerpt.clone(),
                occurred_at: ev.occurred_at,
                created_at: Utc::now(),
                expires_at: ev.expires_at,
            };
            self.evidences.write().unwrap().push(record.clone());
            Ok(record)
        }
        async fn get_evidence(&self, ns: &str, id: Uuid) -> Result<MemoryEvidence> {
            self.evidences
                .read()
                .unwrap()
                .iter()
                .find(|e| e.namespace == ns && e.id == id)
                .cloned()
                .ok_or(HubError::NotFound)
        }
        async fn list_evidence(
            &self,
            ns: &str,
            _sk: Option<EvidenceSourceKind>,
            _sr: Option<&str>,
            _lim: usize,
        ) -> Result<Vec<MemoryEvidence>> {
            Ok(self
                .evidences
                .read()
                .unwrap()
                .iter()
                .filter(|e| e.namespace == ns)
                .cloned()
                .collect())
        }
        async fn all_evidence_in_namespace(&self, ns: &str) -> Result<Vec<MemoryEvidence>> {
            Ok(self
                .evidences
                .read()
                .unwrap()
                .iter()
                .filter(|e| e.namespace == ns)
                .cloned()
                .collect())
        }
        async fn delete_evidence(&self, ns: &str, id: Uuid) -> Result<()> {
            self.evidences
                .write()
                .unwrap()
                .retain(|e| !(e.namespace == ns && e.id == id));
            Ok(())
        }

        async fn purge_expired_evidence(
            &self,
            _ns: &str,
            _now: chrono::DateTime<chrono::Utc>,
        ) -> Result<usize> {
            Ok(0)
        }
    }

    #[async_trait]
    impl RelationStore for TestHubStore {
        async fn add_relation(&self, ns: &str, rel: &NewMemoryRelation) -> Result<MemoryRelation> {
            if self
                .fail_add_relation
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                return Err(HubError::Unavailable);
            }
            let record = MemoryRelation {
                namespace: Some(ns.to_string()),
                from_memory_id: rel.from_memory_id,
                to_memory_id: rel.to_memory_id,
                relation_kind: rel.relation_kind,
                confidence: rel.confidence,
                source_evidence_id: rel.source_evidence_id,
                created_at: Utc::now(),
            };
            self.relations.write().unwrap().push(record.clone());
            Ok(record)
        }
        async fn list_relations_from(&self, _ns: &str, from: Uuid) -> Result<Vec<MemoryRelation>> {
            Ok(self
                .relations
                .read()
                .unwrap()
                .iter()
                .filter(|r| r.from_memory_id == from)
                .cloned()
                .collect())
        }
        async fn list_relations_to(&self, _ns: &str, to: Uuid) -> Result<Vec<MemoryRelation>> {
            Ok(self
                .relations
                .read()
                .unwrap()
                .iter()
                .filter(|r| r.to_memory_id == to)
                .cloned()
                .collect())
        }
        async fn delete_relation(
            &self,
            _ns: &str,
            from: Uuid,
            to: Uuid,
            k: MemoryRelationKind,
        ) -> Result<()> {
            self.relations.write().unwrap().retain(|r| {
                !(r.from_memory_id == from && r.to_memory_id == to && r.relation_kind == k)
            });
            Ok(())
        }
        async fn all_relations_in_namespace(&self, _ns: &str) -> Result<Vec<MemoryRelation>> {
            Ok(self.relations.read().unwrap().clone())
        }
    }

    // Dummy impls for BackupStore, IntegrationStore, GrantStore, AgentStore to satisfy MemoryHubStore
    #[async_trait]
    impl BackupStore for TestHubStore {
        async fn save_backup(&self, _b: &crate::domain::BackupRecord) -> Result<()> {
            Ok(())
        }
        async fn get_backup(&self, _id: Uuid) -> Result<crate::domain::BackupRecord> {
            Err(HubError::NotFound)
        }
        async fn list_backups(
            &self,
            _ns: Option<&str>,
        ) -> Result<Vec<crate::domain::BackupRecord>> {
            Ok(vec![])
        }
        async fn delete_backup(&self, _id: Uuid) -> Result<()> {
            Ok(())
        }
    }
    #[async_trait]
    impl IntegrationStore for TestHubStore {
        async fn save_proposal(&self, _p: &crate::domain::IntegrationProposal) -> Result<()> {
            Ok(())
        }
        async fn get_proposal(&self, _id: Uuid) -> Result<crate::domain::IntegrationProposal> {
            Err(HubError::NotFound)
        }
        async fn list_proposals(
            &self,
            _ns: &str,
            _s: Option<crate::domain::ProposalStatus>,
        ) -> Result<Vec<crate::domain::IntegrationProposal>> {
            Ok(vec![])
        }
        async fn open_exists(&self, _ns: &str, _m: &[(String, Vec<Uuid>)]) -> Result<bool> {
            Ok(false)
        }
    }
    #[async_trait]
    impl crate::infra::CognitiveProposalStore for TestHubStore {
        async fn save_cognitive_proposal(&self, p: &CognitiveProposalRecord) -> Result<()> {
            self.cognitive_proposals
                .write()
                .unwrap()
                .insert(p.id, p.clone());
            Ok(())
        }
        async fn get_cognitive_proposal(&self, id: Uuid) -> Result<CognitiveProposalRecord> {
            self.cognitive_proposals
                .read()
                .unwrap()
                .get(&id)
                .cloned()
                .ok_or(HubError::NotFound)
        }
        async fn list_cognitive_proposals(
            &self,
            ns: &str,
            s: Option<CognitiveProposalStatus>,
        ) -> Result<Vec<CognitiveProposalRecord>> {
            Ok(self
                .cognitive_proposals
                .read()
                .unwrap()
                .values()
                .filter(|p| p.namespace == ns && s.is_none_or(|st| p.status == st))
                .cloned()
                .collect())
        }
        async fn update_cognitive_proposal_status(
            &self,
            id: Uuid,
            expected_status: CognitiveProposalStatus,
            new_status: CognitiveProposalStatus,
            rejection_reason: Option<&str>,
            resulting_memory_id: Option<Uuid>,
            resolved_by: Option<&str>,
        ) -> Result<CognitiveProposalRecord> {
            let mut w = self.cognitive_proposals.write().unwrap();
            let p = w.get_mut(&id).ok_or(HubError::NotFound)?;
            if p.status != expected_status {
                return Err(HubError::Conflict);
            }
            p.status = new_status;
            p.rejection_reason = rejection_reason.map(str::to_string);
            p.resulting_memory_id = resulting_memory_id;
            p.resolved_by = resolved_by.map(str::to_string);
            p.updated_at = Utc::now();
            Ok(p.clone())
        }
    }
    #[async_trait]
    impl GrantStore for TestHubStore {
        async fn ensure_namespace(
            &self,
            ns: &str,
            owner: &AgentId,
        ) -> Result<(crate::domain::NamespaceInfo, bool)> {
            Ok((
                crate::domain::NamespaceInfo {
                    namespace: ns.to_string(),
                    owner_agent: owner.clone(),
                    created_at: Utc::now(),
                },
                false,
            ))
        }
        async fn get_namespace(&self, _ns: &str) -> Result<Option<crate::domain::NamespaceInfo>> {
            Ok(None)
        }
        async fn list_namespace_infos(&self) -> Result<Vec<crate::domain::NamespaceInfo>> {
            Ok(vec![])
        }
        async fn grant(&self, _r: &crate::domain::NamespaceGrantRecord) -> Result<()> {
            Ok(())
        }
        async fn revoke(&self, _ns: &str, _a: &AgentId) -> Result<()> {
            Ok(())
        }
        async fn grants_for(
            &self,
            _a: &AgentId,
        ) -> Result<Vec<crate::domain::NamespaceGrantRecord>> {
            Ok(vec![])
        }
        async fn can_read(&self, _ns: &str, _a: &AgentId) -> Result<bool> {
            Ok(true)
        }
        async fn can_write(&self, _ns: &str, _a: &AgentId) -> Result<bool> {
            Ok(true)
        }
        async fn owned_namespaces(&self, _a: &AgentId) -> Result<Vec<String>> {
            Ok(vec![])
        }
    }
    #[async_trait]
    impl AgentStore for TestHubStore {
        async fn register_agent(&self, _reg: &AgentRegistration) -> Result<AgentRecord> {
            Err(HubError::Unavailable)
        }
        async fn get_agent(&self, _id: &AgentId) -> Result<Option<AgentRecord>> {
            Ok(None)
        }
        async fn list_agents(&self) -> Result<Vec<AgentRecord>> {
            Ok(vec![])
        }
        async fn set_agent_status(&self, _id: &AgentId, _status: &str) -> Result<()> {
            Ok(())
        }
        async fn verify_agent_secret(&self, _id: &AgentId, _sec: &str) -> Result<bool> {
            Ok(false)
        }
    }

    #[async_trait]
    impl TaskStore for TestHubStore {
        async fn create_task(&self, task: &NewMemoryTask) -> Result<MemoryTask> {
            let id = task.id.unwrap_or_else(Uuid::new_v4);
            let record = MemoryTask {
                id,
                namespace: task.namespace.clone(),
                scene: task.scene,
                task: task.task.clone(),
                status: task.status,
                started_at: task.started_at,
                finished_at: task.finished_at,
                result_summary: task.result_summary.clone(),
                failure_reason: task.failure_reason.clone(),
                source_evidence_id: task.source_evidence_id,
                metadata: task.metadata.clone(),
            };
            self.tasks
                .write()
                .unwrap()
                .insert((task.namespace.clone(), id), record.clone());
            Ok(record)
        }

        async fn update_task(&self, task: &MemoryTask) -> Result<MemoryTask> {
            self.tasks
                .write()
                .unwrap()
                .insert((task.namespace.clone(), task.id), task.clone());
            Ok(task.clone())
        }

        async fn get_task(&self, namespace: &str, id: Uuid) -> Result<Option<MemoryTask>> {
            Ok(self
                .tasks
                .read()
                .unwrap()
                .get(&(namespace.to_string(), id))
                .cloned())
        }

        async fn list_tasks(
            &self,
            namespace: &str,
            status: Option<TaskStatus>,
            limit: usize,
        ) -> Result<Vec<MemoryTask>> {
            let r = self.tasks.read().unwrap();
            let mut list: Vec<_> = r
                .values()
                .filter(|t| t.namespace == namespace && status.is_none_or(|s| t.status == s))
                .cloned()
                .collect();
            list.sort_by_key(|a| std::cmp::Reverse(a.started_at));
            list.truncate(limit);
            Ok(list)
        }

        async fn all_tasks_in_namespace(&self, namespace: &str) -> Result<Vec<MemoryTask>> {
            let r = self.tasks.read().unwrap();
            Ok(r.values()
                .filter(|t| t.namespace == namespace)
                .cloned()
                .collect())
        }

        async fn delete_task(&self, namespace: &str, id: Uuid) -> Result<()> {
            let mut r = self.tasks.write().unwrap();
            if r.remove(&(namespace.to_string(), id)).is_some() {
                let mut te = self.task_experiences.write().unwrap();
                te.retain(|l| l.task_id != id);
                Ok(())
            } else {
                Err(HubError::NotFound)
            }
        }
    }

    #[async_trait]
    impl ExperienceStore for TestHubStore {
        async fn save_experience(
            &self,
            experience: &NewMemoryExperience,
        ) -> Result<MemoryExperience> {
            let id = experience.id.unwrap_or_else(Uuid::new_v4);
            let now = Utc::now();
            let record = MemoryExperience {
                id,
                namespace: experience.namespace.clone(),
                task: experience.task.clone(),
                search_keys: experience.search_keys.clone(),
                solution: experience.solution.clone(),
                preconditions: experience.preconditions.clone(),
                boundary_envs: experience.boundary_envs.clone(),
                failure_modes: experience.failure_modes.clone(),
                validation: experience.validation.clone(),
                confidence: experience.confidence,
                status: experience.status,
                feedback_score: experience.feedback_score,
                created_at: now,
                updated_at: now,
            };
            self.experiences
                .write()
                .unwrap()
                .insert((experience.namespace.clone(), id), record.clone());
            Ok(record)
        }

        async fn update_experience(
            &self,
            experience: &MemoryExperience,
        ) -> Result<MemoryExperience> {
            self.experiences.write().unwrap().insert(
                (experience.namespace.clone(), experience.id),
                experience.clone(),
            );
            Ok(experience.clone())
        }

        async fn get_experience(
            &self,
            namespace: &str,
            id: Uuid,
        ) -> Result<Option<MemoryExperience>> {
            Ok(self
                .experiences
                .read()
                .unwrap()
                .get(&(namespace.to_string(), id))
                .cloned())
        }

        async fn list_experiences(
            &self,
            namespace: &str,
            status: Option<ExperienceStatus>,
            limit: usize,
            _offset: usize,
        ) -> Result<Vec<MemoryExperience>> {
            let r = self.experiences.read().unwrap();
            let mut list: Vec<_> = r
                .values()
                .filter(|e| e.namespace == namespace && status.is_none_or(|s| e.status == s))
                .cloned()
                .collect();
            list.sort_by_key(|a| std::cmp::Reverse(a.created_at));
            list.truncate(limit);
            Ok(list)
        }

        async fn link_task_experience(&self, link: &MemoryTaskExperience) -> Result<()> {
            self.task_experiences.write().unwrap().push(link.clone());
            Ok(())
        }

        async fn list_experiences_for_task(
            &self,
            task_id: Uuid,
        ) -> Result<Vec<MemoryTaskExperience>> {
            let r = self.task_experiences.read().unwrap();
            Ok(r.iter()
                .filter(|te| te.task_id == task_id)
                .cloned()
                .collect())
        }

        async fn all_experiences_in_namespace(
            &self,
            namespace: &str,
        ) -> Result<Vec<MemoryExperience>> {
            let r = self.experiences.read().unwrap();
            Ok(r.values()
                .filter(|e| e.namespace == namespace)
                .cloned()
                .collect())
        }

        async fn all_task_experiences_in_namespace(
            &self,
            namespace: &str,
        ) -> Result<Vec<MemoryTaskExperience>> {
            let exps = self.all_experiences_in_namespace(namespace).await?;
            let exp_ids: std::collections::HashSet<_> = exps.into_iter().map(|e| e.id).collect();
            let r = self.task_experiences.read().unwrap();
            Ok(r.iter()
                .filter(|te| exp_ids.contains(&te.experience_id))
                .cloned()
                .collect())
        }

        async fn delete_experience(&self, namespace: &str, id: Uuid) -> Result<()> {
            let mut r = self.experiences.write().unwrap();
            if r.remove(&(namespace.to_string(), id)).is_some() {
                let mut te = self.task_experiences.write().unwrap();
                te.retain(|l| l.experience_id != id);
                Ok(())
            } else {
                Err(HubError::NotFound)
            }
        }
    }

    #[async_trait]
    impl CodingStore for TestHubStore {
        async fn create_thread(&self, _thread: &NewCodingThread) -> Result<CodingThread> {
            Err(HubError::Unavailable)
        }
        async fn get_thread(&self, _namespace: &str, _id: Uuid) -> Result<Option<CodingThread>> {
            Ok(None)
        }
        async fn list_threads(
            &self,
            _namespace: &str,
            _repository: Option<&str>,
            _limit: usize,
        ) -> Result<Vec<CodingThread>> {
            Ok(vec![])
        }
        async fn save_revision(
            &self,
            _rev: &NewCodingRevision,
            _diff_hash: &str,
        ) -> Result<CodingRevision> {
            Err(HubError::Unavailable)
        }
        async fn get_revision(
            &self,
            _namespace: &str,
            _id: Uuid,
        ) -> Result<Option<CodingRevision>> {
            Ok(None)
        }
        async fn get_revision_by_hash(
            &self,
            _namespace: &str,
            _repository: &str,
            _diff_hash: &str,
        ) -> Result<Option<CodingRevision>> {
            Ok(None)
        }
        async fn list_revisions_for_thread(
            &self,
            _namespace: &str,
            _thread_id: Uuid,
            _limit: usize,
        ) -> Result<Vec<CodingRevision>> {
            Ok(vec![])
        }
        async fn list_revisions_for_file(
            &self,
            _namespace: &str,
            _repository: &str,
            _file_path: &str,
            _limit: usize,
        ) -> Result<Vec<CodingRevision>> {
            Ok(vec![])
        }
        async fn all_threads_in_namespace(&self, _namespace: &str) -> Result<Vec<CodingThread>> {
            Ok(vec![])
        }
        async fn all_revisions_in_namespace(
            &self,
            _namespace: &str,
        ) -> Result<Vec<CodingRevision>> {
            Ok(vec![])
        }
        async fn delete_thread(&self, _namespace: &str, _id: Uuid) -> Result<()> {
            Ok(())
        }
        async fn delete_revision(&self, _namespace: &str, _id: Uuid) -> Result<()> {
            Ok(())
        }
    }

    fn auth() -> AuthCtx {
        AuthCtx {
            agent_id: "chat-agent".to_string(),
            account_id: common::BOOTSTRAP_ACCOUNT_ID,
            scopes: vec![Scope::Read, Scope::Write, Scope::Admin],
            acl: Default::default(),
        }
    }

    #[tokio::test]
    async fn test_manual_mode_never_auto_merges_and_produces_pending_proposals() {
        let store = Arc::new(TestHubStore::default());
        let extractor =
            Arc::new(
                MockChatCognitiveExtractor::new().with_candidates(vec![CognitiveCandidate {
                    content: "用户偏好使用暗黑主题".to_string(),
                    scene: MemoryScene::Chat,
                    layer: CognitiveLayer::Identity,
                    confidence: 0.99,
                    valid_from: None,
                    valid_until: None,
                    occurred_at: None,
                    tags: vec!["ui".to_string()],
                    metadata: serde_json::json!({}),
                    relation_kind: None,
                    target_memory_id: None,
                }]),
            );

        let service = ChatCognitiveService::new(
            store.clone(),
            extractor,
            CognitivePolicy {
                mode: CognitiveGovernanceMode::Manual,
                auto_accept_threshold: 0.95,
                ..Default::default()
            },
        );

        let input = ChatCognitiveInput {
            namespace: "test-ns".to_string(),
            session_id: "sess-1".to_string(),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "我喜欢暗黑主题".to_string(),
                timestamp: Some(Utc::now()),
            }],
            occurred_at: Some(Utc::now()),
        };

        let res = service.process_chat_session(&auth(), input).await.unwrap();

        // 绝不自动入库
        assert!(res.committed_entries.is_empty());
        assert_eq!(res.proposals.len(), 1);
        assert_eq!(
            res.proposals[0].status,
            CognitiveProposalStatus::PendingReview
        );
        // 存储中无活跃记忆条目
        let active = store.all_in_namespace("test-ns", false).await.unwrap();
        assert!(active.is_empty());
    }

    #[tokio::test]
    async fn test_review_acceptance_and_rejection() {
        let store = Arc::new(TestHubStore::default());
        let extractor =
            Arc::new(
                MockChatCognitiveExtractor::new().with_candidates(vec![CognitiveCandidate {
                    content: "用户主语言为 Rust".to_string(),
                    scene: MemoryScene::Chat,
                    layer: CognitiveLayer::Fact,
                    confidence: 0.95,
                    valid_from: None,
                    valid_until: None,
                    occurred_at: None,
                    tags: vec![],
                    metadata: serde_json::json!({}),
                    relation_kind: None,
                    target_memory_id: None,
                }]),
            );

        let service = ChatCognitiveService::new(
            store.clone(),
            extractor,
            CognitivePolicy::default(), // Manual
        );

        let input = ChatCognitiveInput {
            namespace: "test-ns".to_string(),
            session_id: "sess-review".to_string(),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "我的主语言是 Rust".to_string(),
                timestamp: None,
            }],
            occurred_at: None,
        };

        let res = service.process_chat_session(&auth(), input).await.unwrap();
        assert_eq!(res.proposals.len(), 1);

        // 1. 模拟人工审核：先拒绝
        let mut rejected_proposal = res.proposals[0].clone();
        let none_entry = service
            .review_proposal(
                &auth(),
                &mut rejected_proposal,
                false,
                Some("信息不准确".into()),
            )
            .await
            .unwrap();
        assert!(none_entry.is_none());
        assert_eq!(rejected_proposal.status, CognitiveProposalStatus::Rejected);
        // 拒绝后不能被检索为已接受事实
        assert!(store
            .all_in_namespace("test-ns", false)
            .await
            .unwrap()
            .is_empty());

        // 2. 模拟人工审核：接受
        let mut accepted_proposal = res.proposals[0].clone();
        let saved = service
            .review_proposal(&auth(), &mut accepted_proposal, true, None)
            .await
            .unwrap()
            .expect("接受应生成条目");

        assert_eq!(accepted_proposal.status, CognitiveProposalStatus::Accepted);
        assert_eq!(saved.content, "用户主语言为 Rust");
        assert_eq!(saved.version, 1);
        let active = store.all_in_namespace("test-ns", false).await.unwrap();
        assert_eq!(active.len(), 1);
    }

    #[tokio::test]
    async fn test_fact_evolution_supersedes() {
        let store = Arc::new(TestHubStore::default());
        // 先写入旧事实：“过去系统采用 MySQL 数据库”
        let old_entry = store
            .insert_initial(&MemoryEntry {
                id: Uuid::new_v4(),
                namespace: "test-ns".to_string(),
                key: None,
                kind: MemoryKind::Fact,
                content: "过去系统采用 MySQL 数据库".to_string(),
                tags: vec!["db".to_string()],
                metadata: serde_json::json!({}),
                embedding: None,
                embedding_model: None,
                source_agent: "agent".to_string(),
                confidence: 0.9,
                status: MemoryStatus::Active,
                version: 1,
                expires_at: None,
                merged_into: None,
                created_at: Utc::now() - chrono::Duration::days(10),
                updated_at: Utc::now() - chrono::Duration::days(10),
                scene: Some(MemoryScene::Chat),
                layer: Some(CognitiveLayer::Fact),
                valid_from: Some(Utc::now() - chrono::Duration::days(10)),
                valid_until: None,
                occurred_at: Some(Utc::now() - chrono::Duration::days(10)),
            })
            .await
            .unwrap();

        // 新候选：“架构评审已通过，即日起决定改用 PostgreSQL 15 作为主数据库”
        let now = Utc::now();
        let extractor =
            Arc::new(
                MockChatCognitiveExtractor::new().with_candidates(vec![CognitiveCandidate {
                    content: "架构评审已通过，即日起决定改用 PostgreSQL 15 作为主数据库"
                        .to_string(),
                    scene: MemoryScene::Chat,
                    layer: CognitiveLayer::Fact,
                    confidence: 0.98,
                    valid_from: Some(now),
                    valid_until: None,
                    occurred_at: Some(now),
                    tags: vec!["db".to_string()],
                    metadata: serde_json::json!({}),
                    relation_kind: Some(MemoryRelationKind::Supersedes),
                    target_memory_id: Some(old_entry.id),
                }]),
            );

        let service = ChatCognitiveService::new(
            store.clone(),
            extractor,
            CognitivePolicy {
                mode: CognitiveGovernanceMode::FullAuto,
                auto_accept_threshold: 0.9,
                ..Default::default()
            },
        );

        let input = ChatCognitiveInput {
            namespace: "test-ns".to_string(),
            session_id: "sess-evo".to_string(),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "我们决定从 MySQL 改用 PostgreSQL 15".to_string(),
                timestamp: Some(now),
            }],
            occurred_at: Some(now),
        };

        let res = service.process_chat_session(&auth(), input).await.unwrap();
        assert_eq!(res.committed_entries.len(), 1);
        let new_entry = &res.committed_entries[0];

        // 验证关系网：建立了 Supersedes 关系 (new -> old)
        let relations = store
            .list_relations_from("test-ns", new_entry.id)
            .await
            .unwrap();
        assert!(relations.iter().any(|r| {
            r.to_memory_id == old_entry.id && r.relation_kind == MemoryRelationKind::Supersedes
        }));

        // 验证旧事实被封堵 valid_until
        let updated_old = store.get("test-ns", old_entry.id).await.unwrap();
        assert!(updated_old.valid_until.is_some());
        assert_eq!(updated_old.version, 2);
    }

    /// supersedes 的次要写（关系建立 / 旧事实封堵）失败时不得拖垮主流程：
    /// 新事实必须已入库并返回成功。日志 warn 断言受限于测试基建（无日志捕获），
    /// 以错误注入 + 主流程结果作为替代验证。
    #[tokio::test]
    async fn test_supersedes_secondary_write_failure_does_not_fail_main_flow() {
        let store = Arc::new(TestHubStore::default());
        let old_entry = store
            .insert_initial(&MemoryEntry {
                id: Uuid::new_v4(),
                namespace: "test-ns".to_string(),
                key: None,
                kind: MemoryKind::Fact,
                content: "过去系统采用 MySQL 数据库".to_string(),
                tags: vec!["db".to_string()],
                metadata: serde_json::json!({}),
                embedding: None,
                embedding_model: None,
                source_agent: "agent".to_string(),
                confidence: 0.9,
                status: MemoryStatus::Active,
                version: 1,
                expires_at: None,
                merged_into: None,
                created_at: Utc::now() - chrono::Duration::days(10),
                updated_at: Utc::now() - chrono::Duration::days(10),
                scene: Some(MemoryScene::Chat),
                layer: Some(CognitiveLayer::Fact),
                valid_from: Some(Utc::now() - chrono::Duration::days(10)),
                valid_until: None,
                occurred_at: Some(Utc::now() - chrono::Duration::days(10)),
            })
            .await
            .unwrap();

        // 注入两类次要写失败
        store
            .fail_add_relation
            .store(true, std::sync::atomic::Ordering::Relaxed);
        store
            .fail_revise
            .store(true, std::sync::atomic::Ordering::Relaxed);

        let extractor =
            Arc::new(
                MockChatCognitiveExtractor::new().with_candidates(vec![CognitiveCandidate {
                    content: "即日起决定改用 PostgreSQL 15 作为主数据库".to_string(),
                    scene: MemoryScene::Chat,
                    layer: CognitiveLayer::Fact,
                    confidence: 0.98,
                    valid_from: None,
                    valid_until: None,
                    occurred_at: Some(Utc::now()),
                    tags: vec!["db".to_string()],
                    metadata: serde_json::json!({}),
                    relation_kind: Some(MemoryRelationKind::Supersedes),
                    target_memory_id: Some(old_entry.id),
                }]),
            );

        let service = ChatCognitiveService::new(
            store.clone(),
            extractor,
            CognitivePolicy {
                mode: CognitiveGovernanceMode::FullAuto,
                auto_accept_threshold: 0.9,
                ..Default::default()
            },
        );

        let input = ChatCognitiveInput {
            namespace: "test-ns".to_string(),
            session_id: "sess-supersede-fail".to_string(),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "我们决定从 MySQL 改用 PostgreSQL 15".to_string(),
                timestamp: Some(Utc::now()),
            }],
            occurred_at: Some(Utc::now()),
        };

        // 主流程成功返回，新事实已入库（次要失败只降级为 warn）
        let res = service.process_chat_session(&auth(), input).await.unwrap();
        assert_eq!(res.committed_entries.len(), 1);
        let new_entry = &res.committed_entries[0];
        assert_eq!(
            new_entry.content,
            "即日起决定改用 PostgreSQL 15 作为主数据库"
        );

        // 关系确实没建立、旧事实确实没被封堵（失败被吞但不假装成功）
        let relations = store
            .list_relations_from("test-ns", new_entry.id)
            .await
            .unwrap();
        assert!(relations.is_empty());
        let old_after = store.get("test-ns", old_entry.id).await.unwrap();
        assert!(old_after.valid_until.is_none());
    }

    #[tokio::test]
    async fn test_speculative_intention_rejected() {
        let store = Arc::new(TestHubStore::default());
        let extractor =
            Arc::new(
                MockChatCognitiveExtractor::new().with_candidates(vec![CognitiveCandidate {
                    content: "下半年我们可能会考虑采用 Rust 重构某些服务".to_string(),
                    scene: MemoryScene::Chat,
                    layer: CognitiveLayer::Intention,
                    confidence: 0.8,
                    valid_from: None,
                    valid_until: None,
                    occurred_at: None,
                    tags: vec![],
                    metadata: serde_json::json!({}),
                    relation_kind: None,
                    target_memory_id: None,
                }]),
            );

        let service = ChatCognitiveService::new(
            store.clone(),
            extractor,
            CognitivePolicy {
                mode: CognitiveGovernanceMode::FullAuto,
                auto_accept_threshold: 0.5,
                ..Default::default()
            },
        );

        let input = ChatCognitiveInput {
            namespace: "test-ns".to_string(),
            session_id: "sess-vague".to_string(),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "可能会考虑采用 Rust".to_string(),
                timestamp: None,
            }],
            occurred_at: None,
        };

        let res = service.process_chat_session(&auth(), input).await.unwrap();
        // 绝不无证据升级为确定意图
        assert!(res.committed_entries.is_empty());
        assert_eq!(res.proposals.len(), 1);
        assert_eq!(res.proposals[0].status, CognitiveProposalStatus::Rejected);
        assert!(res.proposals[0]
            .rejection_reason
            .as_ref()
            .unwrap()
            .contains("含糊猜测"));
    }

    #[tokio::test]
    async fn test_expired_intention_excluded_from_active() {
        let store = Arc::new(TestHubStore::default());
        let now = Utc::now();

        // 1. 已过期意图
        let expired = MemoryEntry {
            id: Uuid::new_v4(),
            namespace: "test-ns".to_string(),
            key: None,
            kind: MemoryKind::Preference,
            content: "本周五前完成 P1 评审".to_string(),
            tags: vec![],
            metadata: serde_json::json!({}),
            embedding: None,
            embedding_model: None,
            source_agent: "agent".to_string(),
            confidence: 0.9,
            status: MemoryStatus::Active,
            version: 1,
            expires_at: Some(now - chrono::Duration::hours(2)),
            merged_into: None,
            created_at: now - chrono::Duration::days(5),
            updated_at: now - chrono::Duration::days(5),
            scene: Some(MemoryScene::Chat),
            layer: Some(CognitiveLayer::Intention),
            valid_from: Some(now - chrono::Duration::days(5)),
            valid_until: Some(now - chrono::Duration::hours(2)), // 已过期
            occurred_at: Some(now - chrono::Duration::days(5)),
        };
        store.insert(&expired).await.unwrap();

        // 2. 有效意图
        let active = MemoryEntry {
            id: Uuid::new_v4(),
            namespace: "test-ns".to_string(),
            key: None,
            kind: MemoryKind::Preference,
            content: "下季度推进多模态嵌入研发".to_string(),
            tags: vec![],
            metadata: serde_json::json!({}),
            embedding: None,
            embedding_model: None,
            source_agent: "agent".to_string(),
            confidence: 0.9,
            status: MemoryStatus::Active,
            version: 1,
            expires_at: Some(now + chrono::Duration::days(30)),
            merged_into: None,
            created_at: now,
            updated_at: now,
            scene: Some(MemoryScene::Chat),
            layer: Some(CognitiveLayer::Intention),
            valid_from: Some(now),
            valid_until: Some(now + chrono::Duration::days(30)), // 未来到期
            occurred_at: Some(now),
        };
        store.insert(&active).await.unwrap();

        let service = ChatCognitiveService::new(
            store.clone(),
            Arc::new(MockChatCognitiveExtractor::new()),
            CognitivePolicy::default(),
        );

        let list = service
            .list_active_intentions(&auth(), "test-ns")
            .await
            .unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, active.id);
        assert_eq!(list[0].content, "下季度推进多模态嵌入研发");
    }

    #[tokio::test]
    async fn test_model_failure_preserves_evidence() {
        let store = Arc::new(TestHubStore::default());
        let extractor = Arc::new(MockChatCognitiveExtractor::new().with_failure("timeout"));

        let service =
            ChatCognitiveService::new(store.clone(), extractor, CognitivePolicy::default());

        let input = ChatCognitiveInput {
            namespace: "test-ns".to_string(),
            session_id: "sess-fail".to_string(),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "重要业务会话数据".to_string(),
                timestamp: None,
            }],
            occurred_at: None,
        };

        // 执行失败
        let res = service.process_chat_session(&auth(), input).await;
        assert!(res.is_err());

        // 核心安全保证：即便 LLM 超时或失败，原始证据仍被持久化保存！
        let evs = store.all_evidence_in_namespace("test-ns").await.unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].source_ref.as_deref(), Some("sess-fail"));
        assert!(evs[0]
            .excerpt
            .as_ref()
            .unwrap()
            .contains("重要业务会话数据"));
    }

    /// 证据入库前的安全闸门：凭证脱敏 + 保留期限
    ///
    /// 回归：原始会话曾被原样写进不可变证据表——一旦写入就再也追不回来。
    #[tokio::test]
    async fn test_evidence_is_redacted_before_persistence() {
        let store = Arc::new(TestHubStore::default());
        let extractor = Arc::new(MockChatCognitiveExtractor::new().with_failure("timeout"));
        let service = ChatCognitiveService::new(
            store.clone(),
            extractor,
            CognitivePolicy {
                evidence_retention_days: 90,
                ..Default::default()
            },
        );

        let secret = concat!("sk-proj-", "abcdefghijklmnopqrstuvwxyz012345");
        let input = ChatCognitiveInput {
            namespace: "safe-ns".to_string(),
            session_id: "sess-secret".to_string(),
            messages: vec![
                ChatMessage {
                    role: ChatRole::User,
                    content: format!("这是我的密钥 {secret}，请保密"),
                    timestamp: None,
                },
                ChatMessage {
                    role: ChatRole::Assistant,
                    content: "已记录。".to_string(),
                    timestamp: None,
                },
            ],
            occurred_at: None,
        };

        // 抽取失败也要落证据（这是证据链的核心保证）
        let res = service.process_chat_session(&auth(), input).await;
        assert!(res.is_err());

        let evs = store.all_evidence_in_namespace("safe-ns").await.unwrap();
        assert_eq!(evs.len(), 1);
        let excerpt = evs[0].excerpt.as_ref().unwrap();
        assert!(
            !excerpt.contains(secret),
            "凭证不得进入不可变证据表：{excerpt}"
        );
        assert!(
            excerpt.contains("[已脱敏:凭证]"),
            "凭证应被占位符替换：{excerpt}"
        );
        // 保留期限必须落库
        assert!(
            evs[0].expires_at.is_some(),
            "证据必须带保留期限，到期由清理任务删除"
        );
    }

    /// 保留期限 = 0 表示永久保留
    #[tokio::test]
    async fn test_evidence_retention_zero_means_forever() {
        let store = Arc::new(TestHubStore::default());
        let extractor = Arc::new(MockChatCognitiveExtractor::new().with_failure("timeout"));
        let service = ChatCognitiveService::new(
            store.clone(),
            extractor,
            CognitivePolicy {
                evidence_retention_days: 0,
                ..Default::default()
            },
        );

        let input = ChatCognitiveInput {
            namespace: "keep-ns".to_string(),
            session_id: "sess-keep".to_string(),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "普通内容".to_string(),
                timestamp: None,
            }],
            occurred_at: None,
        };
        assert!(service.process_chat_session(&auth(), input).await.is_err());

        let evs = store.all_evidence_in_namespace("keep-ns").await.unwrap();
        assert_eq!(evs.len(), 1);
        assert!(
            evs[0].expires_at.is_none(),
            "保留期限为 0 时证据不设到期时间"
        );
    }

    #[tokio::test]
    async fn test_full_auto_respects_acceptance_threshold() {
        // 回归：FullAuto 曾无条件自动入库，auto_accept_threshold 完全不生效
        let store = Arc::new(TestHubStore::default());
        let extractor =
            Arc::new(
                MockChatCognitiveExtractor::new().with_candidates(vec![CognitiveCandidate {
                    content: "团队规范：提交前必须补齐集成测试".to_string(),
                    scene: MemoryScene::Chat,
                    layer: CognitiveLayer::Knowledge,
                    // 低于自动接受阈值 0.9
                    confidence: 0.6,
                    valid_from: None,
                    valid_until: None,
                    occurred_at: None,
                    tags: vec![],
                    metadata: serde_json::json!({}),
                    relation_kind: None,
                    target_memory_id: None,
                }]),
            );

        let service = ChatCognitiveService::new(
            store.clone(),
            extractor,
            CognitivePolicy {
                mode: CognitiveGovernanceMode::FullAuto,
                auto_accept_threshold: 0.9,
                ..Default::default()
            },
        );

        let res = service
            .process_chat_session(
                &auth(),
                ChatCognitiveInput {
                    namespace: "test-ns".to_string(),
                    session_id: "sess-threshold".to_string(),
                    messages: vec![ChatMessage {
                        role: ChatRole::User,
                        content: "提交前要补集成测试".to_string(),
                        timestamp: None,
                    }],
                    occurred_at: None,
                },
            )
            .await
            .unwrap();

        assert!(
            res.committed_entries.is_empty(),
            "低于阈值的候选不得在 FullAuto 下自动入库"
        );
        assert_eq!(res.proposals.len(), 1);
        assert_eq!(res.proposals[0].status, CognitiveProposalStatus::Rejected);
        assert!(
            res.proposals[0]
                .rejection_reason
                .as_deref()
                .unwrap_or_default()
                .contains("自动接受阈值"),
            "拒绝原因必须说明阈值：{:?}",
            res.proposals[0].rejection_reason
        );

        // 同样的候选在阈值放宽后应正常入库（确认不是把候选本身判死）
        let service_loose = ChatCognitiveService::new(
            store.clone(),
            Arc::new(
                MockChatCognitiveExtractor::new().with_candidates(vec![CognitiveCandidate {
                    content: "团队规范：提交前必须补齐端到端回归".to_string(),
                    scene: MemoryScene::Chat,
                    layer: CognitiveLayer::Knowledge,
                    confidence: 0.6,
                    valid_from: None,
                    valid_until: None,
                    occurred_at: None,
                    tags: vec![],
                    metadata: serde_json::json!({}),
                    relation_kind: None,
                    target_memory_id: None,
                }]),
            ),
            CognitivePolicy {
                mode: CognitiveGovernanceMode::FullAuto,
                auto_accept_threshold: 0.5,
                ..Default::default()
            },
        );
        let res_loose = service_loose
            .process_chat_session(
                &auth(),
                ChatCognitiveInput {
                    namespace: "test-ns".to_string(),
                    session_id: "sess-loose".to_string(),
                    messages: vec![ChatMessage {
                        role: ChatRole::User,
                        content: "提交前要补端到端回归".to_string(),
                        timestamp: None,
                    }],
                    occurred_at: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(res_loose.committed_entries.len(), 1);
    }

    #[tokio::test]
    async fn test_duplicate_input_idempotency() {
        let store = Arc::new(TestHubStore::default());
        let extractor =
            Arc::new(
                MockChatCognitiveExtractor::new().with_candidates(vec![CognitiveCandidate {
                    content: "团队规范：统一采用 PR 流程".to_string(),
                    scene: MemoryScene::Chat,
                    layer: CognitiveLayer::Knowledge,
                    confidence: 0.99,
                    valid_from: None,
                    valid_until: None,
                    occurred_at: None,
                    tags: vec![],
                    metadata: serde_json::json!({}),
                    relation_kind: None,
                    target_memory_id: None,
                }]),
            );

        let service = ChatCognitiveService::new(
            store.clone(),
            extractor,
            CognitivePolicy {
                mode: CognitiveGovernanceMode::FullAuto,
                auto_accept_threshold: 0.9,
                ..Default::default()
            },
        );

        let input = ChatCognitiveInput {
            namespace: "test-ns".to_string(),
            session_id: "sess-dup".to_string(),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "统一使用 PR 流程".to_string(),
                timestamp: None,
            }],
            occurred_at: None,
        };

        // 第一次执行：成功入库
        let res1 = service
            .process_chat_session(&auth(), input.clone())
            .await
            .unwrap();
        assert_eq!(res1.committed_entries.len(), 1);

        // 第二次执行（重试同一输入）：幂等阻断，不产生第二份条目
        let res2 = service.process_chat_session(&auth(), input).await.unwrap();
        assert!(res2.committed_entries.is_empty());

        let total = store.all_in_namespace("test-ns", false).await.unwrap();
        assert_eq!(total.len(), 1, "同一事实重复输入不得无限复制");
    }

    /// 负例：两条只是恰好分别提到 MySQL 和 PostgreSQL 的不相关事实，
    /// 不得被判为 supersedes（旧实现硬编码该技术栈对，误封旧事实 valid_until）
    #[tokio::test]
    async fn test_unrelated_tech_mentions_do_not_supersede() {
        let store = Arc::new(TestHubStore::default());
        let now = Utc::now();

        // 两条互不相关的事实：一条讲 MySQL 备份策略，一条是含"改用"措辞的
        // PostgreSQL 迁移演练记录——标签无交集，不是同一主体的演化
        let old = store
            .insert_initial(&MemoryEntry {
                id: Uuid::new_v4(),
                namespace: "test-ns".to_string(),
                key: None,
                kind: MemoryKind::Fact,
                content: "MySQL 备份策略为每日凌晨全量".to_string(),
                tags: vec!["backup".to_string()],
                metadata: serde_json::json!({}),
                embedding: None,
                embedding_model: None,
                source_agent: "agent".to_string(),
                confidence: 0.9,
                status: MemoryStatus::Active,
                version: 1,
                expires_at: None,
                merged_into: None,
                created_at: now - chrono::Duration::days(10),
                updated_at: now - chrono::Duration::days(10),
                scene: Some(MemoryScene::Chat),
                layer: Some(CognitiveLayer::Fact),
                valid_from: Some(now - chrono::Duration::days(10)),
                valid_until: None,
                occurred_at: Some(now - chrono::Duration::days(10)),
            })
            .await
            .unwrap();

        let extractor =
            Arc::new(
                MockChatCognitiveExtractor::new().with_candidates(vec![CognitiveCandidate {
                    content: "演练记录：本周把只读副本从 MySQL 改用 PostgreSQL 驱动重连"
                        .to_string(),
                    scene: MemoryScene::Chat,
                    layer: CognitiveLayer::Fact,
                    confidence: 0.95,
                    valid_from: Some(now),
                    valid_until: None,
                    occurred_at: Some(now),
                    // 标签与旧事实无交集
                    tags: vec!["drill".to_string()],
                    metadata: serde_json::json!({}),
                    relation_kind: None,
                    target_memory_id: None,
                }]),
            );

        let service = ChatCognitiveService::new(
            store.clone(),
            extractor,
            CognitivePolicy {
                mode: CognitiveGovernanceMode::FullAuto,
                auto_accept_threshold: 0.9,
                ..Default::default()
            },
        );

        let input = ChatCognitiveInput {
            namespace: "test-ns".to_string(),
            session_id: "sess-unrelated".to_string(),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "演练：只读副本改用 PostgreSQL 驱动".to_string(),
                timestamp: Some(now),
            }],
            occurred_at: Some(now),
        };

        let res = service.process_chat_session(&auth(), input).await.unwrap();
        assert_eq!(res.committed_entries.len(), 1);

        // 不得建立任何 Supersedes 关系
        let relations = store
            .list_relations_from("test-ns", res.committed_entries[0].id)
            .await
            .unwrap();
        assert!(
            relations.is_empty(),
            "不相关事实不得被判为 supersedes：{relations:?}"
        );

        // 旧事实不得被封堵 valid_until
        let unchanged = store.get("test-ns", old.id).await.unwrap();
        assert!(
            unchanged.valid_until.is_none(),
            "无关旧事实的有效期不得被改写"
        );
        assert_eq!(unchanged.version, 1);
    }

    /// 确定性：同标签多条旧事实时，supersedes 目标必须是"最新"的那条
    /// （occurred_at 优先，其次 updated_at、created_at），与 all_in_namespace
    /// 返回顺序无关——旧实现取 `iter().rev()` 的"最后一个"，选择不确定
    #[tokio::test]
    async fn test_supersedes_target_picks_latest_matching_entry() {
        let store = Arc::new(TestHubStore::default());
        let now = Utc::now();

        let mk = |days_ago: i64, content: &str| MemoryEntry {
            id: Uuid::new_v4(),
            namespace: "test-ns".to_string(),
            key: None,
            kind: MemoryKind::Fact,
            content: content.to_string(),
            tags: vec!["db".to_string()],
            metadata: serde_json::json!({}),
            embedding: None,
            embedding_model: None,
            source_agent: "agent".to_string(),
            confidence: 0.9,
            status: MemoryStatus::Active,
            version: 1,
            expires_at: None,
            merged_into: None,
            created_at: now - chrono::Duration::days(days_ago),
            updated_at: now - chrono::Duration::days(days_ago),
            scene: Some(MemoryScene::Chat),
            layer: Some(CognitiveLayer::Fact),
            valid_from: Some(now - chrono::Duration::days(days_ago)),
            valid_until: None,
            occurred_at: Some(now - chrono::Duration::days(days_ago)),
        };
        let older = store
            .insert_initial(&mk(30, "旧款数据库为 MySQL 5.7"))
            .await
            .unwrap();
        let newer = store
            .insert_initial(&mk(5, "上一代数据库为 MySQL 8.0"))
            .await
            .unwrap();

        let extractor =
            Arc::new(
                MockChatCognitiveExtractor::new().with_candidates(vec![CognitiveCandidate {
                    content: "架构评审已通过，即日起决定改用 PostgreSQL 15 作为主数据库"
                        .to_string(),
                    scene: MemoryScene::Chat,
                    layer: CognitiveLayer::Fact,
                    confidence: 0.98,
                    valid_from: Some(now),
                    valid_until: None,
                    occurred_at: Some(now),
                    tags: vec!["db".to_string()],
                    metadata: serde_json::json!({}),
                    relation_kind: None,
                    target_memory_id: None,
                }]),
            );

        let service = ChatCognitiveService::new(
            store.clone(),
            extractor,
            CognitivePolicy {
                mode: CognitiveGovernanceMode::FullAuto,
                auto_accept_threshold: 0.9,
                ..Default::default()
            },
        );

        let input = ChatCognitiveInput {
            namespace: "test-ns".to_string(),
            session_id: "sess-latest".to_string(),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "决定改用 PostgreSQL 15".to_string(),
                timestamp: Some(now),
            }],
            occurred_at: Some(now),
        };

        let res = service.process_chat_session(&auth(), input).await.unwrap();
        assert_eq!(res.committed_entries.len(), 1);
        let new_entry = &res.committed_entries[0];

        let relations = store
            .list_relations_from("test-ns", new_entry.id)
            .await
            .unwrap();
        assert_eq!(relations.len(), 1, "恰好建立一条 Supersedes 关系");
        assert_eq!(
            relations[0].to_memory_id, newer.id,
            "必须封堵最新的同主体旧事实"
        );
        assert_ne!(relations[0].to_memory_id, older.id);

        let blocked = store.get("test-ns", newer.id).await.unwrap();
        assert!(blocked.valid_until.is_some(), "最新旧事实应被封堵");
        let untouched = store.get("test-ns", older.id).await.unwrap();
        assert!(untouched.valid_until.is_none(), "更早的旧事实保持不变");
    }

    // ===== 认知提案持久化与裁决闭环（链路一修复的端到端验证） =====

    fn candidate(content: &str, confidence: f32) -> CognitiveCandidate {
        CognitiveCandidate {
            content: content.to_string(),
            scene: MemoryScene::Chat,
            layer: CognitiveLayer::Fact,
            confidence,
            valid_from: None,
            valid_until: None,
            occurred_at: None,
            tags: vec![],
            metadata: serde_json::json!({}),
            relation_kind: None,
            target_memory_id: None,
        }
    }

    fn manual_service(
        store: &Arc<TestHubStore>,
        candidates: Vec<CognitiveCandidate>,
    ) -> ChatCognitiveService {
        ChatCognitiveService::new(
            store.clone(),
            Arc::new(MockChatCognitiveExtractor::new().with_candidates(candidates)),
            CognitivePolicy {
                mode: CognitiveGovernanceMode::Manual,
                ..Default::default()
            },
        )
    }

    /// propose → list → 三种裁决（采纳/驳回/编辑后合并）全链路
    #[tokio::test]
    async fn test_cognitive_proposal_persist_list_and_resolve_roundtrip() {
        let store = Arc::new(TestHubStore::default());
        let service = manual_service(
            &store,
            vec![
                candidate("用户主语言为 Rust", 0.95),
                candidate("用户偏好暗黑主题", 0.9),
                candidate("团队每周五发布", 0.85),
            ],
        );

        let res = service
            .process_chat_session(
                &auth(),
                ChatCognitiveInput {
                    namespace: "review-ns".to_string(),
                    session_id: "sess-review-e2e".to_string(),
                    messages: vec![ChatMessage {
                        role: ChatRole::User,
                        content: "记录偏好".to_string(),
                        timestamp: None,
                    }],
                    occurred_at: None,
                },
            )
            .await
            .unwrap();

        // Manual 模式：三候选全部生成待审提案且已持久化
        assert_eq!(res.proposals.len(), 3);
        assert!(res
            .proposals
            .iter()
            .all(|p| p.status == CognitiveProposalStatus::PendingReview));
        let listed = service
            .list_proposals(
                &auth(),
                "review-ns",
                Some(CognitiveProposalStatus::PendingReview),
            )
            .await
            .unwrap();
        assert_eq!(listed.len(), 3);
        // 返回值中的 ID 与存储中的 ID 一致（落库后的真实 ID）
        for p in &res.proposals {
            assert!(listed.iter().any(|l| l.id == p.id));
        }

        // 1. 驳回
        let rejected_id = res.proposals[0].id;
        let outcome = service
            .review_persisted_proposal(
                &auth(),
                rejected_id,
                CognitiveReviewDecision::Reject {
                    reason: Some("信息不准确".into()),
                },
            )
            .await
            .unwrap();
        assert!(outcome.is_none());
        let after = store.get_cognitive_proposal(rejected_id).await.unwrap();
        assert_eq!(after.status, CognitiveProposalStatus::Rejected);
        assert_eq!(after.rejection_reason.as_deref(), Some("信息不准确"));
        assert_eq!(after.resolved_by.as_deref(), Some("chat-agent"));

        // 2. 采纳
        let accepted_id = res.proposals[1].id;
        let entry = service
            .review_persisted_proposal(
                &auth(),
                accepted_id,
                CognitiveReviewDecision::Accept {
                    modified_content: None,
                },
            )
            .await
            .unwrap()
            .expect("采纳应生成条目");
        assert_eq!(entry.content, "用户偏好暗黑主题");
        let after = store.get_cognitive_proposal(accepted_id).await.unwrap();
        assert_eq!(after.status, CognitiveProposalStatus::Accepted);
        assert_eq!(after.resulting_memory_id, Some(entry.id));

        // 3. 编辑后合并：入库正文必须是修订版
        let modified_id = res.proposals[2].id;
        let entry = service
            .review_persisted_proposal(
                &auth(),
                modified_id,
                CognitiveReviewDecision::Accept {
                    modified_content: Some("团队每周五 17:00 发布（法定假日顺延）".into()),
                },
            )
            .await
            .unwrap()
            .expect("编辑后合并应生成条目");
        assert_eq!(entry.content, "团队每周五 17:00 发布（法定假日顺延）");
        let after = store.get_cognitive_proposal(modified_id).await.unwrap();
        assert_eq!(after.status, CognitiveProposalStatus::Accepted);

        // 裁决后的队列：pending_review 清零，accepted 2 条 + rejected 1 条
        let pending = service
            .list_proposals(
                &auth(),
                "review-ns",
                Some(CognitiveProposalStatus::PendingReview),
            )
            .await
            .unwrap();
        assert!(pending.is_empty());
        let accepted = service
            .list_proposals(
                &auth(),
                "review-ns",
                Some(CognitiveProposalStatus::Accepted),
            )
            .await
            .unwrap();
        assert_eq!(accepted.len(), 2);

        // 记忆条目：恰好 2 条（采纳 + 编辑后合并；驳回不入库）
        let entries = store.all_in_namespace("review-ns", false).await.unwrap();
        assert_eq!(entries.len(), 2);
    }

    /// 已裁决提案再次裁决必须 Conflict，绝不重复入库
    #[tokio::test]
    async fn test_double_resolution_conflicts_instead_of_duplicate_commit() {
        let store = Arc::new(TestHubStore::default());
        let service = manual_service(&store, vec![candidate("用户主语言为 Rust", 0.95)]);
        let res = service
            .process_chat_session(
                &auth(),
                ChatCognitiveInput {
                    namespace: "dup-ns".to_string(),
                    session_id: "sess-dup".to_string(),
                    messages: vec![ChatMessage {
                        role: ChatRole::User,
                        content: "记录".to_string(),
                        timestamp: None,
                    }],
                    occurred_at: None,
                },
            )
            .await
            .unwrap();
        let id = res.proposals[0].id;

        // 第一次采纳成功
        service
            .review_persisted_proposal(
                &auth(),
                id,
                CognitiveReviewDecision::Accept {
                    modified_content: None,
                },
            )
            .await
            .unwrap()
            .expect("首次采纳应成功");

        // 第二次裁决（无论动作）必须 Conflict
        let err = service
            .review_persisted_proposal(
                &auth(),
                id,
                CognitiveReviewDecision::Accept {
                    modified_content: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, common::HubError::Conflict), "got: {:?}", err);

        // 记忆条目只有 1 条
        let entries = store.all_in_namespace("dup-ns", false).await.unwrap();
        assert_eq!(entries.len(), 1);
    }

    /// 编辑内容先过候选校验：非法修订不落库，提案停留在 pending_review
    #[tokio::test]
    async fn test_modified_content_validated_before_commit() {
        let store = Arc::new(TestHubStore::default());
        let service = manual_service(&store, vec![candidate("正常候选内容", 0.9)]);
        let res = service
            .process_chat_session(
                &auth(),
                ChatCognitiveInput {
                    namespace: "mod-ns".to_string(),
                    session_id: "sess-mod".to_string(),
                    messages: vec![ChatMessage {
                        role: ChatRole::User,
                        content: "记录".to_string(),
                        timestamp: None,
                    }],
                    occurred_at: None,
                },
            )
            .await
            .unwrap();
        let id = res.proposals[0].id;

        // 修订正文携带真实形态的凭证必须被拒绝
        // （concat 拆分避免仓库级 secret scanning 误判为真实泄露）
        let leaky = format!("泄漏的内容 {}", concat!("sk-proj-", "uxg4cgpiZt9k2mQw"));
        let err = service
            .review_persisted_proposal(
                &auth(),
                id,
                CognitiveReviewDecision::Accept {
                    modified_content: Some(leaky),
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, common::HubError::InvalidArgument(_)),
            "got: {:?}",
            err
        );

        // 提案仍可裁决（未被错误推进），修订为空串同样被拒
        let after = store.get_cognitive_proposal(id).await.unwrap();
        assert_eq!(after.status, CognitiveProposalStatus::PendingReview);
        let err = service
            .review_persisted_proposal(
                &auth(),
                id,
                CognitiveReviewDecision::Accept {
                    modified_content: Some("   ".into()),
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, common::HubError::InvalidArgument(_)));
    }

    /// SemiAuto 低于阈值的候选同样进入持久化待审队列
    #[tokio::test]
    async fn test_semiauto_below_threshold_persists_pending_proposal() {
        let store = Arc::new(TestHubStore::default());
        let service = ChatCognitiveService::new(
            store.clone(),
            Arc::new(
                MockChatCognitiveExtractor::new()
                    .with_candidates(vec![candidate("低置信度候选", 0.7)]),
            ),
            CognitivePolicy {
                mode: CognitiveGovernanceMode::SemiAuto,
                auto_accept_threshold: 0.9,
                ..Default::default()
            },
        );
        let res = service
            .process_chat_session(
                &auth(),
                ChatCognitiveInput {
                    namespace: "semi-ns".to_string(),
                    session_id: "sess-semi".to_string(),
                    messages: vec![ChatMessage {
                        role: ChatRole::User,
                        content: "记录".to_string(),
                        timestamp: None,
                    }],
                    occurred_at: None,
                },
            )
            .await
            .unwrap();
        assert!(res.committed_entries.is_empty());
        assert_eq!(res.proposals.len(), 1);
        // 落库可查——人工可救回
        let stored = store
            .get_cognitive_proposal(res.proposals[0].id)
            .await
            .unwrap();
        assert_eq!(stored.status, CognitiveProposalStatus::PendingReview);
    }

    /// FullAuto 高于阈值自动入库：不产生待审提案，行为保持既有语义
    #[tokio::test]
    async fn test_fullauto_high_confidence_still_autocommits() {
        let store = Arc::new(TestHubStore::default());
        let service = ChatCognitiveService::new(
            store.clone(),
            Arc::new(
                MockChatCognitiveExtractor::new()
                    .with_candidates(vec![candidate("确定事实", 0.99)]),
            ),
            CognitivePolicy {
                mode: CognitiveGovernanceMode::FullAuto,
                auto_accept_threshold: 0.95,
                ..Default::default()
            },
        );
        let res = service
            .process_chat_session(
                &auth(),
                ChatCognitiveInput {
                    namespace: "full-ns".to_string(),
                    session_id: "sess-full".to_string(),
                    messages: vec![ChatMessage {
                        role: ChatRole::User,
                        content: "记录".to_string(),
                        timestamp: None,
                    }],
                    occurred_at: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(res.committed_entries.len(), 1);
        assert!(res.proposals.is_empty());
    }
}
