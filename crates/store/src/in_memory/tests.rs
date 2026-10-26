use super::*;
use mem_core::{
    AccountStore, BackupStore, CognitiveProposalStore, EvidenceStore, GrantStore, IntegrationStore,
    MemoryHubStore, MemoryKind, MemoryScene, MemoryStore, RelationStore,
};

fn entry(ns: &str, content: &str, key: Option<&str>) -> MemoryEntry {
    MemoryEntry {
        id: Uuid::new_v4(),
        namespace: ns.to_string(),
        key: key.map(str::to_string),
        kind: MemoryKind::Fact,
        content: content.to_string(),
        tags: vec![],
        metadata: serde_json::json!({}),
        embedding: None,
        embedding_model: None,
        source_agent: "agent-a".to_string(),
        confidence: 1.0,
        status: MemoryStatus::Active,
        version: 1,
        expires_at: None,
        merged_into: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        scene: None,
        layer: None,
        valid_from: None,
        valid_until: None,
        occurred_at: None,
    }
}

#[tokio::test]
async fn test_namespace_isolation() {
    let store = InMemoryHubStore::new();
    let a = entry("ns-a", "A 的记忆", None);
    store.insert(&a).await.unwrap();

    // 命名空间不对 → NotFound
    assert!(store.get("ns-b", a.id).await.is_err());
    // 命名空间对 → 命中
    assert!(store.get("ns-a", a.id).await.is_ok());
}

#[tokio::test]
async fn test_key_lookup_and_conflict() {
    let store = InMemoryHubStore::new();
    let a = entry("ns", "第一条", Some("k:1"));
    store.insert(&a).await.unwrap();

    assert!(store.get_by_key("ns", "k:1").await.unwrap().is_some());

    // 同 id 重复插入 → Conflict
    let err = store.insert(&a).await.unwrap_err();
    assert!(matches!(err, HubError::Conflict));
}

/// 契约锚定：save 只保存已存在条目（与 PostgreSQL 对齐，见 MemoryStore::save 文档）。
/// 此前内存实现是无条件 upsert，PG 是静默 no-op——双后端语义分叉，
/// 内存测试永远验不出生产丢写。
#[tokio::test]
async fn test_save_requires_existing_entry() {
    let store = InMemoryHubStore::new();
    let missing = entry("ns", "从未插入的条目", None);
    let err = store.save(&missing).await.unwrap_err();
    assert!(
        matches!(err, HubError::NotFound),
        "save 不存在 id 应返回 NotFound，实际 {err:?}"
    );
    // 也不得凭空新建
    assert!(store.get("ns", missing.id).await.is_err());
    assert_eq!(store.count_all(), 0);

    // 已存在条目正常写回
    let a = entry("ns", "初稿", None);
    store.insert(&a).await.unwrap();
    let mut updated = a.clone();
    updated.content = "第二稿".to_string();
    store.save(&updated).await.unwrap();
    assert_eq!(store.get("ns", a.id).await.unwrap().content, "第二稿");
}

/// 契约锚定：revise 带乐观锁，版本不符返回 Conflict（与 PostgreSQL 对齐）；
/// 条目不存在同样按乐观锁前提不成立处理为 Conflict。
#[tokio::test]
async fn test_revise_conflict_contract() {
    let store = InMemoryHubStore::new();
    let a = entry("ns", "初稿", None);
    store.insert(&a).await.unwrap();

    // 正常修订：版本相符 → 成功并追加历史
    let mut rev = a.clone();
    rev.content = "第二稿".to_string();
    rev.version += 1;
    let saved = store
        .revise(&rev, a.version, VersionChange::Updated)
        .await
        .unwrap();
    assert_eq!(saved.version, 2);

    // 版本不符（陈旧读）→ Conflict，且不留历史、不覆盖条目
    let mut stale = a.clone();
    stale.content = "陈旧稿".to_string();
    stale.version += 1;
    let err = store
        .revise(&stale, a.version, VersionChange::Updated)
        .await
        .unwrap_err();
    assert!(
        matches!(err, HubError::Conflict),
        "revise 版本不符应返回 Conflict，实际 {err:?}"
    );
    assert_eq!(store.get("ns", a.id).await.unwrap().content, "第二稿");
    assert_eq!(
        store.versions(a.id).await.unwrap().len(),
        1,
        "冲突修订不得留下版本历史"
    );

    // 条目不存在 → Conflict（乐观锁前提不成立，与 PG update 0 行一致）
    let missing = entry("ns", "不存在的条目", None);
    let err = store
        .revise(&missing, 1, VersionChange::Updated)
        .await
        .unwrap_err();
    assert!(
        matches!(err, HubError::Conflict),
        "revise 不存在条目应返回 Conflict，实际 {err:?}"
    );
}

#[tokio::test]
async fn test_cursor_pagination_and_limit() {
    let store = InMemoryHubStore::new();
    let mut ids = Vec::new();
    for i in 0..5 {
        let mut e = entry("ns", &format!("条目 {}", i), None);
        e.updated_at = Utc::now() + chrono::Duration::seconds(i);
        ids.push(e.id);
        store.insert(&e).await.unwrap();
    }

    // 第一页：取最新 2 条
    let page1 = store
        .list(&MemoryFilter {
            namespace: Some("ns".to_string()),
            limit: Some(2),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page1.len(), 2);
    // 最新在前
    assert!(page1[0].updated_at >= page1[1].updated_at);

    // 第二页：严格在游标之前
    let cursor_ts = page1[1].updated_at;
    let cursor_id = page1[1].id;
    let page2 = store
        .list(&MemoryFilter {
            namespace: Some("ns".to_string()),
            limit: Some(2),
            before: Some((cursor_ts, cursor_id)),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page2.len(), 2);
    assert!(page2.iter().all(|e| e.id != cursor_id));
}

#[tokio::test]
async fn test_similar_filters_status_and_dim() {
    let store = InMemoryHubStore::new();
    let mut e = entry("ns", "有向量的条目", None);
    e.embedding = Some(vec![1.0, 0.0]);
    store.insert(&e).await.unwrap();

    let hit = store
        .similar(
            &MemoryFilter {
                namespace: Some("ns".to_string()),
                ..Default::default()
            },
            &[1.0, 0.0],
            5,
        )
        .await
        .unwrap();
    assert_eq!(hit.len(), 1);
    assert!((hit[0].score - 1.0).abs() < 1e-6);

    // 维度不匹配 → 无结果
    let miss = store
        .similar(
            &MemoryFilter {
                namespace: Some("ns".to_string()),
                ..Default::default()
            },
            &[1.0, 0.0, 0.0],
            5,
        )
        .await
        .unwrap();
    assert!(miss.is_empty());

    // 归档后不再召回
    e.status = MemoryStatus::Archived;
    store.save(&e).await.unwrap();
    let empty = store
        .similar(
            &MemoryFilter {
                namespace: Some("ns".to_string()),
                ..Default::default()
            },
            &[1.0, 0.0],
            5,
        )
        .await
        .unwrap();
    assert!(empty.is_empty());
}

/// 回归：向量召回曾把 scene / valid_at 过滤留在 Rust 侧，
/// 过滤条件下推后，被过滤条目不得占据 top-k 名额
#[tokio::test]
async fn test_similar_pushes_down_scene_and_validity_filters() {
    use chrono::TimeZone;
    let store = InMemoryHubStore::new();

    // 同主题、不同场景的三条：只有 Coding 场景那条符合查询
    let mut chat = entry("ns", "代码重构方案 A", None);
    chat.scene = Some(MemoryScene::Chat);
    chat.embedding = Some(vec![1.0, 0.0]);
    let mut coding = entry("ns", "代码重构方案 B", None);
    coding.scene = Some(MemoryScene::Coding);
    coding.embedding = Some(vec![1.0, 0.0]);
    let mut expired = entry("ns", "代码重构方案 C", None);
    expired.scene = Some(MemoryScene::Coding);
    expired.embedding = Some(vec![1.0, 0.0]);
    expired.valid_until = Some(chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap());

    for e in [&chat, &coding, &expired] {
        store.insert(e).await.unwrap();
    }

    let filter = MemoryFilter {
        namespace: Some("ns".to_string()),
        statuses: Some(vec![MemoryStatus::Active]),
        scene: Some(MemoryScene::Coding),
        valid_at: Some(chrono::Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap()),
        ..Default::default()
    };
    let hits = store.similar(&filter, &[1.0, 0.0], 10).await.unwrap();
    assert_eq!(
        hits.len(),
        1,
        "下推过滤必须只返回满足 scene/valid_at 的条目"
    );
    assert_eq!(hits[0].item.content, "代码重构方案 B");
}

/// 回归：FTS 词法曾用整段查询做裸子串匹配，与 PostgreSQL 分词语义不同，
/// 且 AND 语义下多词查询会整条遗漏
#[tokio::test]
async fn test_list_fts_partial_match_and_relevance_order() {
    let store = InMemoryHubStore::new();
    // 精确命中两个词元的旧条目，与只命中一个词元的新条目
    let mut both = entry("ns", "Redis 缓存集群主节点说明", None);
    both.updated_at = chrono::Utc::now() - chrono::Duration::days(30);
    let mut one = entry("ns", "缓存预热脚本", None);
    one.updated_at = chrono::Utc::now();
    store.insert(&both).await.unwrap();
    store.insert(&one).await.unwrap();

    let filter = MemoryFilter {
        namespace: Some("ns".to_string()),
        text_contains: Some("Redis 缓存集群".to_string()),
        limit: Some(10),
        ..Default::default()
    };
    let rows = store.list(&filter).await.unwrap();

    assert_eq!(
        rows.len(),
        2,
        "部分匹配（OR 语义）应召回只命中一个词元的文档"
    );
    assert_eq!(
        rows[0].content, "Redis 缓存集群主节点说明",
        "命中词元更多的文档必须排在前（相关度优先于更新时间）"
    );
}

#[tokio::test]
async fn test_versions_idempotent_and_sorted() {
    let store = InMemoryHubStore::new();
    let e = entry("ns", "内容", None);
    store.insert(&e).await.unwrap();

    let mk = |v: i64| MemoryVersion {
        memory_id: e.id,
        version: v,
        content: format!("v{}", v),
        tags: vec![],
        metadata: serde_json::json!({}),
        confidence: 1.0,
        changed_by: "agent-a".to_string(),
        change_type: VersionChange::Updated,
        changed_at: Utc::now(),
        scene: None,
        layer: None,
        valid_from: None,
        valid_until: None,
        occurred_at: None,
    };
    store.record_version(&mk(1)).await.unwrap();
    store.record_version(&mk(2)).await.unwrap();
    // 重放 v1 不得重复入账
    store.record_version(&mk(1)).await.unwrap();

    let vs = store.versions(e.id).await.unwrap();
    assert_eq!(vs.len(), 2);
    assert_eq!(vs[0].version, 1);
    assert_eq!(vs[1].version, 2);
}

#[tokio::test]
async fn test_archive_expired() {
    let store = InMemoryHubStore::new();
    let mut e = entry("ns", "将过期", None);
    e.expires_at = Some(Utc::now() - chrono::Duration::hours(1));
    store.insert(&e).await.unwrap();

    let archived = store.archive_expired(Utc::now()).await.unwrap();
    assert_eq!(archived, 1);
    assert_eq!(
        store.get("ns", e.id).await.unwrap().status,
        MemoryStatus::Archived
    );
    // 版本历史记录了归档事件
    let vs = store.versions(e.id).await.unwrap();
    assert!(vs.iter().any(|v| v.change_type == VersionChange::Archived));
}

#[tokio::test]
async fn test_namespace_grants_and_wildcard_disabled() {
    let store = InMemoryHubStore::new();
    store
        .ensure_namespace("ns", &"agent-a".to_string())
        .await
        .unwrap();

    // owner 尚未被授权：不可读写
    assert!(!store.can_read("ns", &"agent-a".to_string()).await.unwrap());

    store
        .grant(&NamespaceGrantRecord {
            namespace: "ns".to_string(),
            agent_id: "agent-a".to_string(),
            can_read: true,
            can_write: true,
            granted_by: "agent-a".to_string(),
            created_at: Utc::now(),
        })
        .await
        .unwrap();
    assert!(store.can_read("ns", &"agent-a".to_string()).await.unwrap());
    assert!(store.can_write("ns", &"agent-a".to_string()).await.unwrap());

    // 沙箱语义：`*` 通配授权行不再对任何命名空间生效
    store
        .grant(&NamespaceGrantRecord {
            namespace: "*".to_string(),
            agent_id: "agent-b".to_string(),
            can_read: true,
            can_write: true,
            granted_by: "agent-a".to_string(),
            created_at: Utc::now(),
        })
        .await
        .unwrap();
    assert!(!store
        .can_read("any-ns", &"agent-b".to_string())
        .await
        .unwrap());
    assert!(!store.can_write("ns", &"agent-b".to_string()).await.unwrap());

    // 撤销后失效
    store.revoke("ns", &"agent-a".to_string()).await.unwrap();
    assert!(!store.can_write("ns", &"agent-a".to_string()).await.unwrap());

    let owned = store
        .owned_namespaces(&"agent-a".to_string())
        .await
        .unwrap();
    assert_eq!(owned, vec!["ns".to_string()]);
}

#[tokio::test]
async fn test_accounts_and_sessions_roundtrip() {
    let store = InMemoryHubStore::new();

    let acct = store
        .create_account("Alice", Some("hash-a"), "爱丽丝")
        .await
        .unwrap();
    assert_eq!(acct.username, "alice", "用户名归一化为小写");
    assert!(acct.has_password);

    // 用户名冲突（大小写不敏感）
    assert!(store.create_account("ALICE", Some("x"), "").await.is_err());

    // 按用户名查哈希（认证路径）
    assert_eq!(
        store.password_hash_of("aLiCe").await.unwrap().as_deref(),
        Some("hash-a")
    );

    // 引导账户幂等（按固定 ID）
    let boot1 = store
        .ensure_account(&common::BOOTSTRAP_ACCOUNT_ID, "bootstrap", "引导账户")
        .await
        .unwrap();
    let boot2 = store
        .ensure_account(&common::BOOTSTRAP_ACCOUNT_ID, "bootstrap", "引导账户")
        .await
        .unwrap();
    assert_eq!(boot1.id, boot2.id);
    assert!(!boot1.has_password, "引导账户初始无口令");

    // 会话：创建 → 读取 → 过期清理 → 删除
    let hash = vec![7u8; 32];
    let future = Utc::now() + chrono::Duration::hours(1);
    store.create_session(&hash, &acct.id, future).await.unwrap();
    assert_eq!(
        store.get_session(&hash).await.unwrap().unwrap().account_id,
        acct.id
    );
    assert_eq!(store.prune_sessions(Utc::now()).await.unwrap(), 0);
    assert_eq!(
        store
            .prune_sessions(future + chrono::Duration::seconds(1))
            .await
            .unwrap(),
        1
    );
    assert!(store.get_session(&hash).await.unwrap().is_none());
}

#[tokio::test]
async fn test_backup_and_proposal_roundtrip() {
    let store = InMemoryHubStore::new();
    let backup = BackupRecord {
        id: Uuid::new_v4(),
        namespace: "ns".to_string(),
        created_by: "agent-a".to_string(),
        note: None,
        entry_count: 0,
        checksum: "abc".to_string(),
        payload: serde_json::json!({}),
        retain_until: None,
        created_at: Utc::now(),
    };
    store.save_backup(&backup).await.unwrap();
    assert_eq!(store.list_backups(None).await.unwrap().len(), 1);
    assert_eq!(store.list_backups(Some("ns")).await.unwrap().len(), 1);
    assert_eq!(store.list_backups(Some("other")).await.unwrap().len(), 0);
    store.delete_backup(backup.id).await.unwrap();
    assert!(store.list_backups(None).await.unwrap().is_empty());

    let proposal = IntegrationProposal {
        id: Uuid::new_v4(),
        namespace: "ns".to_string(),
        kind: mem_core::IntegrationKind::Duplicate,
        member_ids: vec![Uuid::new_v4(), Uuid::new_v4()],
        suggested_survivor: None,
        similarity: 0.99,
        status: ProposalStatus::Open,
        resolution_note: None,
        created_at: Utc::now(),
        resolved_at: None,
        metadata: serde_json::json!({}),
    };
    store.save_proposal(&proposal).await.unwrap();
    assert!(store
        .open_exists(
            "ns",
            &[("duplicate".to_string(), proposal.member_ids.clone())]
        )
        .await
        .unwrap());
    assert_eq!(
        store
            .list_proposals("ns", Some(ProposalStatus::Open))
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn test_hub_store_aggregation() {
    // 编译期即验证：InMemoryHubStore 实现全部七个 trait
    fn assert_impl<T: MemoryHubStore>() {}
    assert_impl::<InMemoryHubStore>();
}

#[tokio::test]
async fn test_evidence_store_crud() {
    let store = InMemoryHubStore::new();
    let ev1 = store
        .append_evidence(&NewEvidence {
            id: None,
            namespace: "ns".to_string(),
            source_kind: EvidenceSourceKind::Session,
            source_ref: Some("sess-1".to_string()),
            content_hash: "hash-1".to_string(),
            excerpt: Some("对话片段1".to_string()),
            occurred_at: Some(Utc::now()),
            expires_at: None,
        })
        .await
        .unwrap();

    let ev2 = store
        .append_evidence(&NewEvidence {
            id: None,
            namespace: "ns".to_string(),
            source_kind: EvidenceSourceKind::GitCommit,
            source_ref: Some("sha-abc".to_string()),
            content_hash: "hash-2".to_string(),
            excerpt: Some("commit diff".to_string()),
            occurred_at: Some(Utc::now()),
            expires_at: None,
        })
        .await
        .unwrap();

    // get_evidence
    let fetched = store.get_evidence("ns", ev1.id).await.unwrap();
    assert_eq!(fetched.id, ev1.id);
    assert_eq!(fetched.source_kind, EvidenceSourceKind::Session);

    // list_evidence with kind filter
    let sessions = store
        .list_evidence("ns", Some(EvidenceSourceKind::Session), None, 10)
        .await
        .unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, ev1.id);

    let commits = store
        .list_evidence("ns", Some(EvidenceSourceKind::GitCommit), None, 10)
        .await
        .unwrap();
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0].id, ev2.id);

    // all_evidence_in_namespace
    let all = store.all_evidence_in_namespace("ns").await.unwrap();
    assert_eq!(all.len(), 2);

    // delete_evidence
    store.delete_evidence("ns", ev1.id).await.unwrap();
    assert!(store.get_evidence("ns", ev1.id).await.is_err());
    assert_eq!(
        store.all_evidence_in_namespace("ns").await.unwrap().len(),
        1
    );
}

#[tokio::test]
async fn test_relation_store_semantics() {
    let store = InMemoryHubStore::new();
    let m1 = entry("ns-a", "条目1", None);
    let m2 = entry("ns-a", "条目2", None);
    let m_diff_ns = entry("ns-b", "异命名空间条目", None);
    store.insert(&m1).await.unwrap();
    store.insert(&m2).await.unwrap();
    store.insert(&m_diff_ns).await.unwrap();

    // 1. 自环拒绝
    let loop_err = store
        .add_relation(
            "ns-a",
            &NewMemoryRelation {
                from_memory_id: m1.id,
                to_memory_id: m1.id,
                relation_kind: MemoryRelationKind::Supports,
                confidence: 1.0,
                source_evidence_id: None,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(loop_err, HubError::InvalidArgument(_)));

    // 2. 跨命名空间关系拒绝
    let cross_err = store
        .add_relation(
            "ns-a",
            &NewMemoryRelation {
                from_memory_id: m1.id,
                to_memory_id: m_diff_ns.id,
                relation_kind: MemoryRelationKind::Supports,
                confidence: 1.0,
                source_evidence_id: None,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(cross_err, HubError::NotFound));

    // 3. 正常添加并双向查询
    let rel = store
        .add_relation(
            "ns-a",
            &NewMemoryRelation {
                from_memory_id: m1.id,
                to_memory_id: m2.id,
                relation_kind: MemoryRelationKind::Supports,
                confidence: 0.95,
                source_evidence_id: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(rel.confidence, 0.95);

    let from = store.list_relations_from("ns-a", m1.id).await.unwrap();
    assert_eq!(from.len(), 1);
    assert_eq!(from[0].to_memory_id, m2.id);

    let to = store.list_relations_to("ns-a", m2.id).await.unwrap();
    assert_eq!(to.len(), 1);
    assert_eq!(to[0].from_memory_id, m1.id);

    // 4. 删除条目引发关联关系级联删除
    store.delete("ns-a", m1.id).await.unwrap();
    assert!(store
        .list_relations_from("ns-a", m1.id)
        .await
        .unwrap()
        .is_empty());
    assert!(store
        .list_relations_to("ns-a", m2.id)
        .await
        .unwrap()
        .is_empty());
}

// ===== 认知提案存储契约 =====

fn cognitive_proposal(ns: &str, status: CognitiveProposalStatus) -> CognitiveProposalRecord {
    CognitiveProposalRecord {
        id: Uuid::new_v4(),
        namespace: ns.to_string(),
        candidate: mem_core::CognitiveCandidate {
            content: "候选内容".to_string(),
            scene: mem_core::MemoryScene::Chat,
            layer: mem_core::CognitiveLayer::Fact,
            confidence: 0.9,
            valid_from: None,
            valid_until: None,
            occurred_at: None,
            tags: vec![],
            metadata: serde_json::json!({}),
            relation_kind: None,
            target_memory_id: None,
        },
        source_evidence_id: Uuid::new_v4(),
        status,
        rejection_reason: None,
        resulting_memory_id: None,
        resolved_by: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

/// 认知提案完整生命周期：保存 → 读取 → 列表（状态过滤）→ 裁决推进 → 乐观锁
#[tokio::test]
async fn test_cognitive_proposal_lifecycle_contract() {
    let store = InMemoryHubStore::new();

    // 保存与读取（载荷 roundtrip）
    let p1 = cognitive_proposal("cog-ns", CognitiveProposalStatus::PendingReview);
    let p2 = cognitive_proposal("cog-ns", CognitiveProposalStatus::PendingReview);
    let p_other_ns = cognitive_proposal("other-ns", CognitiveProposalStatus::PendingReview);
    store.save_cognitive_proposal(&p1).await.unwrap();
    store.save_cognitive_proposal(&p2).await.unwrap();
    store.save_cognitive_proposal(&p_other_ns).await.unwrap();

    let got = store.get_cognitive_proposal(p1.id).await.unwrap();
    assert_eq!(got.id, p1.id);
    assert_eq!(got.candidate, p1.candidate, "候选载荷必须无损往返");
    assert_eq!(got.status, CognitiveProposalStatus::PendingReview);

    // 读取不存在的提案 → NotFound
    assert!(matches!(
        store.get_cognitive_proposal(Uuid::new_v4()).await,
        Err(HubError::NotFound)
    ));

    // 列表按命名空间过滤；状态过滤叠加
    let all = store
        .list_cognitive_proposals("cog-ns", None)
        .await
        .unwrap();
    assert_eq!(all.len(), 2);
    let pending = store
        .list_cognitive_proposals("cog-ns", Some(CognitiveProposalStatus::PendingReview))
        .await
        .unwrap();
    assert_eq!(pending.len(), 2);

    // 同 ID 重复保存 → Conflict（不做覆盖更新）
    assert!(matches!(
        store.save_cognitive_proposal(&p1).await,
        Err(HubError::Conflict)
    ));

    // 裁决推进：pending_review → rejected
    let updated = store
        .update_cognitive_proposal_status(
            p1.id,
            CognitiveProposalStatus::PendingReview,
            CognitiveProposalStatus::Rejected,
            Some("不采用"),
            None,
            Some("reviewer-agent"),
        )
        .await
        .unwrap();
    assert_eq!(updated.status, CognitiveProposalStatus::Rejected);
    assert_eq!(updated.rejection_reason.as_deref(), Some("不采用"));
    assert_eq!(updated.resolved_by.as_deref(), Some("reviewer-agent"));

    // 乐观锁：前置状态不匹配 → Conflict
    assert!(matches!(
        store
            .update_cognitive_proposal_status(
                p1.id,
                CognitiveProposalStatus::PendingReview,
                CognitiveProposalStatus::Accepted,
                None,
                Some(Uuid::new_v4()),
                Some("reviewer-agent"),
            )
            .await,
        Err(HubError::Conflict)
    ));

    // 不存在的提案 → NotFound（而不是 Conflict）
    assert!(matches!(
        store
            .update_cognitive_proposal_status(
                Uuid::new_v4(),
                CognitiveProposalStatus::PendingReview,
                CognitiveProposalStatus::Accepted,
                None,
                None,
                None,
            )
            .await,
        Err(HubError::NotFound)
    ));

    // 推进后列表只剩 1 条 pending
    let pending = store
        .list_cognitive_proposals("cog-ns", Some(CognitiveProposalStatus::PendingReview))
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, p2.id);
}
