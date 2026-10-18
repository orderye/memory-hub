//! `AccountScopedStore` 全量虚拟化的行为验证（使用内存存储当作底层实现）。
//!
//! 沙箱不变量在此得到直接验证：
//! - 不同账户的同名命名空间互不可见、按 ID 读取互相失败；
//! - 授权与命名空间注册都限定在账户内；
//! - 全量备份 `*` 落库为账户内键，账户间列表互不可见；
//! - 前缀拼接幂等，备份负载恢复往返不会二次加前缀。

use chrono::Utc;
use common::AccountId;
use mem_core::{
    scope_name, AccountScopedStore, BackupRecord, BackupStore, GrantStore, IntegrationStore,
    MemoryEntry, MemoryFilter, MemoryKind, MemoryStatus, MemoryStore, NamespaceGrantRecord,
};
use std::sync::Arc;

fn account(n: u128) -> AccountId {
    uuid::Uuid::from_u128(n)
}

fn entry(ns: &str, agent: &str, content: &str) -> MemoryEntry {
    MemoryEntry {
        id: uuid::Uuid::new_v4(),
        namespace: ns.to_string(),
        key: None,
        kind: MemoryKind::Fact,
        content: content.to_string(),
        tags: vec![],
        metadata: serde_json::json!({}),
        embedding: None,
        embedding_model: None,
        source_agent: agent.to_string(),
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

fn filter_ns(ns: &str) -> MemoryFilter {
    MemoryFilter {
        namespace: Some(ns.to_string()),
        ..Default::default()
    }
}

fn hub() -> Arc<dyn mem_core::MemoryHubStore> {
    Arc::new(super::InMemoryHubStore::new())
}

fn scoped(
    store: &Arc<dyn mem_core::MemoryHubStore>,
    acct: AccountId,
) -> AccountScopedStore<dyn mem_core::MemoryHubStore> {
    AccountScopedStore::new(store.clone(), acct)
}

#[tokio::test]
async fn test_same_namespace_name_isolated_between_accounts() {
    let store = hub();
    let sa = scoped(&store, account(1));
    let sb = scoped(&store, account(2));

    let ea = sa
        .insert(&entry("personal", "agent-a", "A 的记忆"))
        .await
        .unwrap();
    let eb = sb
        .insert(&entry("personal", "agent-b", "B 的记忆"))
        .await
        .unwrap();
    assert_eq!(ea.namespace, "personal", "输出必须剥离账户前缀");
    assert_eq!(eb.namespace, "personal");

    // 各自只看得到自己的条目
    assert_eq!(sa.list(&filter_ns("personal")).await.unwrap().len(), 1);
    assert_eq!(sb.list(&filter_ns("personal")).await.unwrap().len(), 1);
    assert!(
        sa.get("personal", eb.id).await.is_err(),
        "跨账户按 ID 读取必须失败"
    );
    assert!(sb.get("personal", ea.id).await.is_err());

    // 同名空间在底层是两个键
    let raw = store.list_namespaces().await.unwrap();
    assert_eq!(raw.len(), 2);
    assert!(raw.iter().all(|ns| ns.contains('/')));

    // 作用域内的空间清单剥掉前缀
    assert_eq!(
        sa.list_namespaces().await.unwrap(),
        vec!["personal".to_string()]
    );
}

#[tokio::test]
async fn test_namespace_and_grants_isolated() {
    let store = hub();
    let sa = scoped(&store, account(1));
    let sb = scoped(&store, account(2));

    sa.ensure_namespace("personal", &"agent-a".to_string())
        .await
        .unwrap();
    sa.grant(&NamespaceGrantRecord {
        namespace: "personal".to_string(),
        agent_id: "agent-a".to_string(),
        can_read: true,
        can_write: true,
        granted_by: "agent-a".to_string(),
        created_at: Utc::now(),
    })
    .await
    .unwrap();

    assert!(sa
        .can_write("personal", &"agent-a".to_string())
        .await
        .unwrap());
    // 账户 B 的同名空间没有授权，且不会命中 A 的授权行
    assert!(!sb
        .can_write("personal", &"agent-b".to_string())
        .await
        .unwrap());
    assert!(!sb
        .can_write("personal", &"agent-a".to_string())
        .await
        .unwrap());
    assert!(sb.get_namespace("personal").await.unwrap().is_none());
    assert!(sa.get_namespace("personal").await.unwrap().is_some());

    // 授权清单剥离前缀且不跨账户
    let grants = sa.grants_for(&"agent-a".to_string()).await.unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].namespace, "personal");
    assert!(sb
        .grants_for(&"agent-a".to_string())
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_backup_star_scoped_per_account() {
    let store = hub();
    let sa = scoped(&store, account(1));
    let sb = scoped(&store, account(2));

    sa.insert(&entry("personal", "agent-a", "A1"))
        .await
        .unwrap();
    sa.insert(&entry("work", "agent-a", "A2")).await.unwrap();
    sb.insert(&entry("personal", "agent-b", "B1"))
        .await
        .unwrap();

    // 账户内"全量"清单只包含本账户
    let mut ns = sa.list_namespaces().await.unwrap();
    ns.sort();
    assert_eq!(ns, vec!["personal".to_string(), "work".to_string()]);
    assert_eq!(
        sb.list_namespaces().await.unwrap(),
        vec!["personal".to_string()]
    );

    // `*` 全量备份记录落库为账户内键，列表互不可见
    let record = BackupRecord {
        id: uuid::Uuid::new_v4(),
        namespace: "*".to_string(),
        created_by: "agent-a".to_string(),
        note: None,
        entry_count: 2,
        checksum: "x".to_string(),
        payload: serde_json::json!({"entries": []}),
        retain_until: None,
        created_at: Utc::now(),
    };
    sa.save_backup(&record).await.unwrap();

    let a_backups = sa.list_backups(None).await.unwrap();
    assert_eq!(a_backups.len(), 1);
    assert_eq!(a_backups[0].namespace, "*", "对外呈现原始 sentinel");
    assert!(sb.list_backups(None).await.unwrap().is_empty());
    // 底层键带账户前缀
    let raw = store.list_backups(None).await.unwrap();
    assert_eq!(raw[0].namespace, format!("{}/*", account(1)));
}

#[tokio::test]
async fn test_get_by_id_is_fail_closed_across_accounts() {
    let store = hub();
    let sa = scoped(&store, account(1));
    let sb = scoped(&store, account(2));

    let backup = BackupRecord {
        id: uuid::Uuid::new_v4(),
        namespace: "personal".to_string(),
        created_by: "agent-a".to_string(),
        note: None,
        entry_count: 1,
        checksum: "x".to_string(),
        payload: serde_json::json!({"entries": []}),
        retain_until: None,
        created_at: Utc::now(),
    };
    sa.save_backup(&backup).await.unwrap();

    // 属主按 ID 可取回
    let owned = sa.get_backup(backup.id).await.unwrap();
    assert_eq!(owned.namespace, "personal");
    // 他账户即使已知该 UUID，也必须当作不存在（而非拿到他人数据与明文命名空间）
    assert!(matches!(
        sb.get_backup(backup.id).await.unwrap_err(),
        common::HubError::NotFound
    ));

    let proposal = mem_core::domain::IntegrationProposal {
        id: uuid::Uuid::new_v4(),
        namespace: "personal".to_string(),
        kind: mem_core::IntegrationKind::Duplicate,
        member_ids: vec![],
        suggested_survivor: None,
        similarity: 0.9,
        status: mem_core::domain::ProposalStatus::Open,
        resolution_note: None,
        created_at: Utc::now(),
        resolved_at: None,
    };
    sa.save_proposal(&proposal).await.unwrap();
    assert!(sa.get_proposal(proposal.id).await.is_ok());
    assert!(matches!(
        sb.get_proposal(proposal.id).await.unwrap_err(),
        common::HubError::NotFound
    ));
}

#[tokio::test]
async fn test_save_if_version_rejects_stale_revision() {
    let store = hub();
    let sa = scoped(&store, account(1));
    let e = sa
        .insert(&entry("personal", "agent-a", "初稿"))
        .await
        .unwrap();
    assert_eq!(e.version, 1);

    // 正常修订：以当前版本为条件
    let mut rev = e.clone();
    rev.content = "第二稿".to_string();
    rev.version += 1;
    let saved = sa.save_if_version(&rev, e.version).await.unwrap();
    assert_eq!(saved.version, 2);

    // 陈旧修订：另一个写者已把版本推进到 2，仍以 1 为条件的写入必须被拒
    let mut stale = e.clone();
    stale.content = "被并发覆盖的稿子".to_string();
    stale.version += 1;
    let err = sa.save_if_version(&stale, e.version).await.unwrap_err();
    assert!(
        matches!(err, common::HubError::Conflict),
        "并发修订应判为冲突，实际 {err:?}"
    );
    // 未被静默覆盖
    let now = sa.get("personal", e.id).await.unwrap();
    assert_eq!(now.content, "第二稿");
    assert_eq!(now.version, 2);
}

#[tokio::test]
async fn test_revise_is_atomic_between_entry_and_history() {
    let store = hub();
    let sa = scoped(&store, account(1));
    let e = sa
        .insert(&entry("personal", "agent-a", "初稿"))
        .await
        .unwrap();

    // 正常修订：条目推进 + 历史追加
    let mut rev = e.clone();
    rev.content = "第二稿".to_string();
    rev.version += 1;
    let saved = sa
        .revise(&rev, e.version, mem_core::domain::VersionChange::Updated)
        .await
        .unwrap();
    assert_eq!(saved.version, 2);
    // 注：`insert` 本身不写历史，"created" 那一行由服务层在插入后追加；
    // 这里只验 store 层 revise 的原子性——一次修订恰好一条历史。
    let history = sa.versions(e.id).await.unwrap();
    assert_eq!(history.len(), 1, "修订应追加一条历史，实际 {history:?}");
    assert_eq!(history[0].content, "第二稿");

    // 冲突修订：既不改条目，也不留历史（否则版本历史与条目分叉）
    let mut stale = e.clone();
    stale.content = "陈旧稿".to_string();
    stale.version += 1;
    let err = sa
        .revise(&stale, e.version, mem_core::domain::VersionChange::Updated)
        .await
        .unwrap_err();
    assert!(matches!(err, common::HubError::Conflict));
    assert_eq!(
        sa.get("personal", e.id).await.unwrap().content,
        "第二稿",
        "冲突写入不得改动条目"
    );
    assert_eq!(
        sa.versions(e.id).await.unwrap().len(),
        1,
        "冲突修订不得留下版本历史"
    );
}

#[tokio::test]
async fn test_prefix_idempotent_roundtrip_from_payload() {
    // 备份负载中的条目名义上是"用户可见名"，但即使拿到已作用域名
    // （如旧负载或未来格式变化），也不能二次加前缀
    let store = hub();
    let sa = scoped(&store, account(1));
    let already = scope_name(&account(1), "personal");
    let e = sa
        .insert(&entry(&already, "agent-a", "往返"))
        .await
        .unwrap();
    assert_eq!(e.namespace, "personal");
    let raw = store.all_in_namespace(&already, true).await.unwrap();
    assert_eq!(raw.len(), 1, "不得出现双重前缀键");
}

#[tokio::test]
async fn test_insert_initial_creates_entry_and_version_atomically() {
    let store = hub();
    let sa = scoped(&store, account(1));
    let e = sa
        .insert_initial(&entry("personal", "agent-a", "第一条记忆"))
        .await
        .unwrap();
    assert_eq!(e.version, 1);
    let history = sa.versions(e.id).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(
        history[0].change_type,
        mem_core::domain::VersionChange::Created
    );
    assert_eq!(history[0].content, "第一条记忆");
    assert_eq!(history[0].version, 1);
}

#[tokio::test]
async fn test_get_by_key_prioritizes_active_over_merged_and_archived() {
    let store = hub();
    let sa = scoped(&store, account(1));

    // 1. 先插入一条历史已合并条目（模拟历史合并留下的相同 key）
    let mut merged = entry("personal", "agent-a", "历史合并内容");
    merged.key = Some("unique-profile".to_string());
    merged.status = MemoryStatus::Merged;
    sa.insert(&merged).await.unwrap();

    // 2. 插入一条当前活跃的条目（相同 key）
    let mut active = entry("personal", "agent-a", "当前活跃内容");
    active.key = Some("unique-profile".to_string());
    active.status = MemoryStatus::Active;
    sa.insert(&active).await.unwrap();

    // get_by_key 必须确定性命中 active 条目，不得误取 merged
    let found = sa.get_by_key("personal", "unique-profile").await.unwrap();
    assert!(found.is_some());
    let unwrapped = found.unwrap();
    assert_eq!(unwrapped.id, active.id);
    assert_eq!(unwrapped.content, "当前活跃内容");
    assert_eq!(unwrapped.status, MemoryStatus::Active);
}
