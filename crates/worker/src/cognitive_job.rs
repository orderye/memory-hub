//! Chat 认知提炼异步作业 (P2)
//!
//! 核心保障：
//! 1. 幂等执行：对相同内容或已处理会话，不产生重复条目或版本；
//! 2. 瞬态错误重试：支持超时/瞬态错误回退重试（默认 3 次）；
//! 3. 失败安全：无论模型抽取成功或失败，原始证据均已提前持久化（append-only）；
//! 4. 治理纪律：遵循 Manual/SemiAuto/FullAuto 策略，Manual 模式下绝不自动合入。

use common::{AgentId, AuthCtx, HubError, Result};
use mem_core::{ChatCognitiveInput, ChatCognitiveResult, ChatCognitiveService};
use std::sync::Arc;
use tracing::{info, warn};
use uuid::Uuid;

/// 认知提炼执行报告
#[derive(Debug, Clone, Default)]
pub struct ChatCognitiveJobReport {
    /// 是否成功执行完成
    pub success: bool,
    /// 实际尝试次数
    pub attempts: usize,
    /// 持久化或查重命中的证据 ID
    pub evidence_id: Option<Uuid>,
    /// 自动提交入库的条目数
    pub committed_count: usize,
    /// 产生待审或被拒的候选提案数
    pub proposals_count: usize,
    /// 最终错误信息（若失败）
    pub error: Option<String>,
}

/// Chat 认知提炼作业执行器
pub struct ChatCognitiveJob {
    service: Arc<ChatCognitiveService>,
    worker_agent: AgentId,
    max_retries: usize,
    initial_backoff_ms: u64,
}

impl ChatCognitiveJob {
    pub fn new(
        service: Arc<ChatCognitiveService>,
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

    /// 执行会话认知提炼（带瞬态重试与幂等控制）
    pub async fn run_session(
        &self,
        auth: &AuthCtx,
        input: ChatCognitiveInput,
    ) -> Result<ChatCognitiveJobReport> {
        let (_, report) = self.run_session_with_result(auth, input).await?;
        Ok(report)
    }

    /// 执行会话认知提炼并返回完整结果及作业报告
    pub async fn run_session_with_result(
        &self,
        auth: &AuthCtx,
        input: ChatCognitiveInput,
    ) -> Result<(ChatCognitiveResult, ChatCognitiveJobReport)> {
        info!(
            worker = %self.worker_agent,
            session_id = %input.session_id,
            namespace = %input.namespace,
            "开始执行 Chat 认知提炼作业"
        );

        let mut attempts = 0;
        let mut last_err = None;

        while attempts < self.max_retries {
            attempts += 1;
            match self.service.process_chat_session(auth, input.clone()).await {
                Ok(res) => {
                    let report = ChatCognitiveJobReport {
                        success: true,
                        attempts,
                        evidence_id: Some(res.evidence_id),
                        committed_count: res.committed_entries.len(),
                        proposals_count: res.proposals.len(),
                        error: None,
                    };
                    info!(
                        worker = %self.worker_agent,
                        attempts,
                        evidence_id = %res.evidence_id,
                        committed = res.committed_entries.len(),
                        proposals = res.proposals.len(),
                        "Chat 认知提炼作业执行成功"
                    );
                    return Ok((res, report));
                }
                Err(e) => {
                    let retryable = e.is_retryable();
                    warn!(
                        worker = %self.worker_agent,
                        attempt = attempts,
                        max_retries = self.max_retries,
                        retryable,
                        error = %e,
                        "Chat 认知提炼尝试失败"
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
            "Chat 认知提炼作业最终失败"
        );
        Err(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::Utc;
    use common::Scope;
    use mem_core::domain::{
        ChatMessage, ChatRole, CognitiveCandidate, CognitiveGovernanceMode, CognitiveLayer,
        CognitivePolicy, MemoryScene,
    };
    use mem_core::infra::{ChatCognitiveExtractor, EvidenceStore};
    use mem_core::testing::MockChatCognitiveExtractor;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use store::InMemoryHubStore;

    fn make_auth() -> AuthCtx {
        AuthCtx {
            agent_id: "test-agent".to_string(),
            account_id: common::BOOTSTRAP_ACCOUNT_ID,
            scopes: vec![Scope::Read, Scope::Write],
            acl: Default::default(),
        }
    }

    struct FlakyExtractor {
        fail_times: usize,
        counter: AtomicUsize,
        candidate: CognitiveCandidate,
    }

    #[async_trait]
    impl ChatCognitiveExtractor for FlakyExtractor {
        async fn extract_candidates(
            &self,
            _input: &ChatCognitiveInput,
        ) -> Result<Vec<CognitiveCandidate>> {
            let current = self.counter.fetch_add(1, Ordering::SeqCst);
            if current < self.fail_times {
                Err(HubError::Unavailable)
            } else {
                Ok(vec![self.candidate.clone()])
            }
        }
    }

    #[tokio::test]
    async fn test_job_retries_transient_failures_and_succeeds() {
        let store = Arc::new(InMemoryHubStore::new());
        let candidate = CognitiveCandidate {
            scene: MemoryScene::Chat,
            layer: CognitiveLayer::Fact,
            content: "项目统一采用 Rust 2024".to_string(),
            tags: vec!["lang".to_string()],
            valid_from: None,
            valid_until: None,
            occurred_at: None,
            confidence: 0.95,
            metadata: serde_json::json!({}),
            relation_kind: None,
            target_memory_id: None,
        };

        let extractor = Arc::new(FlakyExtractor {
            fail_times: 2,
            counter: AtomicUsize::new(0),
            candidate,
        });

        let policy = CognitivePolicy {
            mode: CognitiveGovernanceMode::FullAuto,
            auto_accept_threshold: 0.7,
        };

        let service = Arc::new(ChatCognitiveService::new(store.clone(), extractor, policy));
        let job = ChatCognitiveJob::new(service, "worker-1", 3).with_backoff_ms(1);

        let input = ChatCognitiveInput {
            session_id: "sess-flaky".to_string(),
            namespace: "flaky-ns".to_string(),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "我们以后用 Rust 2024 编写".to_string(),
                timestamp: Some(Utc::now()),
            }],
            occurred_at: None,
        };

        let auth = make_auth();
        let (res, report) = job
            .run_session_with_result(&auth, input)
            .await
            .expect("重试后应当成功");

        assert!(report.success);
        assert_eq!(report.attempts, 3);
        assert_eq!(res.committed_entries.len(), 1);
        assert_eq!(res.committed_entries[0].content, "项目统一采用 Rust 2024");
    }

    #[tokio::test]
    async fn test_job_idempotency_does_not_duplicate_versions() {
        let store = Arc::new(InMemoryHubStore::new());
        let candidate = CognitiveCandidate {
            scene: MemoryScene::Chat,
            layer: CognitiveLayer::Fact,
            content: "目标交付日期为 2026-11".to_string(),
            tags: vec!["target".to_string()],
            valid_from: None,
            valid_until: None,
            occurred_at: None,
            confidence: 0.9,
            metadata: serde_json::json!({}),
            relation_kind: None,
            target_memory_id: None,
        };

        let extractor =
            Arc::new(MockChatCognitiveExtractor::new().with_candidates(vec![candidate]));
        let policy = CognitivePolicy {
            mode: CognitiveGovernanceMode::FullAuto,
            auto_accept_threshold: 0.7,
        };

        let service = Arc::new(ChatCognitiveService::new(store.clone(), extractor, policy));
        let job = ChatCognitiveJob::new(service, "worker-idem", 3).with_backoff_ms(1);

        let input = ChatCognitiveInput {
            session_id: "sess-idem".to_string(),
            namespace: "idem-ns".to_string(),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "目标交付在 2026-11".to_string(),
                timestamp: Some(Utc::now()),
            }],
            occurred_at: None,
        };

        let auth = make_auth();
        let (res1, rep1) = job
            .run_session_with_result(&auth, input.clone())
            .await
            .unwrap();
        assert!(rep1.success);
        assert_eq!(res1.committed_entries.len(), 1);

        // 第二次执行完全相同的输入：幂等，不产生重复条目或新版本
        let (res2, rep2) = job.run_session_with_result(&auth, input).await.unwrap();
        assert!(rep2.success);
        assert_eq!(res2.evidence_id, res1.evidence_id);
        assert_eq!(res2.committed_entries.len(), 0);
        assert_eq!(res2.proposals.len(), 0);
    }

    #[tokio::test]
    async fn test_job_evidence_persisted_even_when_model_permanently_fails() {
        let store = Arc::new(InMemoryHubStore::new());
        let extractor = Arc::new(MockChatCognitiveExtractor::new().with_failure("invalid_json"));
        let policy = CognitivePolicy::default();

        let service = Arc::new(ChatCognitiveService::new(store.clone(), extractor, policy));
        let job = ChatCognitiveJob::new(service, "worker-fail", 2).with_backoff_ms(1);

        let input = ChatCognitiveInput {
            session_id: "sess-fail".to_string(),
            namespace: "fail-ns".to_string(),
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "模型抽取即将失败的信息".to_string(),
                timestamp: Some(Utc::now()),
            }],
            occurred_at: None,
        };

        let auth = make_auth();
        let err = job.run_session(&auth, input.clone()).await.unwrap_err();
        assert!(matches!(err, HubError::InvalidArgument(_)));

        // 验证即使模型失败，底层的原始证据也已在 store 中妥善保存！
        let hash = ChatCognitiveService::compute_messages_hash(&input);
        let evidences = store
            .list_evidence(&input.namespace, None, None, 10)
            .await
            .unwrap();
        assert!(
            evidences.iter().any(|e| e.content_hash == hash),
            "即使模型解析报错，证据存根绝不能丢失"
        );
    }
}
