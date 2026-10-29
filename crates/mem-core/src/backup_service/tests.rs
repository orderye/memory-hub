use super::*;
/// 测试账户作用域名：与 `auth()`/`writer()` 辅助中的账户一致
fn scoped_ns(ns: &str) -> String {
    crate::account_scope::scope_name(&common::BOOTSTRAP_ACCOUNT_ID, ns)
}
use crate::domain::{
    EvidenceSourceKind, ExperienceStatus, IntegrationProposal, MemoryEvidence, MemoryFilter,
    MemoryKind, MemoryRelation, MemoryRelationKind, MemoryStatus, MemoryVersion,
    NamespaceGrantRecord, NamespaceInfo, NewEvidence, NewMemoryRelation, ProposalStatus,
    TaskStatus,
};
use crate::infra::{BackupStore, Embedder, EvidenceStore, GrantStore, MemoryStore, RelationStore};
use crate::testing::{TestEmbedder, TestHubStore};
use common::{AgentId, Scope};

fn admin(agent: &str) -> AuthCtx {
    AuthCtx {
        agent_id: agent.to_string(),
        account_id: common::BOOTSTRAP_ACCOUNT_ID,
        scopes: vec![Scope::Read, Scope::Write, Scope::Admin],
        acl: Default::default(),
    }
}

fn setup() -> (DefaultBackupService, Arc<TestHubStore>) {
    let store = Arc::new(TestHubStore::default());
    let svc = DefaultBackupService::new(store.clone(), Arc::new(GrantChecker::new(store.clone())));
    (svc, store)
}

#[tokio::test]
async fn test_backup_create_and_restore_dry_run() {
    let (svc, store) = setup();
    let a = admin("agent-a");

    // 建立命名空间归属
    store
        .ensure_namespace(&scoped_ns("ns"), &"agent-a".to_string())
        .await
        .unwrap();
    store
        .grant(&NamespaceGrantRecord {
            namespace: scoped_ns("ns"),
            agent_id: "agent-a".to_string(),
            can_read: true,
            can_write: true,
            granted_by: "agent-a".to_string(),
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    let e1 = store.seed(&scoped_ns("ns"), "agent-a", "记忆一");
    store.seed(&scoped_ns("ns"), "agent-a", "记忆二");

    let backup = svc
        .create(
            &a,
            BackupCreateRequest {
                namespace: "ns".to_string(),
                note: Some("测试备份".to_string()),
            },
        )
        .await
        .unwrap();
    assert_eq!(backup.entry_count, 2);
    assert!(!backup.checksum.is_empty());

    // dry_run：应识别 2 条 unchanged
    let report = svc
        .restore(
            &a,
            RestoreRequest {
                backup_id: backup.id,
                mode: RestoreMode::DryRun,
            },
        )
        .await
        .unwrap();
    assert_eq!(report.total, 2);
    assert_eq!(report.unchanged, 2);
    assert_eq!(
        backup.payload["version"],
        serde_json::json!(6),
        "complete backups must use v6 to cover memory version history"
    );
    assert!(backup.payload.get("memory_versions").is_some());

    // 修改一条后 merge：应识别 1 updated 1 unchanged
    let mut changed = store.get(&scoped_ns("ns"), e1.id).await.unwrap();
    changed.content = "被外部改动了".to_string();
    store.save(&changed).await.unwrap();

    let report2 = svc
        .restore(
            &a,
            RestoreRequest {
                backup_id: backup.id,
                mode: RestoreMode::Merge,
            },
        )
        .await
        .unwrap();
    assert_eq!(report2.updated, 1);
    assert_eq!(report2.unchanged, 1);
    assert_eq!(
        store.get(&scoped_ns("ns"), e1.id).await.unwrap().content,
        "记忆一",
        "merge 后内容应恢复为快照值"
    );
}

#[tokio::test]
async fn test_replace_uses_atomic_snapshot_api() {
    let (svc, store) = setup();
    let auth = admin("agent-a");
    for ns in ["atomic-a", "atomic-b"] {
        store
            .ensure_namespace(&scoped_ns(ns), &"agent-a".to_string())
            .await
            .unwrap();
        store
            .grant(&NamespaceGrantRecord {
                namespace: scoped_ns(ns),
                agent_id: "agent-a".to_string(),
                can_read: true,
                can_write: true,
                granted_by: "agent-a".to_string(),
                created_at: chrono::Utc::now(),
            })
            .await
            .unwrap();
        store.seed(&scoped_ns(ns), "agent-a", &format!("{ns} 数据"));
    }
    let backup = svc
        .create(
            &auth,
            BackupCreateRequest {
                namespace: ALL_NAMESPACES.to_string(),
                note: None,
            },
        )
        .await
        .unwrap();

    svc.restore(
        &auth,
        RestoreRequest {
            backup_id: backup.id,
            mode: RestoreMode::Replace,
        },
    )
    .await
    .unwrap();

    assert_eq!(
        store
            .replace_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "Replace 必须一次性走原子替换接口，而不是逐 namespace 分步提交"
    );
    for ns in ["atomic-a", "atomic-b"] {
        let items = store.all_in_namespace(&scoped_ns(ns), true).await.unwrap();
        assert_eq!(items.len(), 1, "{ns} 应被完整替换");
        assert!(items[0].content.contains(ns));
    }
}

/// Replace 后各关联实体必须整体一致：证据、关系、任务、经验、版本、关联与代码链
/// 全部回到快照状态，且没有残留的旧数据。
#[tokio::test]
async fn test_replace_restores_all_related_entities_consistently() {
    use crate::domain::{
        CodeChangeKind, MemoryScene, NewCodingRevision, NewCodingThread, NewExperienceVersion,
        NewMemoryExperience, NewMemoryTask,
    };
    use crate::infra::{CodingStore, ExperienceStore, TaskStore};

    let (svc, store) = setup();
    let auth = admin("agent-a");
    let ns = "consistency";
    store
        .ensure_namespace(&scoped_ns(ns), &"agent-a".to_string())
        .await
        .unwrap();
    store
        .grant(&NamespaceGrantRecord {
            namespace: scoped_ns(ns),
            agent_id: "agent-a".to_string(),
            can_read: true,
            can_write: true,
            granted_by: "agent-a".to_string(),
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();

    let a = store.seed(&scoped_ns(ns), "agent-a", "记忆 A");
    let b = store.seed(&scoped_ns(ns), "agent-a", "记忆 B");
    let evidence = store
        .append_evidence(&NewEvidence {
            id: None,
            namespace: scoped_ns(ns),
            source_kind: EvidenceSourceKind::Session,
            source_ref: Some("s-1".to_string()),
            content_hash: "h-1".to_string(),
            excerpt: Some("证据".to_string()),
            occurred_at: Some(chrono::Utc::now()),
            expires_at: None,
        })
        .await
        .unwrap();
    store
        .add_relation(
            &scoped_ns(ns),
            &NewMemoryRelation {
                from_memory_id: a.id,
                to_memory_id: b.id,
                relation_kind: MemoryRelationKind::DerivedFrom,
                confidence: 0.9,
                source_evidence_id: Some(evidence.id),
            },
        )
        .await
        .unwrap();
    let task = store
        .create_task(&NewMemoryTask {
            id: None,
            namespace: scoped_ns(ns),
            scene: MemoryScene::Working,
            task: "任务".to_string(),
            status: TaskStatus::Completed,
            started_at: chrono::Utc::now(),
            finished_at: Some(chrono::Utc::now()),
            result_summary: None,
            failure_reason: None,
            source_evidence_id: Some(evidence.id),
            metadata: serde_json::json!({}),
        })
        .await
        .unwrap();
    let experience = store
        .save_experience(&NewMemoryExperience {
            id: None,
            namespace: scoped_ns(ns),
            task: "经验".to_string(),
            search_keys: vec!["k".to_string()],
            solution: "方案".to_string(),
            preconditions: serde_json::json!({}),
            boundary_envs: vec![],
            failure_modes: vec![],
            validation: serde_json::json!({}),
            confidence: 0.8,
            status: ExperienceStatus::Active,
            feedback_score: 1.0,
        })
        .await
        .unwrap();
    store
        .link_task_experience(&crate::domain::MemoryTaskExperience {
            namespace: Some(scoped_ns(ns)),
            task_id: task.id,
            experience_id: experience.id,
            relation_kind: "derived_from".to_string(),
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    store
        .append_experience_version(&NewExperienceVersion {
            id: None,
            experience_id: experience.id,
            namespace: scoped_ns(ns),
            version: 1,
            snapshot: serde_json::json!({"solution": "旧方案"}),
            change_kind: crate::domain::ExperienceChangeKind::Created,
            changed_by: "agent-a".to_string(),
            status: ExperienceStatus::Candidate,
            note: None,
            changed_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    let thread = store
        .create_thread(&NewCodingThread {
            id: None,
            namespace: scoped_ns(ns),
            repository: "org/repo".to_string(),
            branch: Some("main".to_string()),
            task_id: Some(task.id),
            title: "线程".to_string(),
            description: None,
        })
        .await
        .unwrap();
    store
        .save_revision(
            &NewCodingRevision {
                id: None,
                thread_id: Some(thread.id),
                namespace: scoped_ns(ns),
                repository: "org/repo".to_string(),
                branch: Some("main".to_string()),
                commit_sha: Some("abc".to_string()),
                parent_sha: None,
                file_path: "src/lib.rs".to_string(),
                change_kind: CodeChangeKind::Modify,
                diff_content: "+x".to_string(),
                symbol_fingerprint: None,
                is_committed: true,
                author: None,
                occurred_at: Some(chrono::Utc::now()),
                source_evidence_id: Some(evidence.id),
                source_task_id: Some(task.id),
            },
            "hash-1",
        )
        .await
        .unwrap();

    let backup = svc
        .create(
            &auth,
            BackupCreateRequest {
                namespace: ns.to_string(),
                note: None,
            },
        )
        .await
        .unwrap();

    // 全部清空后 Replace：必须整体回到快照状态
    for item in store.all_in_namespace(&scoped_ns(ns), true).await.unwrap() {
        store.delete(&scoped_ns(ns), item.id).await.unwrap();
    }
    for e in store
        .all_evidence_in_namespace(&scoped_ns(ns))
        .await
        .unwrap()
    {
        store.delete_evidence(&scoped_ns(ns), e.id).await.unwrap();
    }
    for t in store.all_tasks_in_namespace(&scoped_ns(ns)).await.unwrap() {
        store.delete_task(&scoped_ns(ns), t.id).await.unwrap();
    }
    for e in store
        .all_experiences_in_namespace(&scoped_ns(ns))
        .await
        .unwrap()
    {
        store.delete_experience(&scoped_ns(ns), e.id).await.unwrap();
    }
    for t in store
        .all_threads_in_namespace(&scoped_ns(ns))
        .await
        .unwrap()
    {
        store.delete_thread(&scoped_ns(ns), t.id).await.unwrap();
    }

    let report = svc
        .restore(
            &auth,
            RestoreRequest {
                backup_id: backup.id,
                mode: RestoreMode::Replace,
            },
        )
        .await
        .unwrap();
    assert_eq!(report.created, 2, "两条记忆都应重建");
    assert_eq!(report.failed_evidences, 0);
    assert_eq!(report.failed_relations, 0);
    assert_eq!(report.failed_tasks, 0);
    assert_eq!(report.failed_experiences, 0);
    assert_eq!(report.failed_threads, 0);
    assert_eq!(report.failed_revisions, 0);
    assert!(
        report.safety_backup_id.is_some(),
        "Replace 必须先建安全备份"
    );

    assert_eq!(
        store
            .all_in_namespace(&scoped_ns(ns), true)
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        store
            .all_evidence_in_namespace(&scoped_ns(ns))
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .all_relations_in_namespace(&scoped_ns(ns))
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .all_tasks_in_namespace(&scoped_ns(ns))
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .all_experiences_in_namespace(&scoped_ns(ns))
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .all_experience_versions_in_namespace(&scoped_ns(ns))
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .all_task_experiences_in_namespace(&scoped_ns(ns))
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .all_threads_in_namespace(&scoped_ns(ns))
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .all_revisions_in_namespace(&scoped_ns(ns))
            .await
            .unwrap()
            .len(),
        1
    );

    // 记忆版本历史也必须随之恢复
    assert!(
        !store.versions(a.id).await.unwrap().is_empty(),
        "记忆版本历史应恢复"
    );
}

/// 备份与恢复必须按账户隔离：另一账户不能读取或恢复本账户的快照。
#[tokio::test]
async fn test_backup_and_restore_are_isolated_between_accounts() {
    let (svc, store) = setup();
    let owner = admin("agent-a");
    let ns = "isolated";
    store
        .ensure_namespace(&scoped_ns(ns), &"agent-a".to_string())
        .await
        .unwrap();
    store
        .grant(&NamespaceGrantRecord {
            namespace: scoped_ns(ns),
            agent_id: "agent-a".to_string(),
            can_read: true,
            can_write: true,
            granted_by: "agent-a".to_string(),
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    store.seed(&scoped_ns(ns), "agent-a", "本账户机密");
    let backup = svc
        .create(
            &owner,
            BackupCreateRequest {
                namespace: ns.to_string(),
                note: None,
            },
        )
        .await
        .unwrap();

    // 另一个账户：同名 namespace 在它的作用域里是不同的物理命名空间
    let other_account = uuid::Uuid::from_u128(2);
    let intruder = AuthCtx {
        agent_id: "agent-b".to_string(),
        account_id: other_account,
        scopes: vec![Scope::Read, Scope::Write, Scope::Admin],
        acl: Default::default(),
    };
    let err = svc
        .restore(
            &intruder,
            RestoreRequest {
                backup_id: backup.id,
                mode: RestoreMode::Replace,
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, HubError::NotFound | HubError::InsufficientScope),
        "跨账户恢复必须被拒绝，实际: {err:?}"
    );

    let listed = svc.list(&intruder, None).await.unwrap();
    assert!(listed.is_empty(), "跨账户不得看到他人快照");

    // 本账户数据未受影响
    let items = store.all_in_namespace(&scoped_ns(ns), true).await.unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].content, "本账户机密");
}

#[tokio::test]
async fn test_restore_writes_audit_records_for_success_and_failure() {
    let (svc, store) = setup();
    let auth = admin("agent-a");
    let ns = "audited";
    store
        .ensure_namespace(&scoped_ns(ns), &"agent-a".to_string())
        .await
        .unwrap();
    store
        .grant(&NamespaceGrantRecord {
            namespace: scoped_ns(ns),
            agent_id: "agent-a".to_string(),
            can_read: true,
            can_write: true,
            granted_by: "agent-a".to_string(),
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    store.seed(&scoped_ns(ns), "agent-a", "审计数据");
    let backup = svc
        .create(
            &auth,
            BackupCreateRequest {
                namespace: ns.to_string(),
                note: None,
            },
        )
        .await
        .unwrap();

    svc.restore(
        &auth,
        RestoreRequest {
            backup_id: backup.id,
            mode: RestoreMode::Merge,
        },
    )
    .await
    .unwrap();

    // 失败：未知格式版本
    let mut bad = backup.clone();
    bad.namespace = scoped_ns(ns);
    bad.payload["version"] = serde_json::json!(99);
    store.save_backup(&bad).await.unwrap();
    let failed = svc
        .restore(
            &auth,
            RestoreRequest {
                backup_id: bad.id,
                mode: RestoreMode::Merge,
            },
        )
        .await
        .expect_err("不支持版本必须失败");

    let records = store.audit_records.lock().unwrap().clone();
    let success = records
        .iter()
        .find(|(_, action, _, detail)| {
            action == "memory_restore"
                && detail.get("status").and_then(|s| s.as_str()) == Some("success")
        })
        .expect("成功恢复必须留下审计记录");
    assert_eq!(success.0, "agent-a", "审计必须记录操作者");
    assert_eq!(
        success.2.as_deref(),
        Some(backup.id.to_string().as_str()),
        "审计必须记录目标备份"
    );

    let failure = records
        .iter()
        .find(|(_, action, _, detail)| {
            action == "memory_restore"
                && detail.get("status").and_then(|s| s.as_str()) == Some("failed")
        })
        .expect("失败恢复必须留下审计记录");
    assert!(
        failure.3.get("error").and_then(|e| e.as_str()).is_some(),
        "失败审计必须带诊断信息"
    );
    assert!(!matches!(failed, HubError::NotFound));
}

#[tokio::test]
async fn test_prune_respects_keep_and_ttl_boundaries() {
    use crate::infra::Clock;

    struct FixedClock(chrono::DateTime<chrono::Utc>);
    impl Clock for FixedClock {
        fn now(&self) -> chrono::DateTime<chrono::Utc> {
            self.0
        }
    }

    let store = Arc::new(TestHubStore::default());
    let now = chrono::Utc::now();
    let svc = DefaultBackupService::new(store.clone(), Arc::new(GrantChecker::new(store.clone())))
        .with_clock(Arc::new(FixedClock(now)));

    // 同一命名空间 5 份快照，时间从旧到新
    let mut ids = Vec::new();
    for i in 0..5 {
        let record = BackupRecord {
            id: Uuid::new_v4(),
            namespace: scoped_ns("prune-ns"),
            created_by: "agent-a".to_string(),
            note: None,
            entry_count: 0,
            checksum: String::new(),
            payload: serde_json::json!({}),
            retain_until: None,
            created_at: now - chrono::Duration::days(5 - i),
        };
        store.save_backup(&record).await.unwrap();
        ids.push(record.id);
    }

    // keep=2, retention=0：只保留最新 2 份
    let report = svc.prune(2, 0).await.unwrap();
    assert_eq!(report.deleted.len(), 3);
    let remaining = store
        .list_backups(Some(&scoped_ns("prune-ns")))
        .await
        .unwrap();
    assert_eq!(remaining.len(), 2, "keep=2 必须只留最新两份");
    for kept in &remaining {
        assert!(!report.deleted.contains(&kept.id));
    }

    // TTL：在保留组内再造一份 10 天前的旧快照（非最新），应被淘汰；
    // 最新一份即使超过 TTL 也必须保留。
    let stale = BackupRecord {
        id: Uuid::new_v4(),
        namespace: scoped_ns("prune-ns"),
        created_by: "agent-a".to_string(),
        note: None,
        entry_count: 0,
        checksum: String::new(),
        payload: serde_json::json!({}),
        retain_until: None,
        created_at: now - chrono::Duration::days(10),
    };
    store.save_backup(&stale).await.unwrap();

    let report_ttl = svc.prune(10, 3).await.unwrap();
    assert_eq!(
        report_ttl.deleted.len(),
        1,
        "TTL 只淘汰非最新且过期的快照，最新一份永远保留"
    );
    assert!(report_ttl.deleted.contains(&stale.id));
    let left = store
        .list_backups(Some(&scoped_ns("prune-ns")))
        .await
        .unwrap();
    assert_eq!(left.len(), 2, "最新两份（含最新一份）不得被 TTL 删除");
}

#[tokio::test]
async fn test_replace_creates_safety_backup_and_rebuilds() {
    let (svc, store) = setup();
    let a = admin("agent-a");
    store
        .ensure_namespace(&scoped_ns("ns"), &"agent-a".to_string())
        .await
        .unwrap();
    store
        .grant(&NamespaceGrantRecord {
            namespace: scoped_ns("ns"),
            agent_id: "agent-a".to_string(),
            can_read: true,
            can_write: true,
            granted_by: "agent-a".to_string(),
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    store.seed(&scoped_ns("ns"), "agent-a", "旧内容一");
    store.seed(&scoped_ns("ns"), "agent-a", "旧内容二");

    let backup = svc
        .create(
            &a,
            BackupCreateRequest {
                namespace: "ns".to_string(),
                note: None,
            },
        )
        .await
        .unwrap();

    // 删掉一条、改写一条，再 replace 恢复
    let items = store
        .all_in_namespace(&scoped_ns("ns"), true)
        .await
        .unwrap();
    store.delete(&scoped_ns("ns"), items[0].id).await.unwrap();
    let mut kept = store.get(&scoped_ns("ns"), items[1].id).await.unwrap();
    kept.content = "被改坏了".to_string();
    store.save(&kept).await.unwrap();

    let report = svc
        .restore(
            &a,
            RestoreRequest {
                backup_id: backup.id,
                mode: RestoreMode::Replace,
            },
        )
        .await
        .unwrap();
    assert!(
        report.safety_backup_id.is_some(),
        "replace 前必须自动创建安全备份"
    );
    // replace 先清空当前状态再按快照重建，两条都应计为 created
    assert_eq!(report.created, 2, "replace 后两条都应按快照重建");

    let after = store
        .all_in_namespace(&scoped_ns("ns"), true)
        .await
        .unwrap();
    assert_eq!(after.len(), 2);
    assert!(after.iter().any(|e| e.content == "旧内容一"));
    assert!(after.iter().any(|e| e.content == "旧内容二"));
}

#[tokio::test]
async fn test_safety_backup_can_recover_pre_replace_state() {
    let (svc, store) = setup();
    let a = admin("agent-a");
    store
        .ensure_namespace(&scoped_ns("ns"), &"agent-a".to_string())
        .await
        .unwrap();
    store
        .grant(&NamespaceGrantRecord {
            namespace: scoped_ns("ns"),
            agent_id: "agent-a".to_string(),
            can_read: true,
            can_write: true,
            granted_by: "agent-a".to_string(),
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();

    // 初始状态：版本 A
    store.seed(&scoped_ns("ns"), "agent-a", "原始数据 A");
    let backup_v1 = svc
        .create(
            &a,
            BackupCreateRequest {
                namespace: "ns".to_string(),
                note: Some("v1".to_string()),
            },
        )
        .await
        .unwrap();

    // 后续改动为版本 B
    let all = store
        .all_in_namespace(&scoped_ns("ns"), true)
        .await
        .unwrap();
    let mut cur = all[0].clone();
    cur.content = "被修改后的数据 B".to_string();
    store.save(&cur).await.unwrap();

    // 执行 replace 恢复至 v1，系统必须自动创建安全备份 (保存了数据 B)
    let replace_report = svc
        .restore(
            &a,
            RestoreRequest {
                backup_id: backup_v1.id,
                mode: RestoreMode::Replace,
            },
        )
        .await
        .unwrap();
    let safety_id = replace_report.safety_backup_id.expect("必须生成安全备份");

    // 验证当前已被 v1 替换
    let items_v1 = store
        .all_in_namespace(&scoped_ns("ns"), true)
        .await
        .unwrap();
    assert_eq!(items_v1[0].content, "原始数据 A");

    // 验证：使用 safety_backup_id 恢复可成功找回数据 B
    let rollback_report = svc
        .restore(
            &a,
            RestoreRequest {
                backup_id: safety_id,
                mode: RestoreMode::Replace,
            },
        )
        .await
        .unwrap();
    assert_eq!(rollback_report.total, 1);
    let items_rollback = store
        .all_in_namespace(&scoped_ns("ns"), true)
        .await
        .unwrap();
    assert_eq!(items_rollback[0].content, "被修改后的数据 B");
}

#[tokio::test]
async fn test_restore_detects_corrupted_checksum() {
    let (svc, store) = setup();
    let a = admin("agent-a");
    store
        .ensure_namespace(&scoped_ns("ns"), &"agent-a".to_string())
        .await
        .unwrap();
    store
        .grant(&NamespaceGrantRecord {
            namespace: scoped_ns("ns"),
            agent_id: "agent-a".to_string(),
            can_read: true,
            can_write: true,
            granted_by: "agent-a".to_string(),
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    store.seed(&scoped_ns("ns"), "agent-a", "记忆");

    let backup = svc
        .create(
            &a,
            BackupCreateRequest {
                namespace: "ns".to_string(),
                note: None,
            },
        )
        .await
        .unwrap();

    // 篡改负载内容（校验和不变 → 恢复必须报错）
    let mut tampered = backup.clone();
    // 存储层保存的是带账户前缀的内部键（服务层写入必经 scoped），
    // 直接操作裸存储时需自行还原该形态。
    tampered.namespace = scoped_ns("ns");
    tampered.payload = serde_json::json!({
        "version": BACKUP_PAYLOAD_VERSION,
        "namespace": "ns",
        "entries": [],
        "evidences": [],
        "relations": [],
    });
    store.save_backup(&tampered).await.unwrap();

    let err = svc
        .restore(
            &a,
            RestoreRequest {
                backup_id: backup.id,
                mode: RestoreMode::DryRun,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, HubError::Internal(_)));
}

#[tokio::test]
async fn test_restore_rejects_unsupported_payload_version() {
    let (svc, store) = setup();
    let auth = admin("agent-a");
    store
        .ensure_namespace(&scoped_ns("future-version"), &"agent-a".to_string())
        .await
        .unwrap();
    store
        .grant(&NamespaceGrantRecord {
            namespace: scoped_ns("future-version"),
            agent_id: "agent-a".to_string(),
            can_read: true,
            can_write: true,
            granted_by: "agent-a".to_string(),
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    let entry = store.seed(&scoped_ns("future-version"), "agent-a", "future format");
    let backup = svc
        .create(
            &auth,
            BackupCreateRequest {
                namespace: "future-version".to_string(),
                note: None,
            },
        )
        .await
        .unwrap();
    let mut unsupported = backup.clone();
    unsupported.namespace = scoped_ns("future-version");
    unsupported.payload["version"] = serde_json::json!(99);
    unsupported.checksum = DefaultBackupService::checksum_v1(&[{
        let mut entry = entry;
        entry.namespace = "future-version".to_string();
        entry
    }]);
    store.save_backup(&unsupported).await.unwrap();

    let error = svc
        .restore(
            &auth,
            RestoreRequest {
                backup_id: unsupported.id,
                mode: RestoreMode::DryRun,
            },
        )
        .await
        .expect_err("unsupported payload version must fail closed");
    assert!(matches!(error, HubError::Internal(_)));
}

#[tokio::test]
async fn test_prune_keeps_latest_n() {
    let (svc, store) = setup();
    let a = admin("agent-a");
    store
        .ensure_namespace(&scoped_ns("ns"), &"agent-a".to_string())
        .await
        .unwrap();
    store
        .grant(&NamespaceGrantRecord {
            namespace: scoped_ns("ns"),
            agent_id: "agent-a".to_string(),
            can_read: true,
            can_write: true,
            granted_by: "agent-a".to_string(),
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    store.seed(&scoped_ns("ns"), "agent-a", "记忆");

    let mut ids = Vec::new();
    for i in 0..3 {
        let b = svc
            .create(
                &a,
                BackupCreateRequest {
                    namespace: "ns".to_string(),
                    note: Some(format!("第 {} 份", i)),
                },
            )
            .await
            .unwrap();
        ids.push(b.id);
        // 让创建时间拉开差距
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    let report = svc.prune(1, 0).await.unwrap();
    assert_eq!(report.deleted.len(), 2, "保留 1 份应删除 2 份");
    // 最新的一份必须还在
    let remaining = store.list_backups(None).await.unwrap();
    assert_eq!(remaining.len(), 1);
}

#[tokio::test]
async fn test_list_backup_requires_read_access() {
    let (svc, store) = setup();
    let a = admin("agent-a");
    let stranger = AuthCtx {
        agent_id: "agent-b".to_string(),
        account_id: common::BOOTSTRAP_ACCOUNT_ID,
        scopes: vec![Scope::Read, Scope::Write, Scope::Admin],
        acl: Default::default(),
    };

    store
        .ensure_namespace(&scoped_ns("ns"), &"agent-a".to_string())
        .await
        .unwrap();
    store
        .grant(&NamespaceGrantRecord {
            namespace: scoped_ns("ns"),
            agent_id: "agent-a".to_string(),
            can_read: true,
            can_write: true,
            granted_by: "agent-a".to_string(),
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    store.seed(&scoped_ns("ns"), "agent-a", "记忆");
    svc.create(
        &a,
        BackupCreateRequest {
            namespace: "ns".to_string(),
            note: None,
        },
    )
    .await
    .unwrap();

    // 陌生人即使有全部 scope 也看不到别人的命名空间备份
    let err = svc.list(&stranger, Some("ns")).await.unwrap_err();
    assert!(matches!(err, HubError::InsufficientScope));
}

#[tokio::test]
async fn test_wildcard_backup_visible_to_admin_not_to_reader() {
    let (svc, store) = setup();
    let a = admin("agent-a");
    store
        .ensure_namespace(&scoped_ns("ns"), &"agent-a".to_string())
        .await
        .unwrap();
    store
        .grant(&NamespaceGrantRecord {
            namespace: scoped_ns("ns"),
            agent_id: "agent-a".to_string(),
            can_read: true,
            can_write: true,
            granted_by: "agent-a".to_string(),
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    store.seed(&scoped_ns("ns"), "agent-a", "记忆");

    svc.create(
        &a,
        BackupCreateRequest {
            namespace: "*".to_string(),
            note: None,
        },
    )
    .await
    .unwrap();

    // 全量快照的 `*` 不是授权对象：过去按命名空间判权会让整次列举失败
    let listed = svc
        .list(&a, None)
        .await
        .expect("属主应能列举含全量快照的备份");
    assert!(listed.iter().any(|b| b.namespace == "*"));

    // 无 admin 作用域者不得看到全量快照
    let reader = AuthCtx {
        agent_id: "agent-a".to_string(),
        account_id: common::BOOTSTRAP_ACCOUNT_ID,
        scopes: vec![Scope::Read],
        acl: Default::default(),
    };
    let err = svc.list(&reader, None).await.unwrap_err();
    assert!(matches!(err, HubError::InsufficientScope));
}

#[tokio::test]
async fn test_backup_requires_admin_scope() {
    let (svc, _store) = setup();
    let no_admin = AuthCtx {
        agent_id: "agent-a".to_string(),
        account_id: common::BOOTSTRAP_ACCOUNT_ID,
        scopes: vec![Scope::Read, Scope::Write],
        acl: Default::default(),
    };
    let err = svc
        .create(
            &no_admin,
            BackupCreateRequest {
                namespace: "ns".to_string(),
                note: None,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, HubError::InsufficientScope));
}

#[tokio::test]
async fn test_v1_to_v4_historical_sample_restore_and_checksum_verification() {
    let (svc, store) = setup();
    let a = admin("admin-agent");
    let ns = "hist-ns";

    store
        .ensure_namespace(&scoped_ns(ns), &"admin-agent".to_string())
        .await
        .unwrap();
    store
        .grant(&NamespaceGrantRecord {
            namespace: scoped_ns(ns),
            agent_id: "admin-agent".to_string(),
            can_read: true,
            can_write: true,
            granted_by: "admin-agent".to_string(),
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();

    // 1. 构造历史 v1 备份样本 (只有 entries)
    let mut entry_v1 = store.seed(&scoped_ns(ns), "admin-agent", "历史 v1 记忆事实");
    entry_v1.namespace = ns.to_string();
    let v1_checksum = DefaultBackupService::checksum_v1(&[entry_v1.clone()]);
    let v1_payload = serde_json::json!({
        "version": 1,
        "entries": [entry_v1.clone()]
    });
    let backup_v1 = BackupRecord {
        id: Uuid::new_v4(),
        namespace: scoped_ns(ns),
        created_by: "admin-agent".to_string(),
        note: Some("v1-legacy".to_string()),
        entry_count: 1,
        checksum: v1_checksum,
        payload: v1_payload,
        retain_until: None,
        created_at: chrono::Utc::now(),
    };
    store.save_backup(&backup_v1).await.unwrap();

    // DryRun 验证
    let rep_v1_dry = svc
        .restore(
            &a,
            RestoreRequest {
                backup_id: backup_v1.id,
                mode: RestoreMode::DryRun,
            },
        )
        .await
        .unwrap();
    assert_eq!(rep_v1_dry.mode, RestoreMode::DryRun);
    assert_eq!(rep_v1_dry.total, 1);

    // 2. 构造历史 v2 备份样本 (entries + evidences + relations)
    let ev_v2 = MemoryEvidence {
        id: Uuid::new_v4(),
        namespace: ns.to_string(),
        source_kind: EvidenceSourceKind::Session,
        source_ref: Some("msg-001".to_string()),
        content_hash: "hash001".to_string(),
        excerpt: Some("用户询问架构配置".to_string()),
        occurred_at: Some(chrono::Utc::now()),
        created_at: chrono::Utc::now(),
        expires_at: None,
    };
    let rel_v2 = MemoryRelation {
        namespace: Some(ns.to_string()),
        from_memory_id: entry_v1.id,
        to_memory_id: entry_v1.id,
        relation_kind: MemoryRelationKind::DerivedFrom,
        confidence: 0.95,
        source_evidence_id: Some(ev_v2.id),
        created_at: chrono::Utc::now(),
    };
    let v2_checksum = DefaultBackupService::checksum_v2(
        std::slice::from_ref(&entry_v1),
        std::slice::from_ref(&ev_v2),
        std::slice::from_ref(&rel_v2),
    );
    let v2_payload = serde_json::json!({
        "version": 2,
        "entries": [entry_v1],
        "evidences": [ev_v2],
        "relations": [rel_v2],
    });
    let backup_v2 = BackupRecord {
        id: Uuid::new_v4(),
        namespace: scoped_ns(ns),
        created_by: "admin-agent".to_string(),
        note: Some("v2-legacy".to_string()),
        entry_count: 1,
        checksum: v2_checksum,
        payload: v2_payload,
        retain_until: None,
        created_at: chrono::Utc::now(),
    };
    store.save_backup(&backup_v2).await.unwrap();

    // 3. 构造历史 v3 备份样本 (v2 + tasks + experiences + task_experiences)
    let task_v3 = MemoryTask {
        id: Uuid::new_v4(),
        namespace: ns.to_string(),
        scene: crate::domain::MemoryScene::Working,
        task: "审计历史备份恢复".to_string(),
        status: TaskStatus::Completed,
        started_at: chrono::Utc::now(),
        finished_at: Some(chrono::Utc::now()),
        result_summary: Some("审计通过".to_string()),
        failure_reason: None,
        source_evidence_id: None,
        metadata: serde_json::json!({}),
    };
    let exp_v3 = MemoryExperience {
        id: Uuid::new_v4(),
        namespace: ns.to_string(),
        task: "备份校验".to_string(),
        search_keys: vec!["backup".to_string()],
        solution: "按版本计算校验和".to_string(),
        preconditions: serde_json::json!([]),
        boundary_envs: vec![],
        failure_modes: vec![],
        validation: serde_json::json!("成功"),
        confidence: 1.0,
        status: ExperienceStatus::Active,
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
    let te_v3 = MemoryTaskExperience {
        task_id: task_v3.id,
        experience_id: exp_v3.id,
        namespace: Some(ns.to_string()),
        relation_kind: "derived_from".to_string(),
        created_at: chrono::Utc::now(),
    };
    let v3_checksum = DefaultBackupService::checksum_v3(
        &[],
        &[],
        &[],
        std::slice::from_ref(&task_v3),
        std::slice::from_ref(&exp_v3),
        std::slice::from_ref(&te_v3),
    );
    let v3_payload = serde_json::json!({
        "version": 3,
        "entries": [],
        "tasks": [task_v3],
        "experiences": [exp_v3],
        "task_experiences": [te_v3],
    });
    let backup_v3 = BackupRecord {
        id: Uuid::new_v4(),
        namespace: scoped_ns(ns),
        created_by: "admin-agent".to_string(),
        note: Some("v3-legacy".to_string()),
        entry_count: 0,
        checksum: v3_checksum,
        payload: v3_payload,
        retain_until: None,
        created_at: chrono::Utc::now(),
    };
    store.save_backup(&backup_v3).await.unwrap();

    // 4. 恢复 v3 备份（Merge 模式）
    let rep_v3 = svc
        .restore(
            &a,
            RestoreRequest {
                backup_id: backup_v3.id,
                mode: RestoreMode::Merge,
            },
        )
        .await
        .unwrap();
    assert_eq!(rep_v3.mode, RestoreMode::Merge);

    // 验证 task 和 experience 成功恢复
    let recovered_task = store.get_task(&scoped_ns(ns), task_v3.id).await.unwrap();
    assert!(recovered_task.is_some());
    assert_eq!(recovered_task.unwrap().task, "审计历史备份恢复");

    let recovered_exp = store
        .get_experience(&scoped_ns(ns), exp_v3.id)
        .await
        .unwrap();
    assert!(recovered_exp.is_some());
    assert_eq!(recovered_exp.unwrap().solution, "按版本计算校验和");
}

#[tokio::test]
async fn test_backup_replace_purge_safety_backup_and_rollback() {
    let (svc, store) = setup();
    let a = admin("admin-agent");
    let ns = "replace-ns";

    store
        .ensure_namespace(&scoped_ns(ns), &"admin-agent".to_string())
        .await
        .unwrap();
    store
        .grant(&NamespaceGrantRecord {
            namespace: scoped_ns(ns),
            agent_id: "admin-agent".to_string(),
            can_read: true,
            can_write: true,
            granted_by: "admin-agent".to_string(),
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();

    // 阶段 1：初始状态，写入一条记忆与任务
    let initial_entry = store.seed(&scoped_ns(ns), "admin-agent", "初始重要数据");
    let initial_task = store
        .create_task(&NewMemoryTask {
            id: None,
            namespace: scoped_ns(ns),
            scene: crate::domain::MemoryScene::Working,
            task: "初始任务".to_string(),
            status: TaskStatus::Running,
            started_at: chrono::Utc::now(),
            finished_at: None,
            result_summary: None,
            failure_reason: None,
            source_evidence_id: None,
            metadata: serde_json::json!({}),
        })
        .await
        .unwrap();

    // 阶段 2：准备一份快照（仅包含新条目）
    let new_entry_id = Uuid::new_v4();
    let new_entry = MemoryEntry {
        id: new_entry_id,
        namespace: ns.to_string(),
        key: None,
        kind: MemoryKind::Fact,
        content: "全新的快照数据".to_string(),
        tags: vec![],
        metadata: serde_json::json!({}),
        embedding: None,
        embedding_model: None,
        source_agent: "admin-agent".to_string(),
        confidence: 1.0,
        status: MemoryStatus::Active,
        version: 1,
        expires_at: None,
        merged_into: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        scene: None,
        layer: None,
        valid_from: None,
        valid_until: None,
        occurred_at: None,
    };
    let checksum = DefaultBackupService::checksum_v1(std::slice::from_ref(&new_entry));
    let snapshot_backup = BackupRecord {
        id: Uuid::new_v4(),
        namespace: scoped_ns(ns),
        created_by: "admin-agent".to_string(),
        note: Some("snapshot-to-replace".to_string()),
        entry_count: 1,
        checksum,
        payload: serde_json::json!({
            "version": 1,
            "entries": [new_entry]
        }),
        retain_until: None,
        created_at: chrono::Utc::now(),
    };
    store.save_backup(&snapshot_backup).await.unwrap();

    // 阶段 3：执行 Replace 恢复
    let rep = svc
        .restore(
            &a,
            RestoreRequest {
                backup_id: snapshot_backup.id,
                mode: RestoreMode::Replace,
            },
        )
        .await
        .unwrap();

    // 验证生成了 safety backup
    assert!(rep.safety_backup_id.is_some());
    let safety_id = rep.safety_backup_id.unwrap();

    // 验证旧数据已被彻底清除
    assert!(store.get(&scoped_ns(ns), initial_entry.id).await.is_err());
    assert!(store
        .get_task(&scoped_ns(ns), initial_task.id)
        .await
        .unwrap()
        .is_none());

    // 验证新数据已生效
    let cur = store.get(&scoped_ns(ns), new_entry_id).await.unwrap();
    assert_eq!(cur.content, "全新的快照数据");

    // 阶段 4：模拟故障回滚，通过 safety backup 再次执行 Replace 恢复！
    let rollback_rep = svc
        .restore(
            &a,
            RestoreRequest {
                backup_id: safety_id,
                mode: RestoreMode::Replace,
            },
        )
        .await
        .unwrap();
    assert_eq!(rollback_rep.mode, RestoreMode::Replace);

    // 验证回滚后，初始数据完全复原！
    let restored_initial = store.get(&scoped_ns(ns), initial_entry.id).await.unwrap();
    assert_eq!(restored_initial.content, "初始重要数据");
    let restored_task = store
        .get_task(&scoped_ns(ns), initial_task.id)
        .await
        .unwrap();
    assert!(restored_task.is_some());
    assert_eq!(restored_task.unwrap().task, "初始任务");

    // 验证被替换的新数据已不存在
    assert!(store.get(&scoped_ns(ns), new_entry_id).await.is_err());
}

#[tokio::test]
async fn test_corrupted_checksum_fails_restore() {
    let (svc, store) = setup();
    let a = admin("admin-agent");
    let ns = "corrupt-ns";

    store
        .ensure_namespace(&scoped_ns(ns), &"admin-agent".to_string())
        .await
        .unwrap();
    store
        .grant(&NamespaceGrantRecord {
            namespace: scoped_ns(ns),
            agent_id: "admin-agent".to_string(),
            can_read: true,
            can_write: true,
            granted_by: "admin-agent".to_string(),
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();

    let corrupted_backup = BackupRecord {
        id: Uuid::new_v4(),
        namespace: scoped_ns(ns),
        created_by: "admin-agent".to_string(),
        note: Some("corrupted".to_string()),
        entry_count: 1,
        checksum: "invalid_tampered_checksum".to_string(),
        payload: serde_json::json!({
            "version": 1,
            "entries": []
        }),
        retain_until: None,
        created_at: chrono::Utc::now(),
    };
    store.save_backup(&corrupted_backup).await.unwrap();

    let err = svc
        .restore(
            &a,
            RestoreRequest {
                backup_id: corrupted_backup.id,
                mode: RestoreMode::Merge,
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, HubError::Internal(_)),
        "校验和篡改必须直接拒绝"
    );
}

#[tokio::test]
async fn test_restore_counts_partial_failures_and_continues() {
    // 替身 store：第一条 relation 的 add_relation 返回 Err，
    // 其余操作全部透传内层存储——验证恢复不中止、失败计数准确。
    #[derive(Default)]
    struct FailingRelationStore {
        inner: crate::testing::TestHubStore,
        fail_relation_payloads: std::sync::Mutex<std::collections::HashSet<String>>,
        fail_delete_ids: std::sync::Mutex<std::collections::HashSet<Uuid>>,
    }

    #[async_trait]
    impl MemoryStore for FailingRelationStore {
        async fn insert(&self, e: &MemoryEntry) -> Result<MemoryEntry> {
            self.inner.insert(e).await
        }
        async fn insert_initial(&self, e: &MemoryEntry) -> Result<MemoryEntry> {
            self.inner.insert_initial(e).await
        }
        async fn get(&self, ns: &str, id: Uuid) -> Result<MemoryEntry> {
            self.inner.get(ns, id).await
        }
        async fn get_by_key(&self, ns: &str, key: &str) -> Result<Option<MemoryEntry>> {
            self.inner.get_by_key(ns, key).await
        }
        async fn save(&self, e: &MemoryEntry) -> Result<MemoryEntry> {
            self.inner.save(e).await
        }
        async fn delete(&self, ns: &str, id: Uuid) -> Result<()> {
            if self.fail_delete_ids.lock().unwrap().contains(&id) {
                return Err(HubError::Unavailable);
            }
            self.inner.delete(ns, id).await
        }
        async fn list(&self, filter: &MemoryFilter) -> Result<Vec<MemoryEntry>> {
            self.inner.list(filter).await
        }
        async fn count(&self, filter: &MemoryFilter) -> Result<usize> {
            self.inner.count(filter).await
        }
        async fn similar(
            &self,
            filter: &MemoryFilter,
            qv: &[f32],
            k: usize,
        ) -> Result<Vec<crate::domain::Scored<MemoryEntry>>> {
            self.inner.similar(filter, qv, k).await
        }
        async fn embeddings_by_ids(
            &self,
            namespace: &str,
            ids: &[Uuid],
        ) -> Result<Vec<(Uuid, Vec<f32>)>> {
            self.inner.embeddings_by_ids(namespace, ids).await
        }
        async fn all_in_namespace(
            &self,
            ns: &str,
            include_merged: bool,
        ) -> Result<Vec<MemoryEntry>> {
            self.inner.all_in_namespace(ns, include_merged).await
        }
        async fn list_namespaces(&self) -> Result<Vec<String>> {
            self.inner.list_namespaces().await
        }
        async fn record_version(&self, v: &MemoryVersion) -> Result<()> {
            self.inner.record_version(v).await
        }
        async fn versions(&self, id: Uuid) -> Result<Vec<MemoryVersion>> {
            self.inner.versions(id).await
        }
        async fn archive_expired(&self, _now: chrono::DateTime<chrono::Utc>) -> Result<usize> {
            Ok(0)
        }
        async fn ping(&self) -> Result<()> {
            self.inner.ping().await
        }
        async fn save_if_version(
            &self,
            entry: &MemoryEntry,
            expected_version: i64,
        ) -> Result<MemoryEntry> {
            self.inner.save_if_version(entry, expected_version).await
        }
        async fn revise(
            &self,
            entry: &MemoryEntry,
            expected_version: i64,
            change: VersionChange,
        ) -> Result<MemoryEntry> {
            self.inner.revise(entry, expected_version, change).await
        }
    }

    #[async_trait]
    impl crate::infra::BackupStore for FailingRelationStore {
        async fn save_backup(&self, b: &BackupRecord) -> Result<()> {
            self.inner.save_backup(b).await
        }
        async fn get_backup(&self, id: Uuid) -> Result<BackupRecord> {
            self.inner.get_backup(id).await
        }
        async fn list_backups(&self, ns: Option<&str>) -> Result<Vec<BackupRecord>> {
            self.inner.list_backups(ns).await
        }
        async fn delete_backup(&self, id: Uuid) -> Result<()> {
            self.inner.delete_backup(id).await
        }

        async fn replace_namespaces_from_snapshots(
            &self,
            snapshots: &[(String, crate::domain::RestoreSnapshot)],
        ) -> Result<()> {
            let failing = self.fail_delete_ids.lock().unwrap().clone();
            if snapshots
                .iter()
                .flat_map(|(_, snapshot)| snapshot.entries.iter())
                .any(|entry| failing.contains(&entry.id))
            {
                return Err(HubError::Unavailable);
            }
            self.inner
                .replace_namespaces_from_snapshots(snapshots)
                .await
        }
    }

    #[async_trait]
    impl crate::infra::IntegrationStore for FailingRelationStore {
        async fn save_proposal(&self, p: &IntegrationProposal) -> Result<()> {
            self.inner.save_proposal(p).await
        }
        async fn get_proposal(&self, id: Uuid) -> Result<IntegrationProposal> {
            self.inner.get_proposal(id).await
        }
        async fn list_proposals(
            &self,
            ns: &str,
            status: Option<ProposalStatus>,
        ) -> Result<Vec<IntegrationProposal>> {
            self.inner.list_proposals(ns, status).await
        }
        async fn open_exists(&self, ns: &str, members: &[(String, Vec<Uuid>)]) -> Result<bool> {
            self.inner.open_exists(ns, members).await
        }
    }

    #[async_trait]
    impl crate::infra::CognitiveProposalStore for FailingRelationStore {
        async fn save_cognitive_proposal(
            &self,
            p: &crate::domain::CognitiveProposalRecord,
        ) -> Result<()> {
            self.inner.save_cognitive_proposal(p).await
        }
        async fn get_cognitive_proposal(
            &self,
            id: Uuid,
        ) -> Result<crate::domain::CognitiveProposalRecord> {
            self.inner.get_cognitive_proposal(id).await
        }
        async fn list_cognitive_proposals(
            &self,
            ns: &str,
            status: Option<crate::domain::CognitiveProposalStatus>,
        ) -> Result<Vec<crate::domain::CognitiveProposalRecord>> {
            self.inner.list_cognitive_proposals(ns, status).await
        }
        async fn update_cognitive_proposal_status(
            &self,
            id: Uuid,
            expected_status: crate::domain::CognitiveProposalStatus,
            new_status: crate::domain::CognitiveProposalStatus,
            rejection_reason: Option<&str>,
            resulting_memory_id: Option<Uuid>,
            resolved_by: Option<&str>,
        ) -> Result<crate::domain::CognitiveProposalRecord> {
            self.inner
                .update_cognitive_proposal_status(
                    id,
                    expected_status,
                    new_status,
                    rejection_reason,
                    resulting_memory_id,
                    resolved_by,
                )
                .await
        }
    }

    #[async_trait]
    impl crate::infra::GrantStore for FailingRelationStore {
        async fn ensure_namespace(
            &self,
            ns: &str,
            owner: &AgentId,
        ) -> Result<(NamespaceInfo, bool)> {
            self.inner.ensure_namespace(ns, owner).await
        }
        async fn get_namespace(&self, ns: &str) -> Result<Option<NamespaceInfo>> {
            self.inner.get_namespace(ns).await
        }
        async fn list_namespace_infos(&self) -> Result<Vec<NamespaceInfo>> {
            self.inner.list_namespace_infos().await
        }
        async fn grant(&self, r: &NamespaceGrantRecord) -> Result<()> {
            self.inner.grant(r).await
        }
        async fn revoke(&self, ns: &str, agent: &AgentId) -> Result<()> {
            self.inner.revoke(ns, agent).await
        }
        async fn grants_for(&self, agent: &AgentId) -> Result<Vec<NamespaceGrantRecord>> {
            self.inner.grants_for(agent).await
        }
        async fn can_read(&self, ns: &str, agent: &AgentId) -> Result<bool> {
            self.inner.can_read(ns, agent).await
        }
        async fn can_write(&self, ns: &str, agent: &AgentId) -> Result<bool> {
            self.inner.can_write(ns, agent).await
        }
        async fn owned_namespaces(&self, agent: &AgentId) -> Result<Vec<String>> {
            self.inner.owned_namespaces(agent).await
        }
    }

    #[async_trait]
    impl crate::infra::AgentStore for FailingRelationStore {
        async fn register_agent(
            &self,
            reg: &crate::infra::AgentRegistration,
        ) -> Result<crate::infra::AgentRecord> {
            self.inner.register_agent(reg).await
        }
        async fn get_agent(&self, agent_id: &AgentId) -> Result<Option<crate::infra::AgentRecord>> {
            self.inner.get_agent(agent_id).await
        }
        async fn list_agents(&self) -> Result<Vec<crate::infra::AgentRecord>> {
            self.inner.list_agents().await
        }
        async fn set_agent_status(&self, agent_id: &AgentId, status: &str) -> Result<()> {
            self.inner.set_agent_status(agent_id, status).await
        }
        async fn verify_agent_secret(&self, agent_id: &AgentId, secret: &str) -> Result<bool> {
            self.inner.verify_agent_secret(agent_id, secret).await
        }
    }

    #[async_trait]
    impl EvidenceStore for FailingRelationStore {
        async fn append_evidence(&self, ev: &NewEvidence) -> Result<MemoryEvidence> {
            self.inner.append_evidence(ev).await
        }
        async fn get_evidence(&self, ns: &str, id: Uuid) -> Result<MemoryEvidence> {
            self.inner.get_evidence(ns, id).await
        }
        async fn list_evidence(
            &self,
            ns: &str,
            source_kind: Option<EvidenceSourceKind>,
            source_ref: Option<&str>,
            limit: usize,
        ) -> Result<Vec<MemoryEvidence>> {
            self.inner
                .list_evidence(ns, source_kind, source_ref, limit)
                .await
        }
        async fn all_evidence_in_namespace(&self, ns: &str) -> Result<Vec<MemoryEvidence>> {
            self.inner.all_evidence_in_namespace(ns).await
        }
        async fn delete_evidence(&self, ns: &str, id: Uuid) -> Result<()> {
            self.inner.delete_evidence(ns, id).await
        }
        async fn purge_expired_evidence(
            &self,
            ns: &str,
            now: chrono::DateTime<chrono::Utc>,
        ) -> Result<usize> {
            self.inner.purge_expired_evidence(ns, now).await
        }
    }

    #[async_trait]
    impl RelationStore for FailingRelationStore {
        async fn add_relation(&self, ns: &str, rel: &NewMemoryRelation) -> Result<MemoryRelation> {
            let key = format!(
                "{}|{}|{}|{:?}",
                ns, rel.from_memory_id, rel.to_memory_id, rel.relation_kind
            );
            if self.fail_relation_payloads.lock().unwrap().contains(&key) {
                return Err(HubError::Unavailable);
            }
            self.inner.add_relation(ns, rel).await
        }
        async fn list_relations_from(
            &self,
            ns: &str,
            from_memory_id: Uuid,
        ) -> Result<Vec<MemoryRelation>> {
            self.inner.list_relations_from(ns, from_memory_id).await
        }
        async fn list_relations_to(
            &self,
            ns: &str,
            to_memory_id: Uuid,
        ) -> Result<Vec<MemoryRelation>> {
            self.inner.list_relations_to(ns, to_memory_id).await
        }
        async fn delete_relation(
            &self,
            ns: &str,
            from_memory_id: Uuid,
            to_memory_id: Uuid,
            relation_kind: MemoryRelationKind,
        ) -> Result<()> {
            self.inner
                .delete_relation(ns, from_memory_id, to_memory_id, relation_kind)
                .await
        }
        async fn all_relations_in_namespace(&self, ns: &str) -> Result<Vec<MemoryRelation>> {
            self.inner.all_relations_in_namespace(ns).await
        }
    }

    #[async_trait]
    impl TaskStore for FailingRelationStore {
        async fn create_task(&self, task: &NewMemoryTask) -> Result<MemoryTask> {
            self.inner.create_task(task).await
        }
        async fn update_task(&self, task: &MemoryTask) -> Result<MemoryTask> {
            self.inner.update_task(task).await
        }
        async fn get_task(&self, namespace: &str, id: Uuid) -> Result<Option<MemoryTask>> {
            self.inner.get_task(namespace, id).await
        }
        async fn list_tasks(
            &self,
            namespace: &str,
            status: Option<TaskStatus>,
            limit: usize,
        ) -> Result<Vec<MemoryTask>> {
            self.inner.list_tasks(namespace, status, limit).await
        }
        async fn all_tasks_in_namespace(&self, namespace: &str) -> Result<Vec<MemoryTask>> {
            self.inner.all_tasks_in_namespace(namespace).await
        }
        async fn delete_task(&self, namespace: &str, id: Uuid) -> Result<()> {
            self.inner.delete_task(namespace, id).await
        }
    }

    #[async_trait]
    impl ExperienceStore for FailingRelationStore {
        async fn save_experience(
            &self,
            experience: &NewMemoryExperience,
        ) -> Result<MemoryExperience> {
            self.inner.save_experience(experience).await
        }
        async fn update_experience(
            &self,
            experience: &MemoryExperience,
        ) -> Result<MemoryExperience> {
            self.inner.update_experience(experience).await
        }
        async fn get_experience(
            &self,
            namespace: &str,
            id: Uuid,
        ) -> Result<Option<MemoryExperience>> {
            self.inner.get_experience(namespace, id).await
        }
        async fn list_experiences(
            &self,
            namespace: &str,
            status: Option<ExperienceStatus>,
            limit: usize,
            offset: usize,
        ) -> Result<Vec<MemoryExperience>> {
            self.inner
                .list_experiences(namespace, status, limit, offset)
                .await
        }
        async fn link_task_experience(&self, link: &MemoryTaskExperience) -> Result<()> {
            self.inner.link_task_experience(link).await
        }
        async fn list_experiences_for_task(
            &self,
            task_id: Uuid,
        ) -> Result<Vec<MemoryTaskExperience>> {
            self.inner.list_experiences_for_task(task_id).await
        }
        async fn list_links_for_experience(
            &self,
            namespace: &str,
            experience_id: Uuid,
        ) -> Result<Vec<MemoryTaskExperience>> {
            self.inner
                .list_links_for_experience(namespace, experience_id)
                .await
        }
        async fn update_experience_if_version(
            &self,
            experience: &MemoryExperience,
            expected_version: i64,
        ) -> Result<MemoryExperience> {
            self.inner
                .update_experience_if_version(experience, expected_version)
                .await
        }
        async fn append_experience_version(
            &self,
            version: &crate::domain::NewExperienceVersion,
        ) -> Result<crate::domain::ExperienceVersion> {
            self.inner.append_experience_version(version).await
        }
        async fn list_experience_versions(
            &self,
            namespace: &str,
            experience_id: Uuid,
            limit: usize,
        ) -> Result<Vec<crate::domain::ExperienceVersion>> {
            self.inner
                .list_experience_versions(namespace, experience_id, limit)
                .await
        }
        async fn all_experience_versions_in_namespace(
            &self,
            namespace: &str,
        ) -> Result<Vec<crate::domain::ExperienceVersion>> {
            self.inner
                .all_experience_versions_in_namespace(namespace)
                .await
        }
        async fn all_experiences_in_namespace(
            &self,
            namespace: &str,
        ) -> Result<Vec<MemoryExperience>> {
            self.inner.all_experiences_in_namespace(namespace).await
        }
        async fn all_task_experiences_in_namespace(
            &self,
            namespace: &str,
        ) -> Result<Vec<MemoryTaskExperience>> {
            self.inner
                .all_task_experiences_in_namespace(namespace)
                .await
        }
        async fn delete_experience(&self, namespace: &str, id: Uuid) -> Result<()> {
            self.inner.delete_experience(namespace, id).await
        }
    }

    #[async_trait]
    impl crate::infra::CollectionStore for FailingRelationStore {
        async fn upsert_collection(
            &self,
            collection: &crate::domain::MemoryCollection,
        ) -> Result<crate::domain::MemoryCollection> {
            self.inner.upsert_collection(collection).await
        }
        async fn get_collection(
            &self,
            account_id: common::AccountId,
            id: Uuid,
        ) -> Result<Option<crate::domain::MemoryCollection>> {
            self.inner.get_collection(account_id, id).await
        }
        async fn get_collection_by_name(
            &self,
            account_id: common::AccountId,
            name: &str,
        ) -> Result<Option<crate::domain::MemoryCollection>> {
            self.inner.get_collection_by_name(account_id, name).await
        }
        async fn list_collections(
            &self,
            account_id: common::AccountId,
        ) -> Result<Vec<crate::domain::MemoryCollection>> {
            self.inner.list_collections(account_id).await
        }
        async fn delete_collection(&self, account_id: common::AccountId, id: Uuid) -> Result<()> {
            self.inner.delete_collection(account_id, id).await
        }
        async fn bind_collection(&self, binding: &crate::domain::CollectionBinding) -> Result<()> {
            self.inner.bind_collection(binding).await
        }
        async fn unbind_collection(&self, collection_id: Uuid, agent_id: &String) -> Result<()> {
            self.inner.unbind_collection(collection_id, agent_id).await
        }
        async fn list_bindings(
            &self,
            collection_id: Uuid,
        ) -> Result<Vec<crate::domain::CollectionBinding>> {
            self.inner.list_bindings(collection_id).await
        }
        async fn is_bound(&self, collection_id: Uuid, agent_id: &String) -> Result<bool> {
            self.inner.is_bound(collection_id, agent_id).await
        }
    }

    #[async_trait]
    impl CodingStore for FailingRelationStore {
        async fn create_thread(&self, thread: &NewCodingThread) -> Result<CodingThread> {
            self.inner.create_thread(thread).await
        }
        async fn get_thread(&self, namespace: &str, id: Uuid) -> Result<Option<CodingThread>> {
            self.inner.get_thread(namespace, id).await
        }
        async fn list_threads(
            &self,
            namespace: &str,
            repository: Option<&str>,
            limit: usize,
        ) -> Result<Vec<CodingThread>> {
            self.inner.list_threads(namespace, repository, limit).await
        }
        async fn save_revision(
            &self,
            revision: &NewCodingRevision,
            diff_hash: &str,
        ) -> Result<CodingRevision> {
            self.inner.save_revision(revision, diff_hash).await
        }
        async fn get_revision(&self, namespace: &str, id: Uuid) -> Result<Option<CodingRevision>> {
            self.inner.get_revision(namespace, id).await
        }
        async fn get_revision_by_hash(
            &self,
            namespace: &str,
            repository: &str,
            diff_hash: &str,
        ) -> Result<Option<CodingRevision>> {
            self.inner
                .get_revision_by_hash(namespace, repository, diff_hash)
                .await
        }
        async fn list_revisions_for_thread(
            &self,
            namespace: &str,
            thread_id: Uuid,
            limit: usize,
        ) -> Result<Vec<CodingRevision>> {
            self.inner
                .list_revisions_for_thread(namespace, thread_id, limit)
                .await
        }
        async fn list_revisions_for_file(
            &self,
            namespace: &str,
            repository: &str,
            file_path: &str,
            limit: usize,
        ) -> Result<Vec<CodingRevision>> {
            self.inner
                .list_revisions_for_file(namespace, repository, file_path, limit)
                .await
        }
        async fn all_threads_in_namespace(&self, namespace: &str) -> Result<Vec<CodingThread>> {
            self.inner.all_threads_in_namespace(namespace).await
        }
        async fn all_revisions_in_namespace(&self, namespace: &str) -> Result<Vec<CodingRevision>> {
            self.inner.all_revisions_in_namespace(namespace).await
        }
        async fn delete_thread(&self, namespace: &str, id: Uuid) -> Result<()> {
            self.inner.delete_thread(namespace, id).await
        }
        async fn delete_revision(&self, namespace: &str, id: Uuid) -> Result<()> {
            self.inner.delete_revision(namespace, id).await
        }
    }

    let inner_store = Arc::new(FailingRelationStore {
        inner: crate::testing::TestHubStore::default(),
        fail_relation_payloads: std::sync::Mutex::new(std::collections::HashSet::new()),
        fail_delete_ids: std::sync::Mutex::new(std::collections::HashSet::new()),
    });
    let svc = DefaultBackupService::new(
        inner_store.clone(),
        Arc::new(GrantChecker::new(inner_store.clone())),
    );
    let a = admin("admin-agent");
    let ns = "partial-fail-ns";

    inner_store
        .inner
        .ensure_namespace(&scoped_ns(ns), &"admin-agent".to_string())
        .await
        .unwrap();
    inner_store
        .inner
        .grant(&NamespaceGrantRecord {
            namespace: scoped_ns(ns),
            agent_id: "admin-agent".to_string(),
            can_read: true,
            can_write: true,
            granted_by: "admin-agent".to_string(),
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();

    // 快照：1 条记忆 + 1 个任务 + 2 条关系（一条将被 store 拒绝）
    // 实体命名空间用用户可见名，与服务层 create 产出的 v4 载荷同构
    let mut entry = inner_store
        .inner
        .seed(&scoped_ns(ns), "admin-agent", "记忆");
    entry.namespace = ns.to_string();
    let task = MemoryTask {
        id: Uuid::new_v4(),
        namespace: ns.to_string(),
        scene: crate::domain::MemoryScene::Working,
        task: "关系失败后仍要恢复的任务".to_string(),
        status: TaskStatus::Completed,
        started_at: chrono::Utc::now(),
        finished_at: Some(chrono::Utc::now()),
        result_summary: None,
        failure_reason: None,
        source_evidence_id: None,
        metadata: serde_json::json!({}),
    };
    let rel_ok = MemoryRelation {
        namespace: Some(ns.to_string()),
        from_memory_id: entry.id,
        to_memory_id: entry.id,
        relation_kind: MemoryRelationKind::DerivedFrom,
        confidence: 0.9,
        source_evidence_id: None,
        created_at: chrono::Utc::now(),
    };
    let rel_fail = MemoryRelation {
        namespace: Some(ns.to_string()),
        from_memory_id: entry.id,
        to_memory_id: entry.id,
        relation_kind: MemoryRelationKind::Supersedes,
        confidence: 0.8,
        source_evidence_id: None,
        created_at: chrono::Utc::now(),
    };
    let checksum = DefaultBackupService::checksum_v4(
        &[entry.clone()],
        &[],
        &[rel_ok.clone(), rel_fail.clone()],
        std::slice::from_ref(&task),
        &[],
        &[],
        &[],
        &[],
    );
    let backup = BackupRecord {
        payload: serde_json::json!({
            "version": BACKUP_PAYLOAD_VERSION,
            "namespace": ns,
            "entries": [entry],
            "evidences": [],
            "relations": [rel_ok, rel_fail],
            "tasks": [task],
            "experiences": [],
            "task_experiences": [],
            "coding_threads": [],
            "coding_revisions": [],
        }),
        checksum,
        ..make_record(scoped_ns(ns))
    };
    inner_store.save_backup(&backup).await.unwrap();

    // 删掉当前记忆，让恢复必须真正写回（而不是 unchanged）
    inner_store
        .inner
        .delete(&scoped_ns(ns), entry.id)
        .await
        .unwrap();

    // 预置失败：指定那条 Supersedes 关系在恢复时被 store 拒绝
    {
        let failing_key = format!(
            "{}|{}|{}|{:?}",
            scoped_ns(ns),
            entry.id,
            entry.id,
            MemoryRelationKind::Supersedes
        );
        inner_store
            .fail_relation_payloads
            .lock()
            .unwrap()
            .insert(failing_key);
    }

    let report = svc
        .restore(
            &a,
            RestoreRequest {
                backup_id: backup.id,
                mode: RestoreMode::Merge,
            },
        )
        .await
        .expect("单条关系失败不得中止恢复");
    assert_eq!(report.failed_relations, 1, "失败的关系应被准确计数");
    assert_eq!(report.failed_evidences, 0);
    assert_eq!(report.created, 1, "记忆条目仍应恢复成功");

    // 成功的那条关系确实恢复了
    let relations = inner_store
        .inner
        .all_relations_in_namespace(&scoped_ns(ns))
        .await
        .unwrap();
    assert_eq!(relations.len(), 1, "另一条关系恢复成功，仅失败那条被跳过");
    assert_eq!(relations[0].relation_kind, MemoryRelationKind::DerivedFrom);

    // 关系失败之后恢复的任务也落库了：证明失败不中止后续实体恢复
    let restored_task = inner_store
        .inner
        .get_task(&scoped_ns(ns), task.id)
        .await
        .unwrap();
    assert!(
        restored_task.is_some(),
        "关系失败后，排在后面的任务实体仍应恢复"
    );

    inner_store.fail_delete_ids.lock().unwrap().insert(entry.id);
    let replace_error = svc
        .restore(
            &a,
            RestoreRequest {
                backup_id: backup.id,
                mode: RestoreMode::Replace,
            },
        )
        .await
        .expect_err("Replace 清理失败不得伪装成恢复成功");
    assert!(matches!(replace_error, HubError::Unavailable));
}

fn make_record(ns: String) -> BackupRecord {
    BackupRecord {
        id: Uuid::new_v4(),
        namespace: ns,
        created_by: "admin-agent".to_string(),
        note: None,
        entry_count: 0,
        checksum: String::new(),
        payload: serde_json::json!({}),
        retain_until: None,
        created_at: chrono::Utc::now(),
    }
}

#[tokio::test]
async fn test_replace_multi_namespace_records_all_safety_backup_ids() {
    let (svc, store) = setup();
    let a = admin("admin-agent");

    // 两个命名空间，各有数据；全量快照 namespace = "*"
    for ns in ["multi-a", "multi-b"] {
        store
            .ensure_namespace(&scoped_ns(ns), &"admin-agent".to_string())
            .await
            .unwrap();
        store
            .grant(&NamespaceGrantRecord {
                namespace: scoped_ns(ns),
                agent_id: "admin-agent".to_string(),
                can_read: true,
                can_write: true,
                granted_by: "admin-agent".to_string(),
                created_at: chrono::Utc::now(),
            })
            .await
            .unwrap();
        store.seed(&scoped_ns(ns), "admin-agent", &format!("{ns} 数据"));
    }

    let backup = svc
        .create(
            &a,
            BackupCreateRequest {
                namespace: ALL_NAMESPACES.to_string(),
                note: None,
            },
        )
        .await
        .unwrap();

    let report = svc
        .restore(
            &a,
            RestoreRequest {
                backup_id: backup.id,
                mode: RestoreMode::Replace,
            },
        )
        .await
        .unwrap();

    assert_eq!(
        report.safety_backup_ids.len(),
        2,
        "两个受影响命名空间都应各有一条安全备份 ID"
    );
    assert_eq!(
        report.safety_backup_id,
        Some(report.safety_backup_ids[0]),
        "旧单数字段取第一个，避免下游消费方断裂"
    );
    // 两条安全备份都真实存在且可读取
    for id in &report.safety_backup_ids {
        assert!(store.get_backup(*id).await.is_ok());
    }
}

#[test]
fn test_restore_report_deserializes_legacy_json_without_new_fields() {
    // 旧版本报告没有失败计数与 safety_backup_ids 字段，反序列化必须兼容
    let legacy = serde_json::json!({
        "backup_id": Uuid::new_v4().to_string(),
        "mode": "merge",
        "total": 3,
        "created": 1,
        "updated": 1,
        "unchanged": 1,
        "safety_backup_id": Uuid::new_v4().to_string(),
    });
    let report: RestoreReport = serde_json::from_value(legacy).unwrap();
    assert_eq!(report.failed_relations, 0);
    assert!(report.safety_backup_ids.is_empty());
}

// 编译期检查：Embedder trait 对象可用
#[allow(dead_code)]
fn assert_embedder_object() {
    let _: Arc<dyn Embedder> = Arc::new(TestEmbedder::default());
}
