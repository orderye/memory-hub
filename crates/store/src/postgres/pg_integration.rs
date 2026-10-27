use super::*;
use common::AgentId;
use mem_core::{AgentStore, BackupStore, GrantStore, MemoryKind, MemoryStore};

async fn store() -> PostgresHubStore {
    let url = std::env::var("DATABASE_URL").expect("需设置 DATABASE_URL");
    PostgresHubStore::connect(&url, 5, 0)
        .await
        .expect("连接失败（将自动执行迁移）")
}

fn entry(ns: &str, agent: &str, content: &str) -> MemoryEntry {
    MemoryEntry {
        id: Uuid::new_v4(),
        namespace: ns.to_string(),
        key: None,
        kind: MemoryKind::Fact,
        content: content.to_string(),
        tags: vec!["pg".to_string()],
        metadata: serde_json::json!({"k": "v"}),
        embedding: None,
        embedding_model: None,
        source_agent: agent.to_string(),
        confidence: 0.9,
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
#[ignore = "需要 DATABASE_URL"]
async fn pg_memory_crud_and_namespace_isolation() {
    let store = store().await;
    let ns = "it-ns-crud";
    store
        .ensure_namespace(ns, &AgentId::from("it-agent"))
        .await
        .unwrap();

    let e = entry(ns, "it-agent", "PostgreSQL 持久化冒烟");
    store.insert(&e).await.unwrap();

    let got = store.get(ns, e.id).await.unwrap();
    assert_eq!(got.content, "PostgreSQL 持久化冒烟");
    assert_eq!(got.status, MemoryStatus::Active);

    // 命名空间隔离：错误命名空间读不到
    assert!(store.get("other-ns", e.id).await.is_err());

    // 修订 + 版本历史
    let mut patched = got.clone();
    patched.content = "修订后的内容".to_string();
    patched.version = 2;
    store.save(&patched).await.unwrap();
    store
        .record_version(&MemoryVersion {
            memory_id: e.id,
            version: 2,
            content: "修订后的内容".to_string(),
            tags: vec![],
            metadata: serde_json::json!({}),
            confidence: 0.9,
            changed_by: "it-agent".to_string(),
            change_type: VersionChange::Updated,
            changed_at: Utc::now(),
            scene: None,
            layer: None,
            valid_from: None,
            valid_until: None,
            occurred_at: None,
        })
        .await
        .unwrap();

    let versions = store.versions(e.id).await.unwrap();
    assert!(versions.iter().any(|v| v.version == 2));

    // 清理
    store.delete(ns, e.id).await.unwrap();
    assert!(store.get(ns, e.id).await.is_err());
}

#[tokio::test]
#[ignore = "需要 DATABASE_URL"]
async fn pg_grants_and_agent_registry() {
    let store = store().await;
    let ns = "it-ns-grant";
    let owner = AgentId::from("it-owner");

    let (_, created) = store.ensure_namespace(ns, &owner).await.unwrap();
    assert!(created, "首次注册应报告新建");

    // 被授权方必须先在 agents 表登记：namespace_grants.agent_id 有外键约束，
    // 未登记的 Agent 写不进授权行。生产路径上 memory_grant 工具同样先校验
    // 目标 Agent 存在且同属本账户，再落授权行。
    store
        .register_agent(&core_agent_reg("it-guest", None))
        .await
        .unwrap();

    store
        .grant(&NamespaceGrantRecord {
            namespace: ns.to_string(),
            agent_id: "it-guest".to_string(),
            can_read: true,
            can_write: false,
            granted_by: owner.clone(),
            created_at: Utc::now(),
        })
        .await
        .unwrap();

    assert!(store
        .can_read(ns, &AgentId::from("it-guest"))
        .await
        .unwrap());
    assert!(!store
        .can_write(ns, &AgentId::from("it-guest"))
        .await
        .unwrap());
    assert!(!store
        .can_read(ns, &AgentId::from("stranger"))
        .await
        .unwrap());

    // Agent 注册与密钥校验
    store
        .register_agent(&core_agent_reg("it-agent-x", Some("pw")))
        .await
        .unwrap();
    let agent = store
        .get_agent(&AgentId::from("it-agent-x"))
        .await
        .unwrap()
        .expect("应能读回");
    assert!(agent.confidential);
    assert!(store
        .verify_agent_secret(&AgentId::from("it-agent-x"), "pw")
        .await
        .unwrap());
    assert!(!store
        .verify_agent_secret(&AgentId::from("it-agent-x"), "wrong")
        .await
        .unwrap());

    // 状态停用
    store
        .set_agent_status(&AgentId::from("it-agent-x"), "suspended")
        .await
        .unwrap();
    let agent2 = store
        .get_agent(&AgentId::from("it-agent-x"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(agent2.status, "suspended");
}

fn core_agent_reg(id: &str, secret: Option<&str>) -> mem_core::AgentRegistration {
    mem_core::AgentRegistration {
        agent_id: id.to_string(),
        name: format!("Agent {}", id),
        kind: "service".to_string(),
        owner_account_id: common::BOOTSTRAP_ACCOUNT_ID,
        client_secret: secret.map(str::to_string),
        redirect_uris: vec![],
        max_scopes: vec![common::Scope::Read, common::Scope::Write],
    }
}

#[tokio::test]
#[ignore = "需要 DATABASE_URL"]
async fn pg_backup_and_restore_roundtrip() {
    let store = store().await;
    let ns = "it-ns-backup";
    store
        .ensure_namespace(ns, &AgentId::from("it-agent"))
        .await
        .unwrap();
    let e = entry(ns, "it-agent", "备份往返测试");
    store.insert(&e).await.unwrap();

    let entries = store.all_in_namespace(ns, true).await.unwrap();
    let checksum = {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(serde_json::to_vec(&entries).unwrap()))
    };
    let backup = BackupRecord {
        id: Uuid::new_v4(),
        namespace: ns.to_string(),
        created_by: "it-agent".to_string(),
        note: Some("integration".to_string()),
        entry_count: entries.len(),
        checksum,
        payload: serde_json::json!({ "entries": entries }),
        retain_until: None,
        created_at: Utc::now(),
    };
    store.save_backup(&backup).await.unwrap();
    let loaded = store.get_backup(backup.id).await.unwrap();
    assert_eq!(loaded.entry_count, 1);
    assert_eq!(loaded.checksum, backup.checksum);

    store.delete_backup(backup.id).await.unwrap();
    assert!(store.get_backup(backup.id).await.is_err());
    store.delete(ns, e.id).await.unwrap();
}

/// 迁移 013 + 启动回填的端到端验证：手工制造迁移 005 形态的受害行
/// （tsv_tokens = 原文、fts_retokenize_pending = true），跑回填，
/// 断言词元被写路径同源重写、标记清零、FTS 可命中中文子串。
#[tokio::test]
#[ignore = "需要 DATABASE_URL"]
async fn pg_fts_backfill_retokenizes_legacy_rows() {
    let store = store().await;
    let ns = "it-ns-fts-backfill";
    store
        .ensure_namespace(ns, &AgentId::from("it-agent"))
        .await
        .unwrap();

    let content = "服务部署在东京区域";
    let e = entry(ns, "it-agent", content);
    store.insert(&e).await.unwrap();
    // 正常写路径产出的就是分词流：不该被回填动到
    let expected = tsv_tokens_for(content);
    let pool = store.pool();

    // 还原受害形态（模拟迁移 005 执行后的存量行）
    sqlx::query(
        "update memories set tsv_tokens = $2, fts_retokenize_pending = true \
                     where namespace = $1 and id = $3",
    )
    .bind(ns)
    .bind(content)
    .bind(e.id)
    .execute(&pool)
    .await
    .unwrap();

    // 跑真实回填入口（connect() 内部调用的同一方法，含分批与清标记）
    store.backfill_fts_tokens().await.unwrap();

    let (tokens, pending): (String, bool) = sqlx::query_as(
        "select tsv_tokens, fts_retokenize_pending from memories \
             where namespace = $1 and id = $2",
    )
    .bind(ns)
    .bind(e.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(tokens, expected, "回填结果必须与写路径分词同源");
    assert!(!pending, "回填完成后标记必须清除");

    let pending_left: i64 =
        sqlx::query_scalar("select count(*) from memories where fts_retokenize_pending")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(pending_left, 0, "全部待处理行应被清零");

    // 中文子串经 FTS 召回（修复前 tsv_tokens=原文时不可命中）
    let hits: i64 = sqlx::query_scalar(
        "select count(*) from memories \
             where namespace = $1 and to_tsvector('simple', tsv_tokens) \
                   @@ to_tsquery('simple', '东京 | 区域')",
    )
    .bind(ns)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(hits >= 1, "重分词后中文子串应可经 FTS 命中");

    store.delete(ns, e.id).await.unwrap();
}

/// 迁移 015 + 瘦列契约 + 二进制协议的端到端验证：
/// - 写路径以 pgvector 二进制参数落库（INSERT）；
/// - `similar` 以 halfvec 二进制参数走表达式索引（迁移 015），返回瘦列
///   （不带 embedding）与 fp16 距离分数；
/// - `embeddings_by_ids` 按 id 二进制读回向量，且严格受命名空间隔离；
/// - merged 状态不进部分索引（迁移 015 谓词 `status in ('active','archived')`）。
#[tokio::test]
#[ignore = "需要 DATABASE_URL"]
async fn pg_vector_ann_halfvec_and_embeddings_by_ids() {
    let store = store().await;
    let ns = "it-ns-vec-halfvec";
    let other = "it-ns-vec-halfvec-other";
    store
        .ensure_namespace(ns, &AgentId::from("it-agent"))
        .await
        .unwrap();
    store
        .ensure_namespace(other, &AgentId::from("it-agent"))
        .await
        .unwrap();

    let model = "halfvec-it-embedder";
    let mut target = entry(ns, "it-agent", "东京区域的部署事实");
    target.embedding = Some(vec![1.0; 1024]);
    target.embedding_model = Some(model.to_string());
    store.insert(&target).await.unwrap();

    let mut noise = entry(ns, "it-agent", "无关的噪声条目");
    noise.embedding = Some(vec![-1.0; 1024]);
    noise.embedding_model = Some(model.to_string());
    store.insert(&noise).await.unwrap();

    // merged 行带向量也不该被 ANN 召回（部分索引谓词排除）
    let mut merged_row = entry(ns, "it-agent", "已被整合取代的历史条目");
    merged_row.embedding = Some(vec![1.0; 1024]);
    merged_row.embedding_model = Some(model.to_string());
    merged_row.status = MemoryStatus::Merged;
    store.insert(&merged_row).await.unwrap();

    let mut cross = entry(other, "it-agent", "其它命名空间的同向量条目");
    cross.embedding = Some(vec![1.0; 1024]);
    cross.embedding_model = Some(model.to_string());
    store.insert(&cross).await.unwrap();

    // ANN：查询向量与 target 同向
    let filter = MemoryFilter {
        namespace: Some(ns.to_string()),
        ..Default::default()
    };
    let hits = store.similar(&filter, &vec![1.0; 1024], 10).await.unwrap();
    assert!(!hits.is_empty(), "halfvec ANN 必须命中同向条目");
    assert_eq!(hits[0].item.id, target.id);
    // 瘦列契约：结果不带向量；分数接近 1（同向余弦）
    assert!(hits[0].item.embedding.is_none(), "similar 返回瘦列");
    assert!(
        (hits[0].score - 1.0).abs() < 1e-2,
        "同向条目的半精度余弦分数应接近 1：{}",
        hits[0].score
    );
    assert!(
        hits.iter().all(|h| h.item.id != merged_row.id),
        "merged 行不进部分索引，不应被召回"
    );

    // 二次取回：本命名空间内按 id 取回向量（二进制协议）
    let rows = store
        .embeddings_by_ids(ns, &[target.id, noise.id, merged_row.id])
        .await
        .unwrap();
    assert_eq!(rows.len(), 3, "三条都带向量（merged 只是索引不可见）");
    let got_target = rows.iter().find(|(id, _)| *id == target.id).unwrap();
    assert_eq!(got_target.1, vec![1.0; 1024]);

    // 硬隔离：换命名空间后同 id 集合取不到任何向量
    let rows_other = store
        .embeddings_by_ids(other, &[target.id, noise.id])
        .await
        .unwrap();
    assert!(
        rows_other.is_empty(),
        "embeddings_by_ids 不得跨命名空间取数"
    );

    for id in [target.id, noise.id, merged_row.id] {
        store.delete(ns, id).await.unwrap();
    }
    store.delete(other, cross.id).await.unwrap();
}
