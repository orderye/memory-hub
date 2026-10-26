use super::*;
use crate::domain::{ChatMessage, ChatRole, EvidenceSourceKind, NewEvidence};
use crate::infra::*;
use crate::testing::{MockChatCognitiveExtractor, TestHubStore};

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
                content: "架构评审已通过，即日起决定改用 PostgreSQL 15 作为主数据库".to_string(),
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

    let service = ChatCognitiveService::new(store.clone(), extractor, CognitivePolicy::default());

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
                content: "演练记录：本周把只读副本从 MySQL 改用 PostgreSQL 驱动重连".to_string(),
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
                content: "架构评审已通过，即日起决定改用 PostgreSQL 15 作为主数据库".to_string(),
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
            MockChatCognitiveExtractor::new().with_candidates(vec![candidate("低置信度候选", 0.7)]),
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
            MockChatCognitiveExtractor::new().with_candidates(vec![candidate("确定事实", 0.99)]),
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

/// 回归：同批候选查重。existing_memories 是循环开始前的快照，
/// 同一次抽取返回的两个相同候选必须只提交一条（旧实现会提交两条）。
#[tokio::test]
async fn test_same_batch_identical_candidates_commit_once() {
    let store = Arc::new(TestHubStore::default());
    let service = ChatCognitiveService::new(
        store.clone(),
        Arc::new(MockChatCognitiveExtractor::new().with_candidates(vec![
            candidate("同批重复事实", 0.95),
            candidate("同批重复事实", 0.95),
        ])),
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
                namespace: "batch-dup-ns".to_string(),
                session_id: "sess-batch-dup".to_string(),
                messages: vec![ChatMessage {
                    role: ChatRole::User,
                    content: "记录一条事实两次".to_string(),
                    timestamp: None,
                }],
                occurred_at: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(
        res.committed_entries.len(),
        1,
        "同批相同候选只允许提交一条"
    );
    let active = store.all_in_namespace("batch-dup-ns", false).await.unwrap();
    assert_eq!(active.len(), 1);
}

/// 回归：证据幂等查重必须覆盖会话**全部**历史证据。
/// 旧实现按"最近 100 条"窗口扫描：同会话证据超过窗口后，
/// 重复提交同一会话会再落一条证据（幂等失效）。
#[tokio::test]
async fn test_evidence_idempotency_survives_beyond_window() {
    let store = Arc::new(TestHubStore::default());
    let service = ChatCognitiveService::new(
        store.clone(),
        Arc::new(MockChatCognitiveExtractor::new().with_candidates(vec![candidate(
            "窗口外幂等", 0.99,
        )])),
        CognitivePolicy {
            mode: CognitiveGovernanceMode::FullAuto,
            auto_accept_threshold: 0.95,
            ..Default::default()
        },
    );
    let input = ChatCognitiveInput {
        namespace: "ev-idem-ns".to_string(),
        session_id: "sess-ev-idem".to_string(),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "首次提交".to_string(),
            timestamp: None,
        }],
        occurred_at: None,
    };

    let first = service
        .process_chat_session(&auth(), input.clone())
        .await
        .unwrap();

    // 向同一会话灌入 300 条无关证据，把首条挤出任何列表窗口
    for i in 0..300 {
        store
            .append_evidence(&NewEvidence {
                id: None,
                namespace: "ev-idem-ns".to_string(),
                source_kind: EvidenceSourceKind::Session,
                source_ref: Some("sess-ev-idem".to_string()),
                content_hash: format!("dummy-hash-{i:04}"),
                excerpt: None,
                occurred_at: None,
                expires_at: None,
            })
            .await
            .unwrap();
    }

    let second = service
        .process_chat_session(&auth(), input)
        .await
        .unwrap();
    assert_eq!(
        second.evidence_id, first.evidence_id,
        "窗口外仍必须命中同一条证据，不得重复落证据"
    );
}
