//! Working 经验记忆提炼异步作业 (P3)
//!
//! 核心保障：
//! 1. 幂等执行：对相同任务模式或已提炼的经验，不重复创建，保障幂等关联；
//! 2. 严格门槛：严格过滤敏感凭证、局部任务描述，未达标候选拒绝入库并不予无效重试；
//! 3. 瞬态错误重试：遇到可重试瞬态异常支持指数退避重试；
//! 4. 任务事实分离：源任务事实早已安全持久化，经验提炼异步解耦执行。

use common::{AgentId, AuthCtx, HubError, Result};
use mem_core::domain::{ExperienceCandidate, ExperienceStatus, MemoryExperience};
use mem_core::working_memory_service::WorkingMemoryService;
use std::sync::Arc;
use tracing::{info, warn};
use uuid::Uuid;

/// 经验提炼执行报告
#[derive(Debug, Clone, Default)]
pub struct ExperienceDistillReport {
    /// 是否成功执行完成
    pub success: bool,
    /// 实际尝试次数
    pub attempts: usize,
    /// 源任务 ID
    pub task_id: Uuid,
    /// 提炼生成或复用的经验 ID
    pub experience_id: Option<Uuid>,
    /// 经验当前状态
    pub status: Option<ExperienceStatus>,
    /// 质量未达标时的拒绝原因（若有）
    pub rejected_reason: Option<String>,
    /// 最终错误信息（若失败）
    pub error: Option<String>,
}

/// Working 经验记忆异步提炼作业
pub struct ExperienceDistillJob {
    service: Arc<WorkingMemoryService>,
    worker_agent: AgentId,
    max_retries: usize,
    initial_backoff_ms: u64,
}

impl ExperienceDistillJob {
    pub fn new(
        service: Arc<WorkingMemoryService>,
        worker_agent: impl Into<String>,
        max_retries: usize,
    ) -> Self {
        Self {
            service,
            worker_agent: worker_agent.into(),
            max_retries: max_retries.max(1),
            initial_backoff_ms: 10,
        }
    }

    pub fn with_backoff_ms(mut self, ms: u64) -> Self {
        self.initial_backoff_ms = ms;
        self
    }

    /// 执行经验提炼作业
    pub async fn run_distill(
        &self,
        auth: &AuthCtx,
        namespace: &str,
        candidate: ExperienceCandidate,
    ) -> Result<ExperienceDistillReport> {
        let (_, report) = self
            .run_distill_with_result(auth, namespace, candidate)
            .await?;
        Ok(report)
    }

    /// 执行经验提炼作业并返回完整结果及报告
    pub async fn run_distill_with_result(
        &self,
        auth: &AuthCtx,
        namespace: &str,
        candidate: ExperienceCandidate,
    ) -> Result<(MemoryExperience, ExperienceDistillReport)> {
        let task_id = candidate.source_task_id;
        info!(
            worker = %self.worker_agent,
            namespace = %namespace,
            task_id = %task_id,
            "开始执行 Working 经验记忆提炼作业"
        );

        let mut attempts = 0;
        let mut last_err = None;

        while attempts < self.max_retries {
            attempts += 1;
            match self
                .service
                .distill_experience(auth, namespace, candidate.clone())
                .await
            {
                Ok(exp) => {
                    let report = ExperienceDistillReport {
                        success: true,
                        attempts,
                        task_id,
                        experience_id: Some(exp.id),
                        status: Some(exp.status),
                        rejected_reason: None,
                        error: None,
                    };
                    info!(
                        worker = %self.worker_agent,
                        attempts,
                        task_id = %task_id,
                        experience_id = %exp.id,
                        status = ?exp.status,
                        "Working 经验记忆提炼作业执行成功"
                    );
                    return Ok((exp, report));
                }
                Err(HubError::InvalidArgument(reason)) => {
                    // 门槛校验失败（非瞬态异常，直接返回并记录拒绝原因）
                    warn!(
                        worker = %self.worker_agent,
                        task_id = %task_id,
                        reason = %reason,
                        "经验提炼候选未达质量门槛，已拒绝入库"
                    );
                    return Err(HubError::InvalidArgument(reason));
                }
                Err(e) => {
                    let retryable = e.is_retryable();
                    warn!(
                        worker = %self.worker_agent,
                        attempt = attempts,
                        max_retries = self.max_retries,
                        retryable,
                        error = %e,
                        "经验提炼作业尝试失败"
                    );

                    if !retryable || attempts >= self.max_retries {
                        last_err = Some(e);
                        break;
                    }

                    // 瞬态重试退避
                    let backoff = self.initial_backoff_ms * (1 << (attempts - 1));
                    tokio::time::sleep(tokio::time::Duration::from_millis(backoff)).await;
                }
            }
        }

        let err = last_err.unwrap_or(HubError::Unavailable);
        warn!(
            worker = %self.worker_agent,
            attempts,
            error = %err,
            "Working 经验记忆提炼作业最终失败"
        );
        Err(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::{Scope, BOOTSTRAP_ACCOUNT_ID};
    use mem_core::domain::{MemoryScene, TaskStatus};
    use store::InMemoryHubStore;

    fn make_auth() -> AuthCtx {
        AuthCtx {
            agent_id: "test-distill-worker".to_string(),
            account_id: BOOTSTRAP_ACCOUNT_ID,
            scopes: vec![Scope::Read, Scope::Write],
            acl: Default::default(),
        }
    }

    #[tokio::test]
    async fn test_experience_distill_job_success_and_idempotency() {
        let store = Arc::new(InMemoryHubStore::new());
        let service = Arc::new(WorkingMemoryService::new(store));
        let job = ExperienceDistillJob::new(service.clone(), "worker-1", 3);
        let auth = make_auth();

        // 1. 创建任务并完成
        let task = service
            .start_task(
                &auth,
                "ns_distill",
                "排查并修复生产环境 Redis 连接池泄漏",
                Some(MemoryScene::Working),
                None,
            )
            .await
            .unwrap();

        service
            .finish_task(
                &auth,
                task.id,
                "ns_distill",
                TaskStatus::Completed,
                Some("排查并修改了最大空闲连接数，压测无泄漏".into()),
                None,
                None,
            )
            .await
            .unwrap();

        // 2. 提炼合格经验
        let candidate = ExperienceCandidate {
            source_task_id: task.id,
            task_pattern: "Redis 连接池耗尽或句柄泄漏排查与参数优化".into(),
            search_keys: vec!["redis".into(), "pool".into(), "leak".into()],
            solution: "调大连接池最大存活时间并配合合理的最大空闲连接数，定期健康探测".into(),
            preconditions: serde_json::json!({"auth": "redis_cluster"}),
            boundary_envs: vec!["linux".into(), "k8s".into()],
            failure_modes: vec!["空闲连接数设为 0 导致每次重新握手".into()],
            validation: serde_json::json!({"benchmark": "500 并发压测连接复用正常"}),
            confidence: 0.9,
        };

        let (exp, report) = job
            .run_distill_with_result(&auth, "ns_distill", candidate.clone())
            .await
            .unwrap();

        assert!(report.success);
        assert_eq!(report.attempts, 1);
        assert_eq!(exp.task, candidate.task_pattern);

        // 3. 幂等再次执行：命中已有经验并直接返回
        let (exp2, report2) = job
            .run_distill_with_result(&auth, "ns_distill", candidate)
            .await
            .unwrap();

        assert!(report2.success);
        assert_eq!(exp2.id, exp.id);
    }

    #[tokio::test]
    async fn test_experience_distill_job_rejects_credential_leak() {
        let store = Arc::new(InMemoryHubStore::new());
        let service = Arc::new(WorkingMemoryService::new(store));
        let job = ExperienceDistillJob::new(service.clone(), "worker-1", 3);
        let auth = make_auth();

        let task = service
            .start_task(&auth, "ns_sec", "部署云数据库实例", None, None)
            .await
            .unwrap();

        // 尝试提炼带有敏感 Token 泄露的候选
        let bad_candidate = ExperienceCandidate {
            source_task_id: task.id,
            task_pattern: "自动化云数据库配置与部署".into(),
            search_keys: vec!["db".into()],
            solution: format!(
                "使用密钥 {} 自动化初始化",
                concat!("sk-ant-api03-", "secret-key-1234567890abcdef")
            ),
            preconditions: serde_json::json!({"cloud": "aws"}),
            boundary_envs: vec!["aws".into()],
            failure_modes: vec!["权限不足".into()],
            validation: serde_json::json!({"test": "ok"}),
            confidence: 0.9,
        };

        let err = job
            .run_distill_with_result(&auth, "ns_sec", bad_candidate)
            .await
            .unwrap_err();

        match err {
            HubError::InvalidArgument(msg) => {
                assert!(msg.contains("敏感凭证") || msg.contains("密钥"));
            }
            _ => panic!("应当因敏感凭证泄露直接拒绝: {:?}", err),
        }
    }
}
