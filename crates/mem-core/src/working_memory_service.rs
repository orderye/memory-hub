//! Working 经验记忆服务 (P3)
//!
//! 核心原则：
//! 1. 任务事实层（WorkingFact）与经验层（Experience）严格解耦：
//!    - 任务事实记录“本次任务做了什么”，每个结束的任务均完整归档；
//!    - 经验层通过严格的价值门槛（通用性、可验证性、环境边界、无凭证泄露）；
//! 2. 幂等控制：同一任务多次 finish 保持幂等；
//! 3. 反馈闭环：负反馈降低经验置信度与排序权重，绝不物理删除经验；
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
        let existing = self.store.list_experiences(namespace, None, 100).await?;
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

        if is_success {
            exp.feedback_score = (exp.feedback_score + 0.1).clamp(0.0, 1.0);
            exp.confidence = (exp.confidence + 0.05).clamp(0.0, 1.0);
            if exp.status == ExperienceStatus::Candidate {
                exp.status = ExperienceStatus::Active;
            }
        } else {
            // 负反馈：降低反馈分与置信度，不直接物理删除
            exp.feedback_score = (exp.feedback_score - 0.25).clamp(0.0, 1.0);
            exp.confidence = (exp.confidence - 0.15).clamp(0.0, 1.0);
            if exp.feedback_score < 0.3 {
                exp.status = ExperienceStatus::Deprecated;
            }
        }

        self.store.update_experience(&exp).await
    }

    /// 检索可复用经验（感知环境边界）
    pub async fn search_experiences(
        &self,
        auth: &AuthCtx,
        namespace: &str,
        query: &str,
        current_env: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ExperienceSearchResult>> {
        if !auth.has_scope(&Scope::Read) {
            return Err(HubError::InsufficientScope);
        }

        let all = self.store.list_experiences(namespace, None, 100).await?;

        let query_lower = query.to_lowercase();
        let mut results = Vec::new();

        for exp in all {
            // 过滤已被归档的经验
            if exp.status == ExperienceStatus::Archived {
                continue;
            }

            // 文本/关键词召回打分
            let mut score = 0.0f32;
            let task_lower = exp.task.to_lowercase();
            let solution_lower = exp.solution.to_lowercase();

            if task_lower.contains(&query_lower) {
                score += 0.5;
            }
            if solution_lower.contains(&query_lower) {
                score += 0.3;
            }
            for k in &exp.search_keys {
                if query_lower.contains(&k.to_lowercase()) {
                    score += 0.2;
                }
            }

            if score <= 0.0 && !query.trim().is_empty() {
                continue;
            }

            // 结合置信度与反馈分加权
            score *= exp.confidence * exp.feedback_score;

            // 环境边界校验
            let mut env_mismatch = false;
            let mut boundary_warning = None;

            if let Some(env) = current_env {
                if !exp.boundary_envs.is_empty() {
                    let env_matched = exp
                        .boundary_envs
                        .iter()
                        .any(|be| be.eq_ignore_ascii_case(env));
                    if !env_matched {
                        env_mismatch = true;
                        boundary_warning = Some(format!(
                            "当前环境 [{}] 与经验适用环境边界 {:?} 不匹配，需谨慎参考",
                            env, exp.boundary_envs
                        ));
                        // 环境不匹配时适度降权，但不直接吞没，标明边界供 Agent 判别
                        score *= 0.5;
                    }
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

        // 验证：反馈分与置信度下降，但实体并未被删除
        assert!(updated1.feedback_score < 1.0);
        assert!(updated1.confidence < 0.85);
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
}
