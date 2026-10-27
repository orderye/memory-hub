//! Working 经验记忆服务 (P3)
//!
//! 核心原则：
//! 1. 任务事实层（WorkingFact）与经验层（Experience）严格解耦：
//!    - 任务事实记录“本次任务做了什么”，每个结束的任务均完整归档；
//!    - 经验层通过严格的价值门槛（通用性、可验证性、环境边界、无凭证泄露）；
//! 2. 幂等控制：同一任务多次 finish 保持幂等；
//! 3. 反馈闭环：负反馈只降低反馈分（单一因子）与排序权重，绝不物理删除经验；
//! 4. 环境边界感知：执行环境不匹配时显式警告，严禁伪称适用；
//! 5. 跨命名空间强隔离：账户沙箱下任何跨 namespace 关联均被阻断。

use crate::account_scope::AccountScopedStore;
use crate::domain::{
    validate_experience_candidate, ExperienceCandidate, ExperienceChangeKind, ExperienceStatus,
    ExperienceTransition, ExperienceVersion, MemoryExperience, MemoryScene, MemoryTask,
    MemoryTaskExperience, NewExperienceVersion, NewMemoryExperience, NewMemoryTask, TaskStatus,
};
use crate::infra::{AccessChecker, MemoryHubStore};
use chrono::Utc;
use common::{AccountId, AuthCtx, HubError, Result, Scope};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

/// 经验检索匹配结果
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperienceSearchResult {
    pub experience: MemoryExperience,
    pub score: f32,
    pub env_mismatch: bool,
    pub boundary_warning: Option<String>,
}

/// Working 经验记忆服务
pub struct WorkingMemoryService {
    store: Arc<dyn MemoryHubStore>,
    /// 命名空间级访问控制。
    ///
    /// `None` 仅用于单元测试与纯库嵌入场景（此时只校验 Scope）；
    /// **生产装配必须注入**，否则同一账户内的 Agent 可以凭令牌读取任意命名空间。
    access: Option<Arc<dyn AccessChecker>>,
}

impl WorkingMemoryService {
    pub fn new(store: Arc<dyn MemoryHubStore>) -> Self {
        Self {
            store,
            access: None,
        }
    }

    /// 注入命名空间 ACL 检查器（生产必须调用）
    pub fn with_access(mut self, access: Arc<dyn AccessChecker>) -> Self {
        self.access = Some(access);
        self
    }

    /// 切换底层作用域存储（账户沙箱化）
    pub fn scoped(&self, account_id: AccountId) -> Self {
        Self {
            store: Arc::new(AccountScopedStore::new(self.store.clone(), account_id)),
            access: self.access.clone(),
        }
    }

    async fn ensure_read(&self, auth: &AuthCtx, namespace: &str) -> Result<()> {
        if !auth.has_scope(&Scope::Read) {
            return Err(HubError::InsufficientScope);
        }
        if let Some(access) = &self.access {
            if !access.can_read(auth, namespace).await? {
                return Err(HubError::InsufficientScope);
            }
        }
        Ok(())
    }

    async fn ensure_write(&self, auth: &AuthCtx, namespace: &str) -> Result<()> {
        if !auth.has_scope(&Scope::Write) {
            return Err(HubError::InsufficientScope);
        }
        if let Some(access) = &self.access {
            if !access.can_write(auth, namespace).await? {
                return Err(HubError::InsufficientScope);
            }
        }
        Ok(())
    }

    /// 启动新任务并持久化初始任务事实
    pub async fn start_task(
        &self,
        auth: &AuthCtx,
        namespace: &str,
        task: &str,
        scene: Option<MemoryScene>,
        metadata: Option<serde_json::Value>,
    ) -> Result<MemoryTask> {
        self.start_task_with_evidence(auth, namespace, task, scene, None, metadata)
            .await
    }

    /// 启动新任务并关联前置证据溯源
    pub async fn start_task_with_evidence(
        &self,
        auth: &AuthCtx,
        namespace: &str,
        task: &str,
        scene: Option<MemoryScene>,
        source_evidence_id: Option<Uuid>,
        metadata: Option<serde_json::Value>,
    ) -> Result<MemoryTask> {
        self.ensure_write(auth, namespace).await?;
        let task_trimmed = task.trim();
        if task_trimmed.is_empty() {
            return Err(HubError::InvalidArgument("任务目标描述不能为空".into()));
        }

        let new_task = NewMemoryTask {
            id: None,
            namespace: namespace.to_string(),
            scene: scene.unwrap_or(MemoryScene::Working),
            task: task_trimmed.to_string(),
            status: TaskStatus::Running,
            started_at: Utc::now(),
            finished_at: None,
            result_summary: None,
            failure_reason: None,
            source_evidence_id,
            metadata: metadata.unwrap_or_else(|| serde_json::json!({})),
        };

        self.store.create_task(&new_task).await
    }

    /// 记录任务检查点摘要或阶段性状态
    pub async fn checkpoint_task(
        &self,
        auth: &AuthCtx,
        task_id: Uuid,
        namespace: &str,
        checkpoint_summary: &str,
        metadata: Option<serde_json::Value>,
    ) -> Result<MemoryTask> {
        self.ensure_write(auth, namespace).await?;
        let mut task = self
            .store
            .get_task(namespace, task_id)
            .await?
            .ok_or(HubError::NotFound)?;

        task.result_summary = Some(checkpoint_summary.to_string());
        if let Some(m) = metadata {
            task.metadata = m;
        }

        self.store.update_task(&task).await
    }

    /// 结束任务（支持幂等重入）
    #[allow(clippy::too_many_arguments)]
    pub async fn finish_task(
        &self,
        auth: &AuthCtx,
        task_id: Uuid,
        namespace: &str,
        status: TaskStatus,
        result_summary: Option<String>,
        failure_reason: Option<String>,
        source_evidence_id: Option<Uuid>,
    ) -> Result<MemoryTask> {
        self.ensure_write(auth, namespace).await?;
        let mut task = self
            .store
            .get_task(namespace, task_id)
            .await?
            .ok_or(HubError::NotFound)?;

        // 幂等性：如果任务已经处于终态（非 Running），直接返回已有记录，杜绝状态覆写破坏
        if task.status != TaskStatus::Running {
            return Ok(task);
        }

        task.status = status;
        task.finished_at = Some(Utc::now());
        if let Some(res) = result_summary {
            task.result_summary = Some(res);
        }
        if let Some(err) = failure_reason {
            task.failure_reason = Some(err);
        }
        if source_evidence_id.is_some() {
            task.source_evidence_id = source_evidence_id;
        }

        self.store.update_task(&task).await
    }

    /// 提炼并保存经验记忆（执行严格价值门槛校验）
    pub async fn distill_experience(
        &self,
        auth: &AuthCtx,
        namespace: &str,
        candidate: ExperienceCandidate,
    ) -> Result<MemoryExperience> {
        self.ensure_write(auth, namespace).await?;

        // 门槛 1：质量与安全门槛校验（包含敏感词与密钥扫描、可复用性、验证结果）
        validate_experience_candidate(&candidate)?;

        // 门槛 2：确认源任务存在且属于当前命名空间
        let source_task = self
            .store
            .get_task(namespace, candidate.source_task_id)
            .await?
            .ok_or_else(|| HubError::InvalidArgument("源任务不存在或无权访问".into()))?;

        // 查重：若命名空间内已存在相同任务模式和解决方案的活跃经验，幂等返回
        // （分页扫描全量，避免"只看最近 N 条"导致重复经验被反复写入）
        let existing = self.scan_experiences(namespace).await?;
        if let Some(found) = existing.into_iter().find(|e| {
            e.task.trim() == candidate.task_pattern.trim()
                && e.solution.trim() == candidate.solution.trim()
        }) {
            // 补充关联当前源任务，并刷新来源计数（验证门槛依赖它）
            let link = MemoryTaskExperience {
                namespace: Some(namespace.to_string()),
                task_id: source_task.id,
                experience_id: found.id,
                relation_kind: "reused".to_string(),
                created_at: Utc::now(),
            };
            self.store.link_task_experience(&link).await?;
            return self.refresh_source_count(namespace, &found).await;
        }

        let new_exp = NewMemoryExperience {
            id: None,
            namespace: namespace.to_string(),
            task: candidate.task_pattern.clone(),
            search_keys: candidate.search_keys.clone(),
            solution: candidate.solution.clone(),
            preconditions: candidate.preconditions.clone(),
            boundary_envs: candidate.boundary_envs.clone(),
            failure_modes: candidate.failure_modes.clone(),
            validation: candidate.validation.clone(),
            confidence: candidate.confidence,
            // 任务书 4.3：自动提炼只能产出候选，绝不能直接写“已验证”。
            // 验证必须走显式 `transition(Verify)` 并满足来源与验证条件。
            status: ExperienceStatus::Candidate,
            feedback_score: 1.0,
        };

        let saved = self.store.save_experience(&new_exp).await?;

        // 建立任务与经验的双向关联映射
        let link = MemoryTaskExperience {
            namespace: Some(namespace.to_string()),
            task_id: source_task.id,
            experience_id: saved.id,
            relation_kind: "derived".to_string(),
            created_at: Utc::now(),
        };
        self.store.link_task_experience(&link).await?;

        // 记录初始版本快照（只追加，供后续版本历史对照）
        self.append_snapshot(&saved, ExperienceChangeKind::Created, &auth.agent_id, None)
            .await?;

        // source_count 是派生元数据，不参与版本推进
        self.refresh_source_count(namespace, &saved).await
    }

    /// 重算并写回来源任务数（distinct）。
    ///
    /// `source_count` 只用于验证门槛与展示，不是经验内容，**不推进 version**，
    /// 否则每次关联来源都会制造一个无意义的版本。
    async fn refresh_source_count(
        &self,
        namespace: &str,
        experience: &MemoryExperience,
    ) -> Result<MemoryExperience> {
        let links = self
            .store
            .list_links_for_experience(namespace, experience.id)
            .await?;
        let distinct: std::collections::BTreeSet<Uuid> =
            links.into_iter().map(|l| l.task_id).collect();
        let count = distinct.len() as i64;
        if count == experience.source_count {
            return Ok(experience.clone());
        }
        let mut updated = experience.clone();
        updated.source_count = count;
        self.store
            .update_experience_if_version(&updated, updated.version)
            .await
    }

    /// 追加某个版本的快照（幂等：同一 version 只保留第一条）
    ///
    /// 语义：**每个 version 恰好一行**，`change_kind` 描述该版本因何产生。
    /// 因此创建时写 v1/Created，流转到 v2 后写 v2/Verified，依此类推。
    async fn append_snapshot(
        &self,
        before: &MemoryExperience,
        kind: ExperienceChangeKind,
        actor: &str,
        note: Option<&str>,
    ) -> Result<ExperienceVersion> {
        let snapshot = serde_json::to_value(before)
            .map_err(|e| HubError::Internal(anyhow::anyhow!("经验版本快照序列化失败: {}", e)))?;
        self.store
            .append_experience_version(&NewExperienceVersion {
                id: None,
                experience_id: before.id,
                namespace: before.namespace.clone(),
                version: before.version,
                snapshot,
                change_kind: kind,
                changed_by: actor.to_string(),
                status: before.status,
                note: note.map(str::to_string),
                changed_at: Utc::now(),
            })
            .await
    }

    /// 读取经验版本历史（新版本在前）
    pub async fn list_versions(
        &self,
        auth: &AuthCtx,
        namespace: &str,
        experience_id: Uuid,
        limit: usize,
    ) -> Result<Vec<ExperienceVersion>> {
        self.ensure_read(auth, namespace).await?;
        // 不存在或不属于该命名空间一律 NotFound，避免跨租户探测
        self.store
            .get_experience(namespace, experience_id)
            .await?
            .ok_or(HubError::NotFound)?;
        self.store
            .list_experience_versions(namespace, experience_id, limit)
            .await
    }

    /// 经验资产状态流转（任务书 4.3：状态转换必须有明确条件）
    ///
    /// - `Verify`：仅 `candidate → active`，且满足 `min_sources` 个**不同**的已完成
    ///   来源任务、关联任务无失败、`validation` 已声明；
    /// - `Reject`：需给出理由，任意未归档状态 → `rejected`；
    /// - `Deprecate`：需给出理由，`candidate/active → deprecated`；
    /// - `Restore`：`rejected/deprecated/archived → candidate`，历史保留。
    ///
    /// 所有流转都先写入旧版本快照再推进版本号；并发写用乐观锁，失败返回 `Conflict`。
    #[allow(clippy::too_many_arguments)]
    pub async fn transition(
        &self,
        auth: &AuthCtx,
        namespace: &str,
        experience_id: Uuid,
        action: ExperienceTransition,
        reason: Option<&str>,
        min_sources: usize,
    ) -> Result<MemoryExperience> {
        self.ensure_write(auth, namespace).await?;
        let current = self
            .store
            .get_experience(namespace, experience_id)
            .await?
            .ok_or(HubError::NotFound)?;

        let legal = match action {
            ExperienceTransition::Verify => current.status == ExperienceStatus::Candidate,
            ExperienceTransition::Reject => matches!(
                current.status,
                ExperienceStatus::Candidate
                    | ExperienceStatus::Active
                    | ExperienceStatus::Deprecated
            ),
            ExperienceTransition::Deprecate => current.status == ExperienceStatus::Active,
            ExperienceTransition::Restore => matches!(
                current.status,
                ExperienceStatus::Rejected
                    | ExperienceStatus::Deprecated
                    | ExperienceStatus::Archived
            ),
        };
        if !legal {
            return Err(HubError::InvalidArgument(format!(
                "不允许从状态 {} 执行 {} 流转",
                current.status.as_str(),
                action.as_str()
            )));
        }

        let reason_trimmed = reason.map(str::trim).filter(|r| !r.is_empty());
        if matches!(
            action,
            ExperienceTransition::Reject | ExperienceTransition::Deprecate
        ) && reason_trimmed.is_none()
        {
            return Err(HubError::InvalidArgument(format!(
                "{} 必须给出理由",
                action.as_str()
            )));
        }

        let source_count = if action == ExperienceTransition::Verify {
            self.ensure_verifiable(namespace, &current, min_sources.max(1))
                .await?
        } else {
            current.source_count
        };

        let prev_version = current.version;
        let change_kind = match action {
            ExperienceTransition::Verify => ExperienceChangeKind::Verified,
            ExperienceTransition::Reject => ExperienceChangeKind::Rejected,
            ExperienceTransition::Deprecate => ExperienceChangeKind::Deprecated,
            ExperienceTransition::Restore => ExperienceChangeKind::Restored,
        };

        let mut updated = current.clone();
        updated.status = action.target_status();
        updated.status_reason = reason_trimmed.map(str::to_string);
        updated.source_count = source_count;
        updated.version = prev_version + 1;
        updated.updated_at = Utc::now();
        match action {
            ExperienceTransition::Verify => {
                updated.verified_at = Some(Utc::now());
                updated.verified_by = Some(auth.agent_id.clone());
                updated.verification_note = reason_trimmed.map(str::to_string);
            }
            ExperienceTransition::Reject | ExperienceTransition::Deprecate => {
                // 当前状态的验证元数据清空；历史仍可在版本快照中追溯
                updated.verified_at = None;
                updated.verified_by = None;
                updated.verification_note = None;
            }
            ExperienceTransition::Restore => {
                updated.verified_at = None;
                updated.verified_by = None;
                updated.verification_note = None;
            }
        }

        let saved = self
            .store
            .update_experience_if_version(&updated, prev_version)
            .await?;

        // 版本快照语义：**每个 version 恰好一行**，change_kind 记录该版本
        // 因何产生。先更新再写快照，避免与已存在的上一版本快照撞唯一键
        // （先写旧版本会被创建时的快照吞掉，导致流转动作在历史里不可见）。
        self.append_snapshot(&saved, change_kind, &auth.agent_id, reason_trimmed)
            .await?;

        Ok(saved)
    }

    /// 验证条件校验：返回去重后的来源任务数
    async fn ensure_verifiable(
        &self,
        namespace: &str,
        experience: &MemoryExperience,
        min_sources: usize,
    ) -> Result<i64> {
        let links = self
            .store
            .list_links_for_experience(namespace, experience.id)
            .await?;
        let distinct: std::collections::BTreeSet<Uuid> =
            links.into_iter().map(|l| l.task_id).collect();

        let mut completed = 0usize;
        for task_id in &distinct {
            let task = self
                .store
                .get_task(namespace, *task_id)
                .await?
                .ok_or_else(|| {
                    HubError::InvalidArgument(format!("来源任务 {} 不存在，无法验证经验", task_id))
                })?;
            match task.status {
                TaskStatus::Completed => completed += 1,
                TaskStatus::Failed => {
                    return Err(HubError::InvalidArgument(format!(
                        "来源任务 {} 失败，不能据此验证经验",
                        task_id
                    )));
                }
                TaskStatus::Running | TaskStatus::Cancelled => {}
            }
        }

        if completed < min_sources {
            return Err(HubError::InvalidArgument(format!(
                "验证至少需要 {} 个不同的已完成来源任务，当前为 {}",
                min_sources, completed
            )));
        }

        if !validation_declared(&experience.validation) {
            return Err(HubError::InvalidArgument(
                "验证方法未声明（validation 为空），不能升级为已验证经验".into(),
            ));
        }

        Ok(distinct.len() as i64)
    }

    /// 记录经验采用反馈（闭环迭代机制，负反馈降低权重，不硬删除）
    ///
    /// 任务书 4.3：**单次成功不能把候选提升为已验证**，因此正反馈只加
    /// `feedback_score`，不改变状态。负反馈把分数压到阈值以下时标记
    /// `deprecated`，并写入版本快照（状态变化必须可追溯）。
    pub async fn record_feedback(
        &self,
        auth: &AuthCtx,
        namespace: &str,
        experience_id: Uuid,
        is_success: bool,
        notes: Option<String>,
    ) -> Result<MemoryExperience> {
        self.ensure_write(auth, namespace).await?;
        let current = self
            .store
            .get_experience(namespace, experience_id)
            .await?
            .ok_or(HubError::NotFound)?;

        if matches!(
            current.status,
            ExperienceStatus::Rejected | ExperienceStatus::Archived
        ) {
            return Err(HubError::InvalidArgument(format!(
                "状态为 {} 的经验不接受反馈",
                current.status.as_str()
            )));
        }

        let mut updated = current.clone();
        if is_success {
            updated.feedback_score = (updated.feedback_score + 0.1).clamp(0.0, 1.0);
        } else {
            updated.feedback_score = (updated.feedback_score - 0.25).clamp(0.0, 1.0);
        }

        let prev_version = current.version;
        let becomes_deprecated = !is_success
            && updated.feedback_score < 0.3
            && updated.status != ExperienceStatus::Deprecated;
        let note =
            if becomes_deprecated {
                Some(notes.unwrap_or_else(|| {
                    format!("负反馈累计使反馈分降至 {:.2}", updated.feedback_score)
                }))
            } else {
                None
            };

        if becomes_deprecated {
            updated.status = ExperienceStatus::Deprecated;
            updated.status_reason = note.clone();
            updated.version = prev_version + 1;
            updated.verified_at = None;
            updated.verified_by = None;
            updated.verification_note = None;
        }

        let saved = self
            .store
            .update_experience_if_version(&updated, prev_version)
            .await?;

        if becomes_deprecated {
            // 状态变化必须留下对应版本的快照（与 transition 同一语义）
            self.append_snapshot(
                &saved,
                ExperienceChangeKind::Deprecated,
                &auth.agent_id,
                note.as_deref(),
            )
            .await?;
        }

        Ok(saved)
    }

    /// 经验检索单页大小与单次扫描上限
    const EXPERIENCE_PAGE: usize = 200;
    const EXPERIENCE_SCAN_CAP: usize = 2_000;

    /// 分页扫描命名空间内的经验（替代"只取最近 100 条"的硬上限）
    async fn scan_experiences(&self, namespace: &str) -> Result<Vec<MemoryExperience>> {
        let mut pool: Vec<MemoryExperience> = Vec::new();
        let mut offset = 0usize;
        loop {
            let page = self
                .store
                .list_experiences(namespace, None, Self::EXPERIENCE_PAGE, offset)
                .await?;
            let fetched = page.len();
            pool.extend(page);
            if fetched < Self::EXPERIENCE_PAGE || pool.len() >= Self::EXPERIENCE_SCAN_CAP {
                break;
            }
            offset += fetched;
        }
        pool.truncate(Self::EXPERIENCE_SCAN_CAP);
        Ok(pool)
    }

    /// 经验的可检索文本：任务模式 + 方案 + 检索别名 + 已知失败模式
    fn searchable_text(exp: &MemoryExperience) -> String {
        let mut text = format!("{} {}", exp.task, exp.solution);
        if !exp.search_keys.is_empty() {
            text.push(' ');
            text.push_str(&exp.search_keys.join(" "));
        }
        if !exp.failure_modes.is_empty() {
            text.push(' ');
            text.push_str(&exp.failure_modes.join(" "));
        }
        text
    }

    /// 经验文档词元缓存上限（条目数）：经验池本身被 `EXPERIENCE_SCAN_CAP`
    /// 限制在 2000，缓存上限略高于它；超限整体清空（偶发重算可接受）
    const EXPERIENCE_TOKEN_CACHE_CAP: usize = 4096;

    /// 进程内词元缓存：key = (namespace, experience_id)，value = (正文指纹, 词元集)。
    ///
    /// `search_experiences` 每次查询都对候选池**全量重新分词**（CJK 1/2-gram），
    /// 同一条经验在多次查询间被反复切分——这是经验检索在池增大后的主要
    /// CPU 开销。分词是纯函数，按 (命名空间, id) 缓存即可跨查询复用。
    ///
    /// 命中条件带 `searchable_text` 指纹：经验正文理论上可经
    /// `update_experience` 变更（当前只有状态/反馈会走它），指纹比对保证
    /// 任何字段变化都会自然失效，不依赖"内容不可变"的约定。
    fn document_tokens_cached(exp: &MemoryExperience) -> Arc<std::collections::HashSet<String>> {
        use std::collections::HashMap;
        use std::hash::{Hash, Hasher};
        use std::sync::{Arc, Mutex, OnceLock};

        type TokenCache = HashMap<(String, Uuid), (u64, Arc<std::collections::HashSet<String>>)>;
        static CACHE: OnceLock<Mutex<TokenCache>> = OnceLock::new();
        fn cache() -> &'static Mutex<TokenCache> {
            CACHE.get_or_init(|| Mutex::new(HashMap::new()))
        }
        fn lock<T>(guard: std::result::Result<T, std::sync::PoisonError<T>>) -> T {
            guard.unwrap_or_else(std::sync::PoisonError::into_inner)
        }

        let text = Self::searchable_text(exp);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hasher);
        let fingerprint = hasher.finish();
        let key = (exp.namespace.clone(), exp.id);

        {
            let map = lock(cache().lock());
            if let Some((fp, tokens)) = map.get(&key) {
                if *fp == fingerprint {
                    return tokens.clone();
                }
            }
        }

        let tokens = Arc::new(crate::algorithm::document_tokens(&text));
        let mut map = lock(cache().lock());
        if map.len() >= Self::EXPERIENCE_TOKEN_CACHE_CAP {
            map.clear();
        }
        map.insert(key, (fingerprint, tokens.clone()));
        tokens
    }

    /// 检索可复用经验（与 memory_search 同源的词法打分 + 环境边界匹配）
    ///
    /// - 命中判定基于分词词元集合与池内 IDF 加权，不再用整段查询做裸子串匹配；
    /// - `env` 给出且与经验 `boundary_envs` 不匹配时降权 ×0.5 并给出显式告警，
    ///   但不吞没结果，交由 Agent 判别；
    /// - `strict_env` 为真时改为**拒绝**：环境不匹配的经验直接不返回。
    ///   降权只是排序提示，Agent 仍可能误取；而有安全后果的场景
    ///   （如把仅适用于 kafka 的处置步骤用在 jvm 上）需要的是硬规则，
    ///   不能只依赖排序。
    /// - 查询为空时退化为列表召回（按置信度 × 反馈分排序）。
    pub async fn search_experiences(
        &self,
        auth: &AuthCtx,
        namespace: &str,
        query: &str,
        current_env: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ExperienceSearchResult>> {
        self.search_experiences_with(auth, namespace, query, current_env, limit, false)
            .await
    }

    /// 同 [`Self::search_experiences`]，但可要求**严格**环境边界（不匹配即拒绝）
    pub async fn search_experiences_with(
        &self,
        auth: &AuthCtx,
        namespace: &str,
        query: &str,
        current_env: Option<&str>,
        limit: usize,
        strict_env: bool,
    ) -> Result<Vec<ExperienceSearchResult>> {
        self.search_experiences_governed(
            auth,
            namespace,
            query,
            ExperienceSearchOptions {
                current_env,
                limit,
                strict_env,
                include_statuses: None,
            },
        )
        .await
    }

    /// 治理版经验检索：支持显式限定状态集合
    ///
    /// 默认（`include_statuses = None`）排除 `rejected` / `archived`，
    /// 并按任务书 4.3.5 对未验证经验降权（active 1.0 / candidate 0.6 /
    /// deprecated 0.3）。显式传入状态集合时按集合过滤（可用于审计检索）。
    pub async fn search_experiences_governed(
        &self,
        auth: &AuthCtx,
        namespace: &str,
        query: &str,
        options: ExperienceSearchOptions<'_>,
    ) -> Result<Vec<ExperienceSearchResult>> {
        self.ensure_read(auth, namespace).await?;

        let pool = self.scan_experiences(namespace).await?;

        let terms = crate::algorithm::extract_terms(query);
        // 词元集走进程内缓存：同一条经验在多次查询间不必反复分词
        let docs: Vec<Arc<std::collections::HashSet<String>>> =
            pool.iter().map(Self::document_tokens_cached).collect();
        // 池内 IDF 打分器：与混合检索同一实现（mem_core::PoolLexical）
        let pool_lexical = crate::algorithm::PoolLexical::new(terms, &docs);

        let mut results = Vec::new();
        for (exp, tokens) in pool.into_iter().zip(docs) {
            let status_allowed = match options.include_statuses {
                Some(allowed) => allowed.contains(&exp.status),
                None => exp.status.searchable_by_default(),
            };
            if !status_allowed {
                continue;
            }

            let lexical = if pool_lexical.is_empty() {
                1.0
            } else {
                pool_lexical.score(&tokens)
            };
            if lexical <= 0.0 {
                continue;
            }

            // 置信度 × 反馈分 × 状态权重：反馈只调 feedback_score 一个因子，
            // 状态权重表达“验证程度”，两者语义不同、不得互相替代
            let mut score =
                lexical * exp.confidence * exp.feedback_score * exp.status.retrieval_weight();

            // 环境边界校验
            let mut env_mismatch = false;
            let mut boundary_warning = None;
            if let Some(env) = options.current_env {
                if !exp.boundary_envs.is_empty()
                    && !exp
                        .boundary_envs
                        .iter()
                        .any(|be| be.eq_ignore_ascii_case(env))
                {
                    env_mismatch = true;
                    boundary_warning = Some(format!(
                        "当前环境 [{}] 与经验适用环境边界 {:?} 不匹配，需谨慎参考",
                        env, exp.boundary_envs
                    ));
                    if options.strict_env {
                        // 严格模式：环境不匹配 = 不可用，直接拒绝。
                        // 降权只是排序提示，Agent 仍可能误取；有安全后果的场景
                        // 需要硬规则，而不是"排在后面"。
                        continue;
                    }
                    // 环境不匹配时适度降权，但不直接吞没，标明边界供 Agent 判别
                    score *= 0.5;
                }
            }

            results.push(ExperienceSearchResult {
                experience: exp,
                score,
                env_mismatch,
                boundary_warning,
            });
        }

        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(options.limit.max(1));
        Ok(results)
    }
}

/// 经验检索选项（治理版）
#[derive(Debug, Clone, Copy)]
pub struct ExperienceSearchOptions<'a> {
    pub current_env: Option<&'a str>,
    pub limit: usize,
    pub strict_env: bool,
    /// `None` = 默认可见状态（排除 rejected/archived）
    pub include_statuses: Option<&'a [ExperienceStatus]>,
}

/// `validation` 是否已声明（非 null / 非空对象 / 非空数组 / 非空字符串）
fn validation_declared(validation: &serde_json::Value) -> bool {
    match validation {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::String(s) => !s.trim().is_empty(),
        serde_json::Value::Array(a) => !a.is_empty(),
        serde_json::Value::Object(o) => !o.is_empty(),
        serde_json::Value::Number(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ExperienceCandidate, ExperienceStatus, MemoryScene, TaskStatus};
    use crate::testing::TestHubStore;
    use common::Scope;

    fn make_auth(agent: &str) -> AuthCtx {
        AuthCtx {
            agent_id: agent.to_string(),
            account_id: common::BOOTSTRAP_ACCOUNT_ID,
            scopes: vec![Scope::Read, Scope::Write],
            acl: Default::default(),
        }
    }

    #[tokio::test]
    async fn test_task_lifecycle_and_idempotent_finish() {
        let store = Arc::new(TestHubStore::default());
        let service = WorkingMemoryService::new(store);
        let auth = make_auth("worker-agent");

        // 1. 启动任务
        let task = service
            .start_task(
                &auth,
                "working-ns",
                "升级依赖至 Rust 2024",
                Some(MemoryScene::Working),
                None,
            )
            .await
            .unwrap();
        assert_eq!(task.status, TaskStatus::Running);
        assert!(task.finished_at.is_none());

        // 2. 检查点记录
        let checkpoint = service
            .checkpoint_task(
                &auth,
                task.id,
                "working-ns",
                "编译检查完成，准备跑全量测试",
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            checkpoint.result_summary.as_deref(),
            Some("编译检查完成，准备跑全量测试")
        );

        // 3. 第一次 finish 任务
        let finished1 = service
            .finish_task(
                &auth,
                task.id,
                "working-ns",
                TaskStatus::Completed,
                Some("所有 260 项测试通过".into()),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(finished1.status, TaskStatus::Completed);
        assert!(finished1.finished_at.is_some());

        // 4. 第二次重复 finish（幂等重入）
        let finished2 = service
            .finish_task(
                &auth,
                task.id,
                "working-ns",
                TaskStatus::Failed, // 试图覆写状态为 Failed
                Some("其他总结".into()),
                Some("错误原因".into()),
                None,
            )
            .await
            .unwrap();

        // 验证幂等保护：状态保持首次 finish 的 Completed，未被脏覆写
        assert_eq!(finished2.status, TaskStatus::Completed);
        assert_eq!(
            finished2.result_summary.as_deref(),
            Some("所有 260 项测试通过")
        );
    }

    #[tokio::test]
    async fn test_experience_quality_threshold_enforcement() {
        let store = Arc::new(TestHubStore::default());
        let service = WorkingMemoryService::new(store);
        let auth = make_auth("worker-agent");

        let task = service
            .start_task(&auth, "test-ns", "排查生产死锁", None, None)
            .await
            .unwrap();

        // 1. 拦截包含 API Key / 凭据的内容
        let leak_candidate = ExperienceCandidate {
            task_pattern: "修复连接池泄漏".into(),
            search_keys: vec!["pool".into()],
            solution: format!(
                "使用密钥 {} 连接测试服务器排查",
                concat!("sk-", "1234567890abcdefgz")
            ),
            preconditions: serde_json::json!({"env": "staging"}),
            boundary_envs: vec!["linux".into()],
            failure_modes: vec!["timeout".into()],
            validation: serde_json::json!({"test": "cargo test"}),
            confidence: 0.9,
            source_task_id: task.id,
        };
        let err1 = service
            .distill_experience(&auth, "test-ns", leak_candidate)
            .await
            .unwrap_err();
        assert!(err1.to_string().contains("敏感凭证"));

        // 2. 拦截纯局部单次任务描述
        let local_only_candidate = ExperienceCandidate {
            task_pattern: "本次任务我完成了几个接口的修改".into(),
            search_keys: vec!["local".into()],
            solution: "把 main.rs 里的端口改成了 8080".into(),
            preconditions: serde_json::json!({"env": "local"}),
            boundary_envs: vec!["macos".into()],
            failure_modes: vec![],
            validation: serde_json::json!({"test": "curl"}),
            confidence: 0.8,
            source_task_id: task.id,
        };
        let err2 = service
            .distill_experience(&auth, "test-ns", local_only_candidate)
            .await
            .unwrap_err();
        assert!(err2.to_string().contains("通用可复用性"));

        // 3. 拦截未经验证 / 验证说明为空的候选
        let unverified_candidate = ExperienceCandidate {
            task_pattern: "优化大规模批量插入性能".into(),
            search_keys: vec!["pg".into()],
            solution: "改用 COPY FROM STDIN 代替逐条 INSERT".into(),
            preconditions: serde_json::json!({"db": "pg"}),
            boundary_envs: vec!["postgres".into()],
            failure_modes: vec![],
            validation: serde_json::json!({}), // 空验证！
            confidence: 0.8,
            source_task_id: task.id,
        };
        let err3 = service
            .distill_experience(&auth, "test-ns", unverified_candidate)
            .await
            .unwrap_err();
        assert!(err3.to_string().contains("验证结果或验证方法说明"));

        // 4. 合法且通过门槛的经验成功落库并关联任务
        let valid_candidate = ExperienceCandidate {
            task_pattern: "优化大规模批量插入性能".into(),
            search_keys: vec!["postgres".into(), "batch-insert".into()],
            solution: "改用 UNNEST 或 COPY FROM 代替逐条 INSERT，吞吐提升 10 倍".into(),
            preconditions: serde_json::json!({"db": "postgres >= 14"}),
            boundary_envs: vec!["postgresql".into()],
            failure_modes: vec!["大事务造成复制延迟".into()],
            validation: serde_json::json!({"benchmark": "100k 条耗时从 4.2s 降至 0.38s"}),
            confidence: 0.92,
            source_task_id: task.id,
        };
        let exp = service
            .distill_experience(&auth, "test-ns", valid_candidate)
            .await
            .unwrap();
        assert_eq!(exp.status, ExperienceStatus::Candidate);
        assert_eq!(exp.feedback_score, 1.0);
    }

    #[tokio::test]
    async fn test_negative_feedback_lowers_confidence_without_deletion() {
        let store = Arc::new(TestHubStore::default());
        let service = WorkingMemoryService::new(store);
        let auth = make_auth("feedback-agent");

        let task = service
            .start_task(&auth, "fb-ns", "调优 HNSW 索引参数", None, None)
            .await
            .unwrap();

        let candidate = ExperienceCandidate {
            task_pattern: "调优 pgvector HNSW 检索精度与内存".into(),
            search_keys: vec!["hnsw".into(), "pgvector".into()],
            solution: "设置 m=16, ef_construction=64 可取得较佳性价比".into(),
            preconditions: serde_json::json!({"extension": "pgvector"}),
            boundary_envs: vec!["postgresql".into()],
            failure_modes: vec!["构建内存过大".into()],
            validation: serde_json::json!({"recall_at_10": 0.98}),
            confidence: 0.85,
            source_task_id: task.id,
        };

        let exp = service
            .distill_experience(&auth, "fb-ns", candidate)
            .await
            .unwrap();
        assert_eq!(exp.feedback_score, 1.0);

        // 记录一次负反馈（如在特定高维场景下召回率不佳）
        let updated1 = service
            .record_feedback(
                &auth,
                "fb-ns",
                exp.id,
                false,
                Some("在 1536 维场景下召回率只有 0.72".into()),
            )
            .await
            .unwrap();

        // 验证：反馈只降低反馈分（单一因子），提炼时的置信度不变；实体未被删除
        assert!(updated1.feedback_score < 1.0);
        assert_eq!(
            updated1.confidence, 0.85,
            "反馈不得再同时下调置信度（会被打分区相乘放大成双重惩罚）"
        );
        assert_eq!(
            updated1.status,
            ExperienceStatus::Candidate,
            "单次负反馈只降分，不改状态"
        );

        // 再次多次负反馈触发降级 Deprecated
        let _updated2 = service
            .record_feedback(&auth, "fb-ns", exp.id, false, None)
            .await
            .unwrap();
        let updated3 = service
            .record_feedback(&auth, "fb-ns", exp.id, false, None)
            .await
            .unwrap();
        assert_eq!(updated3.status, ExperienceStatus::Deprecated);

        // 经验依然可查，保证历史溯源完整
        let list = service
            .search_experiences(&auth, "fb-ns", "HNSW", None, 10)
            .await
            .unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].experience.id, exp.id);
    }

    #[tokio::test]
    async fn test_environment_boundary_awareness_in_search() {
        let store = Arc::new(TestHubStore::default());
        let service = WorkingMemoryService::new(store);
        let auth = make_auth("search-agent");

        let task = service
            .start_task(&auth, "env-ns", "配置 jemalloc 内存分配器", None, None)
            .await
            .unwrap();

        let candidate = ExperienceCandidate {
            task_pattern: "为 Rust 服务启用 jemalloc 减少内存碎片".into(),
            search_keys: vec!["jemalloc".into(), "memory".into()],
            solution: "在 main.rs 中配置 #[global_allocator] tikv_jemallocator".into(),
            preconditions: serde_json::json!({"os": "linux"}),
            boundary_envs: vec!["linux".into()],
            failure_modes: vec!["在带有空格路径的 macOS 环境下编译构建失败".into()],
            validation: serde_json::json!({"rss": "减少 30% 常驻内存"}),
            confidence: 0.9,
            source_task_id: task.id,
        };

        service
            .distill_experience(&auth, "env-ns", candidate)
            .await
            .unwrap();

        // 场景 A：在匹配的环境 (linux) 下搜索
        let hits_linux = service
            .search_experiences(&auth, "env-ns", "jemalloc", Some("linux"), 5)
            .await
            .unwrap();
        assert_eq!(hits_linux.len(), 1);
        assert!(!hits_linux[0].env_mismatch);
        assert!(hits_linux[0].boundary_warning.is_none());

        // 场景 B：在不匹配的环境 (macos) 下搜索
        let hits_macos = service
            .search_experiences(&auth, "env-ns", "jemalloc", Some("macos"), 5)
            .await
            .unwrap();
        assert_eq!(hits_macos.len(), 1);
        assert!(hits_macos[0].env_mismatch, "不匹配环境必须标明 mismatch");
        assert!(hits_macos[0]
            .boundary_warning
            .as_ref()
            .unwrap()
            .contains("不匹配"));
        assert!(hits_macos[0].score < hits_linux[0].score, "不匹配环境降权");
    }

    /// 严格环境边界：环境不匹配的经验**直接不返回**
    ///
    /// 降权只是排序提示——Agent 仍可能误取；而有安全后果的场景
    /// （把只适用于 linux 的处置步骤用在 macOS 上）需要的是硬规则。
    #[tokio::test]
    async fn test_strict_env_rejects_mismatched_experience() {
        let store = Arc::new(TestHubStore::default());
        let service = WorkingMemoryService::new(store);
        let auth = make_auth("strict-env-agent");

        let task = service
            .start_task(&auth, "strict-ns", "配置 jemalloc 内存分配器", None, None)
            .await
            .unwrap();
        let candidate = ExperienceCandidate {
            task_pattern: "为 Rust 服务启用 jemalloc 减少内存碎片".into(),
            search_keys: vec!["jemalloc".into(), "memory".into()],
            solution: "在 main.rs 中配置 #[global_allocator] tikv_jemallocator".into(),
            preconditions: serde_json::json!({"os": "linux"}),
            boundary_envs: vec!["linux".into()],
            failure_modes: vec!["在带有空格路径的 macOS 环境下编译构建失败".into()],
            validation: serde_json::json!({"rss": "减少 30% 常驻内存"}),
            confidence: 0.9,
            source_task_id: task.id,
        };
        service
            .distill_experience(&auth, "strict-ns", candidate)
            .await
            .unwrap();

        // 非严格模式：仍然返回，但标明 mismatch 并降权
        let lenient = service
            .search_experiences(&auth, "strict-ns", "jemalloc", Some("macos"), 5)
            .await
            .unwrap();
        assert_eq!(lenient.len(), 1);
        assert!(lenient[0].env_mismatch);

        // 严格模式：环境不匹配 → 硬拒绝，一条都不返回
        let strict = service
            .search_experiences_with(&auth, "strict-ns", "jemalloc", Some("macos"), 5, true)
            .await
            .unwrap();
        assert!(
            strict.is_empty(),
            "严格环境边界下，环境不匹配的经验必须被拒绝：{strict:?}"
        );

        // 环境匹配时不受影响
        let strict_ok = service
            .search_experiences_with(&auth, "strict-ns", "jemalloc", Some("linux"), 5, true)
            .await
            .unwrap();
        assert_eq!(strict_ok.len(), 1);
        assert!(!strict_ok[0].env_mismatch);
    }

    #[tokio::test]
    async fn test_cross_namespace_isolation_is_fail_closed() {
        let store = Arc::new(TestHubStore::default());
        let service = WorkingMemoryService::new(store);
        let auth = make_auth("agent-ns");

        let task_a = service
            .start_task(&auth, "ns-a", "私有任务 A", None, None)
            .await
            .unwrap();

        // 尝试使用 ns-b 访问或操作 ns-a 的 task，直接返回 NotFound 或拒绝
        let fetch = service
            .checkpoint_task(&auth, task_a.id, "ns-b", "越权探测", None)
            .await;
        assert!(matches!(fetch, Err(HubError::NotFound)));
    }

    #[tokio::test]
    async fn test_experience_search_is_tokenized_and_env_aware() {
        let store = Arc::new(TestHubStore::default());
        let service = WorkingMemoryService::new(store.clone());
        let auth = make_auth("exp-search-agent");

        let linux_task = service
            .start_task(&auth, "exp-ns", "排查生产环境内存泄漏", None, None)
            .await
            .unwrap();
        service
            .distill_experience(
                &auth,
                "exp-ns",
                ExperienceCandidate {
                    task_pattern: "排查生产环境内存泄漏".into(),
                    search_keys: vec!["heaptrack".into()],
                    solution: "用 heaptrack 抓取堆栈定位泄漏点后修复".into(),
                    preconditions: serde_json::json!({"os": "linux"}),
                    boundary_envs: vec!["linux".into()],
                    failure_modes: vec!["未安装调试符号导致堆栈缺失".into()],
                    validation: serde_json::json!({"ok": true}),
                    confidence: 1.0,
                    source_task_id: linux_task.id,
                },
            )
            .await
            .unwrap();

        let mac_task = service
            .start_task(&auth, "exp-ns", "配置 jemalloc 内存分配器", None, None)
            .await
            .unwrap();
        service
            .distill_experience(
                &auth,
                "exp-ns",
                ExperienceCandidate {
                    task_pattern: "配置 jemalloc 内存分配器".into(),
                    search_keys: vec!["jemalloc".into()],
                    solution: "设置 #[global_allocator] 指向 tikv_jemallocator".into(),
                    preconditions: serde_json::json!({"os": "macos"}),
                    boundary_envs: vec!["macos".into()],
                    failure_modes: vec![],
                    validation: serde_json::json!({"ok": true}),
                    confidence: 1.0,
                    source_task_id: mac_task.id,
                },
            )
            .await
            .unwrap();

        // 回归：查询不是正文的整段子串（"内存泄漏排查" ∉ "排查生产环境内存泄漏"），
        // 旧的裸子串匹配会整条漏召回；分词部分匹配必须命中
        let hits = service
            .search_experiences(&auth, "exp-ns", "内存泄漏排查", Some("linux"), 10)
            .await
            .unwrap();
        assert!(!hits.is_empty(), "分词部分匹配必须召回经验");
        assert!(
            hits[0].experience.task.contains("内存泄漏"),
            "词元命中更多的经验应排在首位：{:#?}",
            hits.iter()
                .map(|h| (&h.experience.task, h.score))
                .collect::<Vec<_>>()
        );
        assert!(!hits[0].env_mismatch, "环境匹配时不应标记不匹配");

        // 环境不匹配：显式告警并降权，但不吞没结果
        let mismatched = service
            .search_experiences(&auth, "exp-ns", "内存泄漏排查", Some("windows"), 10)
            .await
            .unwrap();
        let linux_hit = mismatched
            .iter()
            .find(|h| h.experience.task.contains("内存泄漏"))
            .expect("不匹配时仍应保留条目");
        assert!(linux_hit.env_mismatch);
        assert!(linux_hit
            .boundary_warning
            .as_deref()
            .unwrap_or_default()
            .contains("windows"));
        assert!(linux_hit.score < hits[0].score, "环境边界不匹配必须降权");

        // 无关查询：无词元命中即不返回
        let unrelated = service
            .search_experiences(&auth, "exp-ns", "量子纠缠波长", None, 10)
            .await
            .unwrap();
        assert!(
            unrelated.is_empty(),
            "无词元命中不应返回结果：{unrelated:#?}"
        );
    }

    #[tokio::test]
    async fn test_experience_search_scans_all_pages_not_only_recent() {
        // 回归：检索曾硬编码 list_experiences(..., 100)，只看最近 100 条
        use crate::infra::ExperienceStore as _;
        let store = Arc::new(TestHubStore::default());
        let service = WorkingMemoryService::new(store.clone());
        let auth = make_auth("exp-page-agent");

        for i in 0..205 {
            let exp = NewMemoryExperience {
                id: None,
                namespace: "page-ns".to_string(),
                task: format!("历史经验 {}", i),
                search_keys: vec![format!("key-{}", i)],
                solution: format!("解决方案 {}", i),
                preconditions: serde_json::json!({}),
                boundary_envs: vec![],
                failure_modes: vec![],
                validation: serde_json::json!({}),
                confidence: 1.0,
                status: ExperienceStatus::Active,
                feedback_score: 1.0,
            };
            store.save_experience(&exp).await.unwrap();
        }

        // 空查询 → 列表召回；分页扫描必须覆盖第一页之外的条目
        let all = service
            .search_experiences(&auth, "page-ns", "", None, 1_000)
            .await
            .unwrap();
        assert_eq!(
            all.len(),
            205,
            "检索必须分页扫描全量经验，而不是只看最近 100 条"
        );
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use crate::domain::ExperienceStatus;

    /// 微基准（手动运行）：经验检索的文档分词成本——冷（逐条分词，即缓存前
    /// 每次查询的成本）vs 热（缓存命中）。
    /// ```bash
    /// cargo test -p mem-core --lib working_memory_service::bench -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "微基准：手动运行"]
    fn test_experience_tokenize_benchmark() {
        let exps: Vec<MemoryExperience> = (0..2_000usize)
            .map(|i| MemoryExperience {
                id: Uuid::from_u128(i as u128),
                namespace: "bench-ns".to_string(),
                task: format!("排查生产环境第 {i} 类连接池泄漏并优化最大空闲参数"),
                search_keys: vec!["redis".into(), "pool".into(), format!("key-{i}")],
                solution: "调大连接池最大存活时间并配合合理的最大空闲连接数，定期健康探测".into(),
                preconditions: serde_json::json!({}),
                boundary_envs: vec!["linux".into()],
                failure_modes: vec!["空闲连接数设为 0 导致每次重新握手".into()],
                validation: serde_json::json!({"benchmark": "500 并发压测连接复用正常"}),
                confidence: 0.9,
                status: ExperienceStatus::Active,
                feedback_score: 1.0,
                version: 1,
                superseded_by: None,
                verified_at: None,
                verified_by: None,
                verification_note: None,
                status_reason: None,
                source_count: 0,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .collect();

        // 冷：逐条分词（缓存前每次查询都要付出的成本）
        let t0 = std::time::Instant::now();
        let cold: Vec<std::collections::HashSet<String>> = exps
            .iter()
            .map(|e| crate::algorithm::document_tokens(&WorkingMemoryService::searchable_text(e)))
            .collect();
        let cold_time = t0.elapsed();
        assert_eq!(cold.len(), exps.len());

        // 首遍走缓存（未命中 → 分词并写缓存）
        let t1 = std::time::Instant::now();
        let _warm1: Vec<_> = exps
            .iter()
            .map(WorkingMemoryService::document_tokens_cached)
            .collect();
        let warm1_time = t1.elapsed();

        // 热：缓存全命中——这是"缓存后每次查询"的成本
        let t2 = std::time::Instant::now();
        let warm2: Vec<_> = exps
            .iter()
            .map(WorkingMemoryService::document_tokens_cached)
            .collect();
        let warm2_time = t2.elapsed();
        assert_eq!(warm2.len(), exps.len());

        println!(
            "2000 条经验分词：冷 {cold_time:?}（缓存前每次查询）| 首遍写缓存 {warm1_time:?} | 缓存命中 {warm2_time:?}（缓存后每次查询）"
        );
    }
}
