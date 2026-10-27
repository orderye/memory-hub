//! 资产治理端到端回归（P1 生命周期 / P2 集合与 ACL）
//!
//! 只使用公开 API + 测试存储，覆盖任务书 4.3 / 4.4 / 4.6 的关键断言。

use async_trait::async_trait;
use common::{AccountId, AuthCtx, HubError, Scope, BOOTSTRAP_ACCOUNT_ID};
use mem_core::{
    scope_name, CodingMemoryService, CollectionFilter, CollectionService, ExperienceCandidate,
    ExperienceStatus, ExperienceTransition, MemoryExperience, MemoryScene, TaskStatus, TaskStore,
    TestHubStore, UpsertCollectionRequest, WorkingMemoryService,
};
use std::sync::Arc;
use uuid::Uuid;

fn auth(agent: &str) -> AuthCtx {
    AuthCtx {
        agent_id: agent.to_string(),
        account_id: BOOTSTRAP_ACCOUNT_ID,
        scopes: vec![Scope::Read, Scope::Write, Scope::Admin],
        acl: Default::default(),
    }
}

fn auth_in(account: AccountId, agent: &str) -> AuthCtx {
    AuthCtx {
        agent_id: agent.to_string(),
        account_id: account,
        scopes: vec![Scope::Read, Scope::Write, Scope::Admin],
        acl: Default::default(),
    }
}

fn candidate(task: &str, solution: &str, source_task_id: Uuid) -> ExperienceCandidate {
    ExperienceCandidate {
        task_pattern: task.to_string(),
        search_keys: vec!["连接池".into(), "泄漏".into()],
        solution: solution.to_string(),
        preconditions: serde_json::json!({}),
        boundary_envs: vec!["linux".into()],
        failure_modes: vec!["空闲连接设为 0".into()],
        validation: serde_json::json!({"test": "500 并发压测通过"}),
        confidence: 0.9,
        source_task_id,
    }
}

/// 建一个已完成的任务事实，返回 task id
async fn completed_task(
    service: &WorkingMemoryService,
    auth: &AuthCtx,
    ns: &str,
    text: &str,
) -> Uuid {
    let task = service
        .start_task(auth, ns, text, Some(MemoryScene::Working), None)
        .await
        .unwrap();
    service
        .finish_task(
            auth,
            task.id,
            ns,
            TaskStatus::Completed,
            Some("done".into()),
            None,
            None,
        )
        .await
        .unwrap();
    task.id
}

// ===== 生命周期 =====

#[tokio::test]
async fn distill_only_creates_candidate_and_feedback_does_not_promote() {
    let store = Arc::new(TestHubStore::default());
    let service = WorkingMemoryService::new(store);
    let auth = auth("agent-a");
    let task_id = completed_task(&service, &auth, "ns-life", "修复连接池泄漏").await;

    let exp = service
        .distill_experience(
            &auth,
            "ns-life",
            candidate("连接池泄漏排查", "调整 maxIdle", task_id),
        )
        .await
        .unwrap();
    assert_eq!(exp.status, ExperienceStatus::Candidate, "提炼只能产出候选");
    assert_eq!(exp.version, 1);
    assert_eq!(exp.source_count, 1);

    // 单次正反馈：只涨反馈分，不改变状态（任务书 4.3）
    let after = service
        .record_feedback(&auth, "ns-life", exp.id, true, None)
        .await
        .unwrap();
    assert_eq!(
        after.status,
        ExperienceStatus::Candidate,
        "单次成功不能变成已验证"
    );
}

#[tokio::test]
async fn verify_requires_two_distinct_completed_sources() {
    let store = Arc::new(TestHubStore::default());
    let service = WorkingMemoryService::new(store);
    let auth = auth("agent-a");

    let t1 = completed_task(&service, &auth, "ns-verify", "第一次修复").await;
    let exp = service
        .distill_experience(
            &auth,
            "ns-verify",
            candidate("连接池泄漏排查", "调整 maxIdle", t1),
        )
        .await
        .unwrap();

    let err = service
        .transition(
            &auth,
            "ns-verify",
            exp.id,
            ExperienceTransition::Verify,
            None,
            2,
        )
        .await
        .expect_err("只有一个来源时不得升级");
    assert!(
        format!("{err}").contains("至少需要 2 个"),
        "错误信息应说明门槛：{err}"
    );

    // 第二个已完成来源（同一经验、不同任务）
    let t2 = completed_task(&service, &auth, "ns-verify", "第二次修复").await;
    let same = service
        .distill_experience(
            &auth,
            "ns-verify",
            candidate("连接池泄漏排查", "调整 maxIdle", t2),
        )
        .await
        .unwrap();
    assert_eq!(same.id, exp.id, "相同模式与方案应复用同一条经验");
    assert_eq!(same.source_count, 2);

    let verified = service
        .transition(
            &auth,
            "ns-verify",
            exp.id,
            ExperienceTransition::Verify,
            Some("两处生产验证"),
            2,
        )
        .await
        .unwrap();
    assert_eq!(verified.status, ExperienceStatus::Active);
    assert_eq!(verified.version, 2);
    assert!(verified.verified_at.is_some());
    assert_eq!(verified.verified_by.as_deref(), Some("agent-a"));

    let versions = service
        .list_versions(&auth, "ns-verify", exp.id, 10)
        .await
        .unwrap();
    assert!(versions.len() >= 2, "创建与验证都必须有版本快照");
    assert!(versions
        .iter()
        .any(|v| v.change_kind == mem_core::ExperienceChangeKind::Verified));
}

#[tokio::test]
async fn failed_source_blocks_verification() {
    let store = Arc::new(TestHubStore::default());
    let service = WorkingMemoryService::new(store);
    let auth = auth("agent-a");

    let t1 = completed_task(&service, &auth, "ns-failed", "成功任务").await;
    let exp = service
        .distill_experience(&auth, "ns-failed", candidate("模式A", "方案A", t1))
        .await
        .unwrap();

    // 第二个来源是失败任务
    let failed = service
        .start_task(
            &auth,
            "ns-failed",
            "失败任务",
            Some(MemoryScene::Working),
            None,
        )
        .await
        .unwrap();
    service
        .finish_task(
            &auth,
            failed.id,
            "ns-failed",
            TaskStatus::Failed,
            None,
            Some("boom".into()),
            None,
        )
        .await
        .unwrap();
    service
        .distill_experience(&auth, "ns-failed", candidate("模式A", "方案A", failed.id))
        .await
        .unwrap();

    let err = service
        .transition(
            &auth,
            "ns-failed",
            exp.id,
            ExperienceTransition::Verify,
            None,
            2,
        )
        .await
        .expect_err("存在失败来源时不得验证");
    assert!(format!("{err}").contains("失败"));
}

#[tokio::test]
async fn reject_requires_reason_and_hides_from_default_search() {
    let store = Arc::new(TestHubStore::default());
    let service = WorkingMemoryService::new(store);
    let auth = auth("agent-a");
    let t1 = completed_task(&service, &auth, "ns-reject", "任务").await;
    let exp = service
        .distill_experience(&auth, "ns-reject", candidate("连接池泄漏排查", "方案", t1))
        .await
        .unwrap();

    let err = service
        .transition(
            &auth,
            "ns-reject",
            exp.id,
            ExperienceTransition::Reject,
            None,
            2,
        )
        .await
        .expect_err("reject 必须给理由");
    assert!(format!("{err}").contains("理由"));

    let rejected = service
        .transition(
            &auth,
            "ns-reject",
            exp.id,
            ExperienceTransition::Reject,
            Some("无法复现"),
            2,
        )
        .await
        .unwrap();
    assert_eq!(rejected.status, ExperienceStatus::Rejected);

    let hits = service
        .search_experiences(&auth, "ns-reject", "连接池泄漏排查", Some("linux"), 10)
        .await
        .unwrap();
    assert!(hits.is_empty(), "rejected 默认不参与检索");

    let audited = service
        .search_experiences_governed(
            &auth,
            "ns-reject",
            "连接池泄漏排查",
            mem_core::ExperienceSearchOptions {
                current_env: Some("linux"),
                limit: 10,
                strict_env: false,
                include_statuses: Some(&[ExperienceStatus::Rejected]),
            },
        )
        .await
        .unwrap();
    assert_eq!(audited.len(), 1, "显式审计检索应能取到 rejected");
}

#[tokio::test]
async fn candidate_ranks_below_active_with_equal_lexical_signal() {
    let store = Arc::new(TestHubStore::default());
    let service = WorkingMemoryService::new(store.clone());
    let auth = auth("agent-a");

    for (id, status) in [
        (Uuid::new_v4(), ExperienceStatus::Candidate),
        (Uuid::new_v4(), ExperienceStatus::Active),
    ] {
        let exp = MemoryExperience {
            id,
            namespace: "ns-rank".into(),
            task: "连接池泄漏排查".into(),
            search_keys: vec!["连接池".into()],
            solution: "调整 maxIdle".into(),
            preconditions: serde_json::json!({}),
            boundary_envs: vec!["linux".into()],
            failure_modes: vec![],
            validation: serde_json::json!({"ok": true}),
            confidence: 0.9,
            status,
            feedback_score: 1.0,
            version: 1,
            superseded_by: None,
            verified_at: None,
            verified_by: None,
            verification_note: None,
            status_reason: None,
            source_count: 0,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        // 直接落库：绕过 distill 的幂等查重，构造同文本的不同状态样本
        store
            .experiences
            .write()
            .unwrap()
            .insert(("ns-rank".into(), id), exp);
    }

    let hits = service
        .search_experiences(&auth, "ns-rank", "连接池泄漏排查", Some("linux"), 10)
        .await
        .unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].experience.status, ExperienceStatus::Active);
    assert!(
        hits[0].score > hits[1].score,
        "已验证经验权重必须高于候选：{} vs {}",
        hits[0].score,
        hits[1].score
    );
}

#[tokio::test]
async fn illegal_transitions_are_rejected() {
    let store = Arc::new(TestHubStore::default());
    let service = WorkingMemoryService::new(store);
    let auth = auth("agent-a");
    let t1 = completed_task(&service, &auth, "ns-illegal", "任务").await;
    let exp = service
        .distill_experience(&auth, "ns-illegal", candidate("模式B", "方案B", t1))
        .await
        .unwrap();

    // candidate 不能 deprecate
    let err = service
        .transition(
            &auth,
            "ns-illegal",
            exp.id,
            ExperienceTransition::Deprecate,
            Some("原因"),
            2,
        )
        .await
        .expect_err("candidate 不允许 deprecate");
    assert!(format!("{err}").contains("不允许"));

    // candidate 不能 restore
    let err = service
        .transition(
            &auth,
            "ns-illegal",
            exp.id,
            ExperienceTransition::Restore,
            None,
            2,
        )
        .await
        .expect_err("candidate 不允许 restore");
    assert!(format!("{err}").contains("不允许"));
}

// ===== ACL =====

struct DenyAll;

#[async_trait]
impl mem_core::AccessChecker for DenyAll {
    async fn can_read(&self, _auth: &AuthCtx, _namespace: &str) -> common::Result<bool> {
        Ok(false)
    }
    async fn can_write(&self, _auth: &AuthCtx, _namespace: &str) -> common::Result<bool> {
        Ok(false)
    }
    async fn is_owner(&self, _auth: &AuthCtx, _namespace: &str) -> common::Result<bool> {
        Ok(false)
    }
    async fn ensure_namespace(&self, _auth: &AuthCtx, _namespace: &str) -> common::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn cognitive_services_enforce_namespace_acl() {
    let store = Arc::new(TestHubStore::default());
    let auth = auth("agent-a");

    let working = WorkingMemoryService::new(store.clone()).with_access(Arc::new(DenyAll));
    let err = working
        .start_task(&auth, "secret", "越权任务", None, None)
        .await
        .expect_err("无命名空间权限必须 fail closed");
    assert!(matches!(err, HubError::InsufficientScope));

    let coding = CodingMemoryService::new(store).with_access(Arc::new(DenyAll));
    let err = coding
        .start_thread(
            &auth,
            mem_core::NewCodingThread {
                id: None,
                namespace: "secret".into(),
                repository: "repo".into(),
                branch: None,
                title: "越权线程".into(),
                description: None,
                task_id: None,
            },
        )
        .await
        .expect_err("Coding 也必须 fail closed");
    assert!(matches!(err, HubError::InsufficientScope));
}

// ===== 集合 =====

/// 直接种入 Agent 记录：`TestHubStore` 的 register_agent 故意返回 Unavailable，
/// 但 `get_agent` 可用，足以验证集合绑定的账户归属校验。
fn seed_agent(store: &Arc<TestHubStore>, agent: &str) {
    store.agents.write().unwrap().insert(
        agent.to_string(),
        mem_core::AgentRecord {
            agent_id: agent.to_string(),
            name: agent.to_string(),
            kind: "assistant".into(),
            status: "active".into(),
            owner_account_id: BOOTSTRAP_ACCOUNT_ID,
            redirect_uris: vec![],
            max_scopes: vec![Scope::Read, Scope::Write, Scope::Admin],
            confidential: false,
            created_at: chrono::Utc::now(),
        },
    );
}

#[tokio::test]
async fn collection_binding_is_fail_closed() {
    let store = Arc::new(TestHubStore::default());
    seed_agent(&store, "agent-a");
    seed_agent(&store, "agent-b");
    let service = CollectionService::new(store.clone());
    let admin = auth("agent-a");

    let collection = service
        .upsert(
            &admin,
            UpsertCollectionRequest {
                id: None,
                name: "coding-assets".into(),
                description: None,
                filter: CollectionFilter {
                    namespaces: vec!["project-a".into()],
                    ..Default::default()
                },
            },
        )
        .await
        .unwrap();

    // 未绑定 → fail closed
    let err = service
        .resolve(&auth("agent-b"), "coding-assets")
        .await
        .expect_err("未绑定集合不得解析");
    assert!(matches!(err, HubError::InsufficientScope));

    service
        .bind(&admin, collection.id, "agent-b")
        .await
        .unwrap();
    let resolved = service
        .resolve(&auth("agent-b"), "coding-assets")
        .await
        .unwrap();
    assert_eq!(resolved.resolved_namespaces, vec!["project-a".to_string()]);

    // 解绑立即生效
    service
        .unbind(&admin, collection.id, "agent-b")
        .await
        .unwrap();
    assert!(matches!(
        service.resolve(&auth("agent-b"), "coding-assets").await,
        Err(HubError::InsufficientScope)
    ));
}

#[tokio::test]
async fn collection_rejects_wildcard_and_cross_account_binding() {
    let store = Arc::new(TestHubStore::default());
    seed_agent(&store, "agent-a");
    let service = CollectionService::new(store.clone());
    let admin = auth("agent-a");

    let err = service
        .upsert(
            &admin,
            UpsertCollectionRequest {
                id: None,
                name: "all".into(),
                description: None,
                filter: CollectionFilter {
                    namespaces: vec!["*".into()],
                    ..Default::default()
                },
            },
        )
        .await
        .expect_err("通配符必须拒绝");
    assert!(format!("{err}").contains("通配"));

    let collection = service
        .upsert(
            &admin,
            UpsertCollectionRequest {
                id: None,
                name: "scoped".into(),
                description: None,
                filter: CollectionFilter {
                    namespaces: vec!["ns-a".into()],
                    ..Default::default()
                },
            },
        )
        .await
        .unwrap();

    // 另一个账户不能绑定该集合（也看不到它）
    let other = auth_in(Uuid::from_u128(42), "agent-x");
    assert!(service
        .bind(&other, collection.id, "agent-a")
        .await
        .is_err());
}

#[tokio::test]
async fn coding_chain_records_change_and_revert_in_order() {
    let store = Arc::new(TestHubStore::default());
    let service = CodingMemoryService::new(store);
    let auth = auth("agent-a");

    let thread = service
        .start_thread(
            &auth,
            mem_core::NewCodingThread {
                id: None,
                namespace: "ns-code".into(),
                repository: "acme/repo".into(),
                branch: Some("main".into()),
                title: "重试策略".into(),
                description: None,
                task_id: None,
            },
        )
        .await
        .unwrap();

    let change = mem_core::NewCodingRevision {
        id: None,
        thread_id: Some(thread.id),
        namespace: "ns-code".into(),
        repository: "acme/repo".into(),
        branch: Some("main".into()),
        commit_sha: Some("c1".into()),
        parent_sha: None,
        file_path: "src/retry.rs".into(),
        change_kind: mem_core::CodeChangeKind::Modify,
        diff_content: "@@ -1 +1 @@\n-const N: u32 = 3;\n+const N: u32 = 5;\n".into(),
        symbol_fingerprint: Some("retry::N".into()),
        is_committed: true,
        author: Some("dev".into()),
        occurred_at: None,
        source_evidence_id: None,
        source_task_id: None,
    };
    let first = service.record_revision(&auth, change).await.unwrap();

    // 回滚：新的修订把改动改回去（真实 diff 相反），parent_sha 指向前一次提交
    let revert = mem_core::NewCodingRevision {
        id: None,
        thread_id: Some(thread.id),
        namespace: "ns-code".into(),
        repository: "acme/repo".into(),
        branch: Some("main".into()),
        commit_sha: Some("c2".into()),
        parent_sha: Some("c1".into()),
        file_path: "src/retry.rs".into(),
        change_kind: mem_core::CodeChangeKind::Modify,
        diff_content: "@@ -1 +1 @@\n-const N: u32 = 5;\n+const N: u32 = 3;\n".into(),
        symbol_fingerprint: Some("retry::N".into()),
        is_committed: true,
        author: Some("dev".into()),
        occurred_at: None,
        source_evidence_id: None,
        source_task_id: None,
    };
    let second = service.record_revision(&auth, revert).await.unwrap();
    assert_ne!(first.diff_hash, second.diff_hash);
    assert_eq!(second.parent_sha.as_deref(), Some("c1"));

    let chain = service
        .list_file_revisions(&auth, "ns-code", "acme/repo", "src/retry.rs", 10)
        .await
        .unwrap();
    assert_eq!(chain.len(), 2, "两次修改都必须可在文件演化链中查到");

    // 跨命名空间不可见
    let other_ns = service
        .list_file_revisions(&auth, "other-ns", "acme/repo", "src/retry.rs", 10)
        .await
        .unwrap();
    assert!(other_ns.is_empty(), "命名空间隔离不得泄露演化链");
}

#[tokio::test]
async fn deleting_collection_does_not_delete_memories() {
    let store = Arc::new(TestHubStore::default());
    seed_agent(&store, "agent-a");
    let service = CollectionService::new(store.clone());
    let admin = auth("agent-a");
    let working = WorkingMemoryService::new(store.clone()).scoped(admin.account_id);

    let task = working
        .start_task(&admin, "keep-ns", "必须保留的任务", None, None)
        .await
        .unwrap();

    let collection = service
        .upsert(
            &admin,
            UpsertCollectionRequest {
                id: None,
                name: "keep".into(),
                description: None,
                filter: CollectionFilter {
                    namespaces: vec!["keep-ns".into()],
                    ..Default::default()
                },
            },
        )
        .await
        .unwrap();
    service
        .bind(&admin, collection.id, "agent-a")
        .await
        .unwrap();
    service.delete(&admin, collection.id).await.unwrap();

    let scoped_ns = scope_name(&BOOTSTRAP_ACCOUNT_ID, "keep-ns");
    let tasks = store.all_tasks_in_namespace(&scoped_ns).await.unwrap();
    assert!(
        tasks.iter().any(|t| t.id == task.id),
        "删除集合不得删除底层任务事实"
    );
}
