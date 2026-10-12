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
    scope_name, AccountScopedStore, BackupRecord, BackupStore, GrantStore, MemoryEntry,
    MemoryFilter, MemoryKind, MemoryStatus, MemoryStore, NamespaceGrantRecord,
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
    }
}

fn filter_ns(ns: &str) -> MemoryFilter {
    MemoryFilter {
        namespace: Some(ns.to_string()),
        kinds: None,
        statuses: None,
        tags: None,
        source_agent: None,
        key: None,
        text_contains: None,
        before: None,
        limit: None,
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
