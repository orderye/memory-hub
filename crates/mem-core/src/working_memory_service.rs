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
    validate_experience_candidate, ExperienceCandidate, ExperienceStatus, MemoryExperience,
    MemoryScene, MemoryTask, MemoryTaskExperience, NewMemoryExperience, NewMemoryTask, TaskStatus,
};
use crate::infra::MemoryHubStore;
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
}

impl WorkingMemoryService {
    pub fn new(store: Arc<dyn MemoryHubStore>) -> Self {
        Self { store }
    }

    /// 切换底层作用域存储（账户沙箱化）
    pub fn scoped(&self, account_id: AccountId) -> Self {
        Self {
            store: Arc::new(AccountScopedStore::new(self.store.clone(), account_id)),
        }
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
        if !auth.has_scope(&Scope::Write) {
            return Err(HubError::InsufficientScope);
        }
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
        if !auth.has_scope(&Scope::Write) {
            return Err(HubError::InsufficientScope);
        }
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
        if !auth.has_scope(&Scope::Write) {
            return Err(HubError::InsufficientScope);
        }
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
        if !auth.has_scope(&Scope::Write) {
            return Err(HubError::InsufficientScope);
        }

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
            // 补充关联当前源任务
            let link = MemoryTaskExperience {
                namespace: Some(namespace.to_string()),
                task_id: source_task.id,
                experience_id: found.id,
                relation_kind: "reused".to_string(),
                created_at: Utc::now(),
            };
            let _ = self.store.link_task_experience(&link).await;
            return Ok(found);
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
            status: ExperienceStatus::Active,
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

        Ok(saved)
    }

    /// 记录经验采用反馈（闭环迭代机制，负反馈降低权重，不硬删除）
    pub async fn record_feedback(
        &self,
        auth: &AuthCtx,
        namespace: &str,
        experience_id: Uuid,
        is_success: bool,
        _notes: Option<String>,
    ) -> Result<MemoryExperience> {
        if !auth.has_scope(&Scope::Write) {
            return Err(HubError::InsufficientScope);
        }
        let mut exp = self
            .store
            .get_experience(namespace, experience_id)
            .await?
            .ok_or(HubError::NotFound)?;

        // 反馈只调整 feedback_score 一个因子：置信度反映"提炼时的可信度"，
        // 此前两者同时被调整、又在打分区相乘，一次负反馈被放大成两次惩罚
        if is_success {
            exp.feedback_score = (exp.feedback_score + 0.1).clamp(0.0, 1.0);
            if exp.status == ExperienceStatus::Candidate {
                exp.status = ExperienceStatus::Active;
            }
        } else {
            exp.feedback_score = (exp.feedback_score - 0.25).clamp(0.0, 1.0);
            if exp.feedback_score < 0.3 {
                exp.status = ExperienceStatus::Deprecated;
            }
        }

        self.store.update_experience(&exp).await
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
        if !auth.has_scope(&Scope::Read) {
            return Err(HubError::InsufficientScope);
        }

        let pool = self.scan_experiences(namespace).await?;

        let terms = crate::algorithm::extract_terms(query);
        let docs: Vec<std::collections::HashSet<String>> = pool
            .iter()
            .map(|e| crate::algorithm::document_tokens(&Self::searchable_text(e)))
            .collect();
        let pool_size = docs.len();
        let doc_freq: Vec<usize> = terms
            .iter()
            .map(|t| docs.iter().filter(|d| d.contains(t)).count())
            .collect();
        let idf = |term: &str| -> f32 {
            terms
                .iter()
                .position(|t| t == term)
                .map(|i| crate::algorithm::inverse_document_frequency(pool_size, doc_freq[i]))
                .unwrap_or(1.0)
        };

        let mut results = Vec::new();
        for (exp, tokens) in pool.into_iter().zip(docs) {
            // 归档经验不召回（弃用经验保留但由低反馈分自然降序）
            if exp.status == ExperienceStatus::Archived {
                continue;
            }

            let lexical = if terms.is_empty() {
                1.0
            } else {
                crate::algorithm::keyword_score_weighted(&tokens, &terms, &idf)
            };
            if lexical <= 0.0 {
                continue;
            }

            // 置信度 × 反馈分：反馈只调 feedback_score 一个因子，无双重计分
            let mut score = lexical * exp.confidence * exp.feedback_score;

            // 环境边界校验
            let mut env_mismatch = false;
            let mut boundary_warning = None;
            if let Some(env) = current_env {
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
                    if strict_env {
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
        results.truncate(limit.max(1));
        Ok(results)
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
            solution: "使用密钥 sk-abcdef123456 连接测试服务器排查".into(),
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
        assert_eq!(exp.status, ExperienceStatus::Active);
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
        assert_eq!(updated1.status, ExperienceStatus::Active);

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
