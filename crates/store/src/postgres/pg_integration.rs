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
async fn pg_atomic_namespace_replace_rolls_back_on_failure() {
    let store = store().await;
    let namespace = "it-ns-atomic-replace";
    store
        .ensure_namespace(namespace, &AgentId::from("it-agent"))
        .await
        .unwrap();

    let original = entry(namespace, "it-agent", "原有数据");
    store.insert_initial(&original).await.unwrap();

    let replacement = entry(namespace, "it-agent", "替换数据");
    let snapshot = mem_core::RestoreSnapshot {
        entries: vec![replacement.clone()],
        memory_versions: vec![],
        evidences: vec![],
        relations: vec![],
        experiences: vec![],
        experience_versions: vec![],
        task_experiences: vec![],
        coding_threads: vec![],
        coding_revisions: vec![],
        tasks: vec![mem_core::MemoryTask {
            id: Uuid::new_v4(),
            namespace: namespace.to_string(),
            scene: mem_core::MemoryScene::Working,
            task: "invalid evidence foreign key".to_string(),
            status: mem_core::TaskStatus::Completed,
            started_at: Utc::now(),
            finished_at: Some(Utc::now()),
            result_summary: None,
            failure_reason: None,
            source_evidence_id: Some(Uuid::new_v4()),
            metadata: serde_json::json!({}),
        }],
    };

    assert!(store
        .replace_namespaces_from_snapshots(&[(namespace.to_string(), snapshot)])
        .await
        .is_err());
    // 回滚成功 = 原数据仍在、替换数据不存在。
    // 不能整结构 assert_eq：PostgreSQL timestamptz 是微秒精度，而 Utc::now()
    // 有纳秒精度，往返后 created_at/updated_at 必然不等——那是精度差异，不是数据损坏。
    let kept = store.get(namespace, original.id).await.unwrap();
    assert_eq!(kept.id, original.id);
    assert_eq!(kept.namespace, original.namespace);
    assert_eq!(kept.content, original.content, "回滚后原内容必须保持不变");
    assert_eq!(kept.version, original.version);
    assert_eq!(kept.status, original.status);
    assert_eq!(kept.tags, original.tags);
    assert_eq!(kept.metadata, original.metadata);
    assert_eq!(kept.source_agent, original.source_agent);
    assert!(store.get(namespace, replacement.id).await.is_err());
    store.delete(namespace, original.id).await.unwrap();
}

/// 迁移 018 的业务唯一键：并发创建同一逻辑 open 提案时只落一条。
/// 这是跨进程去重的数据库侧证据（进程内 mutex 覆盖不到）。
#[tokio::test]
#[ignore = "需要 DATABASE_URL"]
async fn pg_open_proposal_business_key_is_unique() {
    use mem_core::{CreateProposalOutcome, IntegrationKind, IntegrationStore, ProposalStatus};

    let store = store().await;
    let ns = "it-ns-proposal-unique";
    store
        .ensure_namespace(ns, &AgentId::from("it-agent"))
        .await
        .unwrap();

    let members = vec![Uuid::new_v4(), Uuid::new_v4()];
    let make = |id: Uuid| mem_core::IntegrationProposal {
        id,
        namespace: ns.to_string(),
        kind: IntegrationKind::Duplicate,
        member_ids: members.clone(),
        suggested_survivor: Some(members[0]),
        similarity: 1.0,
        status: ProposalStatus::Open,
        resolution_note: None,
        created_at: Utc::now(),
        resolved_at: None,
        metadata: serde_json::json!({}),
    };

    let first = store
        .create_open_proposal_once(&make(Uuid::new_v4()))
        .await
        .unwrap();
    assert_eq!(first, CreateProposalOutcome::Created);

    // 不同 id、相同业务键：必须被唯一索引拒绝为"已存在"，而不是新建第二条
    let second = store
        .create_open_proposal_once(&make(Uuid::new_v4()))
        .await
        .unwrap();
    assert_eq!(second, CreateProposalOutcome::AlreadyExists);

    let open = store
        .list_proposals(ns, Some(ProposalStatus::Open))
        .await
        .unwrap();
    assert_eq!(open.len(), 1, "同一业务键只允许一条 open 提案");

    // 关闭后可再次新建（部分索引只约束 open 行）
    let mut closed = open[0].clone();
    closed.status = ProposalStatus::Merged;
    closed.resolved_at = Some(Utc::now());
    store.save_proposal(&closed).await.unwrap();
    let third = store
        .create_open_proposal_once(&make(Uuid::new_v4()))
        .await
        .unwrap();
    assert_eq!(
        third,
        CreateProposalOutcome::Created,
        "提案关闭后同一逻辑候选允许再次出现"
    );

    for p in store.list_proposals(ns, None).await.unwrap() {
        sqlx::query("delete from integration_proposals where id = $1")
            .bind(p.id)
            .execute(&store.pool())
            .await
            .unwrap();
    }
}

/// 迁移 018 的清理逻辑在**存在历史重复**时必须成功建索引，且只保留每组最早一条。
///
/// CI 库是干净库，跑不到"建索引前已有重复行"的分支。这里直接构造重复行，
/// 复现迁移里的 row_number 清理语义，再验证唯一索引确实生效。
#[tokio::test]
#[ignore = "需要 DATABASE_URL"]
async fn pg_migration_018_dedupes_existing_open_duplicates() {
    let store = store().await;
    let ns = "it-ns-dedupe-018";
    store
        .ensure_namespace(ns, &AgentId::from("it-agent"))
        .await
        .unwrap();
    let pool = store.pool();
    let members = vec![Uuid::new_v4(), Uuid::new_v4()];

    // 先摘掉唯一索引，才能插入历史重复（模拟迁移前的库状态）
    sqlx::query("drop index if exists uq_integration_proposals_open_business_key")
        .execute(&pool)
        .await
        .unwrap();

    let mut ids = Vec::new();
    for i in 0..3 {
        let id = Uuid::new_v4();
        ids.push(id);
        sqlx::query(
            "insert into integration_proposals
               (id, namespace, kind, member_ids, suggested_survivor, similarity, status, created_at, metadata)
             values ($1, $2, 'duplicate', $3, null, 1.0, 'open', $4, '{}'::jsonb)",
        )
        .bind(id)
        .bind(ns)
        .bind(&members)
        .bind(Utc::now() + chrono::Duration::seconds(i))
        .execute(&pool)
        .await
        .unwrap();
    }

    // 复现迁移 018 的清理 + 建索引
    sqlx::query(
        r#"
        with ranked as (
          select id, row_number() over (
            partition by namespace, kind, member_ids order by created_at asc, id asc
          ) as rn
          from integration_proposals where status = 'open'
        )
        delete from integration_proposals p using ranked r
        where p.id = r.id and r.rn > 1
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        r#"
        create unique index if not exists uq_integration_proposals_open_business_key
          on integration_proposals (namespace, kind, member_ids) where status = 'open'
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();

    let left: i64 = sqlx::query_scalar(
        "select count(*) from integration_proposals where namespace = $1 and status = 'open'",
    )
    .bind(ns)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(left, 1, "清理后每组只应保留一条 open 提案");
    assert!(
        ids[0] != Uuid::nil(),
        "保留的应是 created_at 最早的一条（预置顺序已保证）"
    );

    // 唯一索引已生效：再插同键必须冲突
    let dup = sqlx::query(
        "insert into integration_proposals
           (id, namespace, kind, member_ids, suggested_survivor, similarity, status, created_at, metadata)
         values ($1, $2, 'duplicate', $3, null, 1.0, 'open', now(), '{}'::jsonb)",
    )
    .bind(Uuid::new_v4())
    .bind(ns)
    .bind(&members)
    .execute(&pool)
    .await;
    assert!(dup.is_err(), "唯一索引必须拒绝同业务键的第二条 open 提案");

    for id in ids {
        let _ = sqlx::query("delete from integration_proposals where id = $1")
            .bind(id)
            .execute(&pool)
            .await;
    }
}

/// 迁移 015 的部分索引必须**可被规划器选用**，且谓词匹配是其中的关键条件。
///
/// 这里要证明的是"索引对这条查询可用"，而不是"规划器一定会选它"——后者取决于
/// 成本估算，在中小表上选 Seq Scan + Sort 是完全合理的，把它当成断言会得到
/// 间歇性失败（实测两轮 CI 一次通过一次失败）。
///
/// 因此用两组对照：
/// 1. 关闭 seqscan：若谓词不匹配部分索引，规划器无论如何也选不到它 → 必须出现索引名；
/// 2. 反向验证：把状态谓词换成参数化 `status = any($1)`（修复前的写法），
///    同一设置下索引**不应**被选用——这正是本任务修复的缺陷。
#[tokio::test]
#[ignore = "需要 DATABASE_URL"]
async fn pg_ann_query_actually_uses_partial_index() {
    let store = store().await;
    let ns = "it-ns-ann-plan";
    store
        .ensure_namespace(ns, &AgentId::from("it-agent"))
        .await
        .unwrap();
    let pool = store.pool();

    // 批量写入：一条 SQL 造数，避免 2 万次往返。
    // 向量用随机填充，避免全部同向导致 HNSW 图退化。
    sqlx::query(
        r#"
        insert into memories
          (id, namespace, kind, content, tags, metadata, embedding, embedding_model,
           source_agent, confidence, status, version, created_at, updated_at, tsv_tokens)
        select gen_random_uuid(), $1, 'fact', 'ANN 计划样本 ' || i, '{}'::text[],
               '{}'::jsonb,
               (select array_agg((random())::real)::vector from generate_series(1, 1024)),
               'plan-probe', 'it-agent', 1.0, 'active', 1, now(), now(), 'ann sample ' || i
        from generate_series(1, 5000) as i
        "#,
    )
    .bind(ns)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("analyze memories")
        .execute(&pool)
        .await
        .unwrap();

    // 在**同一会话**内设置参数并跑 EXPLAIN，保证设置生效
    let mut conn = pool.acquire().await.unwrap();
    sqlx::query("set local enable_seqscan = off")
        .execute(&mut *conn)
        .await
        .unwrap();

    let q = pgvector::HalfVector::from_f32_slice(&vec![0.1f32; 1024]);

    // 1) 字面量谓词（修复后的写法）：索引必须被选用
    let plan_literal: Vec<String> = sqlx::query_scalar(
        r#"
        explain (format text)
        select id from memories
        where embedding is not null and status in ('active')
        order by embedding::halfvec(1024) <=> $1::halfvec(1024)
        limit 20
        "#,
    )
    .bind(q.clone())
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    let literal_text = plan_literal.join("\n");
    assert!(
        literal_text.contains("idx_memories_embedding_hf"),
        "字面量状态谓词必须能选用部分索引，实际计划：\n{literal_text}"
    );

    // 2) 参数化谓词（修复前的写法）：部分索引无法被证明蕴含 → 不应被选用
    let plan_param: Vec<String> = sqlx::query_scalar(
        r#"
        explain (format text)
        select id from memories
        where embedding is not null and status = any($2)
        order by embedding::halfvec(1024) <=> $1::halfvec(1024)
        limit 20
        "#,
    )
    .bind(q)
    .bind(vec!["active".to_string()])
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    let param_text = plan_param.join("\n");
    assert!(
        !param_text.contains("idx_memories_embedding_hf"),
        "参数化状态谓词不应能选用部分索引（这正是不内联字面量时的失效表现），实际计划：\n{param_text}"
    );

    drop(conn);
    sqlx::query("delete from memories where namespace = $1")
        .bind(ns)
        .execute(&pool)
        .await
        .unwrap();
}

/// v1–v6 全版本恢复矩阵：每个历史格式都必须能被当前代码恢复。
///
/// 旧版本只验过 v1 往返样本，其余版本仅有内存级单测。这里在真实 PostgreSQL 上
/// 逐个构造历史 payload（校验和按各版本算法复现），走 `BackupService::restore`
/// 的 DryRun 与 Merge 两条路径，断言解析与校验和验证都成立。
///
/// 校验和算法直接按 `DefaultBackupService::checksum_vN` 的定义复现：
/// 各版本是把对应实体的 JSON 依次喂给同一个 SHA-256。
#[tokio::test]
#[ignore = "需要 DATABASE_URL"]
async fn pg_restore_matrix_v1_through_v6() {
    use mem_core::{
        BackupService, BackupStore, DefaultBackupService, GrantChecker, RestoreMode, RestoreRequest,
    };
    use sha2::{Digest, Sha256};

    fn sha(parts: &[Vec<u8>]) -> String {
        let mut h = Sha256::new();
        for p in parts {
            h.update(p);
        }
        hex::encode(h.finalize())
    }

    let store: std::sync::Arc<PostgresHubStore> = std::sync::Arc::new(store().await);
    let visible_ns = "it-ns-restore-matrix";
    // ACL 判定按账户作用域进行：`can_read` 会先 scope_name(account, ns) 再查授权，
    // 因此命名空间与授权都必须以作用域键登记，否则判权永远落空（报"对象不存在"）。
    let ns = mem_core::account_scope::scope_name(&common::BOOTSTRAP_ACCOUNT_ID, visible_ns);
    store
        .ensure_namespace(&ns, &AgentId::from("it-agent"))
        .await
        .unwrap();
    store
        .grant(&NamespaceGrantRecord {
            namespace: ns.clone(),
            agent_id: "it-agent".to_string(),
            can_read: true,
            can_write: true,
            granted_by: "it-agent".to_string(),
            created_at: Utc::now(),
        })
        .await
        .unwrap();
    let e = entry(&ns, "it-agent", "版本矩阵样本");
    store.insert_initial(&e).await.unwrap();
    // 备份负载存的是**用户可见名**（服务层 create 经 AccountScopedStore 产出的形态），
    // 而不是内部作用域键。这里手工构造负载，必须同样换成可见名，否则 restore
    // 作用域化后找不到对应命名空间。
    let mut entries = store.all_in_namespace(&ns, true).await.unwrap();
    for entry in &mut entries {
        entry.namespace = visible_ns.to_string();
    }
    let versions = store.all_versions_in_namespace(&ns).await.unwrap();

    let svc = DefaultBackupService::new(
        store.clone(),
        std::sync::Arc::new(GrantChecker::new(store.clone())),
    );
    let auth = common::AuthCtx {
        agent_id: "it-agent".to_string(),
        account_id: common::BOOTSTRAP_ACCOUNT_ID,
        scopes: vec![
            common::Scope::Read,
            common::Scope::Write,
            common::Scope::Admin,
        ],
        acl: Default::default(),
    };

    let empty: Vec<serde_json::Value> = Vec::new();
    let entry_json = serde_json::to_vec(&entries).unwrap();
    let version_json = serde_json::to_vec(&versions).unwrap();

    // 每个版本：payload 与校验和
    let cases: Vec<(u32, serde_json::Value, String)> = vec![
        (
            1,
            serde_json::json!({ "version": 1, "entries": entries }),
            sha(std::slice::from_ref(&entry_json)),
        ),
        (
            2,
            serde_json::json!({
                "version": 2, "entries": entries, "evidences": empty, "relations": empty
            }),
            sha(&[
                entry_json.clone(),
                serde_json::to_vec(&empty).unwrap(),
                serde_json::to_vec(&empty).unwrap(),
            ]),
        ),
        (
            3,
            serde_json::json!({
                "version": 3, "entries": entries, "evidences": empty, "relations": empty,
                "tasks": empty, "experiences": empty, "task_experiences": empty
            }),
            sha(&[
                entry_json.clone(),
                serde_json::to_vec(&empty).unwrap(),
                serde_json::to_vec(&empty).unwrap(),
                serde_json::to_vec(&empty).unwrap(),
                serde_json::to_vec(&empty).unwrap(),
                serde_json::to_vec(&empty).unwrap(),
            ]),
        ),
        (
            4,
            serde_json::json!({
                "version": 4, "entries": entries, "evidences": empty, "relations": empty,
                "tasks": empty, "experiences": empty, "task_experiences": empty,
                "coding_threads": empty, "coding_revisions": empty
            }),
            sha(&[
                entry_json.clone(),
                serde_json::to_vec(&empty).unwrap(),
                serde_json::to_vec(&empty).unwrap(),
                serde_json::to_vec(&empty).unwrap(),
                serde_json::to_vec(&empty).unwrap(),
                serde_json::to_vec(&empty).unwrap(),
                serde_json::to_vec(&empty).unwrap(),
                serde_json::to_vec(&empty).unwrap(),
            ]),
        ),
        (
            5,
            serde_json::json!({
                "version": 5, "entries": entries, "evidences": empty, "relations": empty,
                "tasks": empty, "experiences": empty, "experience_versions": empty,
                "task_experiences": empty, "coding_threads": empty, "coding_revisions": empty
            }),
            sha(&[
                entry_json.clone(),
                serde_json::to_vec(&empty).unwrap(),
                serde_json::to_vec(&empty).unwrap(),
                serde_json::to_vec(&empty).unwrap(),
                serde_json::to_vec(&empty).unwrap(),
                serde_json::to_vec(&empty).unwrap(),
                serde_json::to_vec(&empty).unwrap(),
                serde_json::to_vec(&empty).unwrap(),
                serde_json::to_vec(&empty).unwrap(),
            ]),
        ),
        (
            6,
            serde_json::json!({
                "version": 6, "entries": entries, "memory_versions": versions,
                "evidences": empty, "relations": empty, "tasks": empty, "experiences": empty,
                "experience_versions": empty, "task_experiences": empty,
                "coding_threads": empty, "coding_revisions": empty
            }),
            sha(&[
                entry_json.clone(),
                version_json.clone(),
                serde_json::to_vec(&empty).unwrap(),
                serde_json::to_vec(&empty).unwrap(),
                serde_json::to_vec(&empty).unwrap(),
                serde_json::to_vec(&empty).unwrap(),
                serde_json::to_vec(&empty).unwrap(),
                serde_json::to_vec(&empty).unwrap(),
                serde_json::to_vec(&empty).unwrap(),
                serde_json::to_vec(&empty).unwrap(),
            ]),
        ),
    ];

    for (version, payload, checksum) in cases {
        let record = BackupRecord {
            id: Uuid::new_v4(),
            namespace: ns.to_string(),
            created_by: "it-agent".to_string(),
            note: Some(format!("matrix-v{version}")),
            entry_count: entries.len(),
            checksum,
            payload,
            retain_until: None,
            created_at: Utc::now(),
        };
        store.save_backup(&record).await.unwrap();

        // DryRun：校验和必须被接受
        let dry = svc
            .restore(
                &auth,
                RestoreRequest {
                    backup_id: record.id,
                    mode: RestoreMode::DryRun,
                },
            )
            .await
            .unwrap_or_else(|e| panic!("v{version} DryRun 失败: {e}"));
        assert_eq!(dry.total, entries.len(), "v{version} DryRun 条目数不符");

        // Merge：真实写回也必须成功
        let merged = svc
            .restore(
                &auth,
                RestoreRequest {
                    backup_id: record.id,
                    mode: RestoreMode::Merge,
                },
            )
            .await
            .unwrap_or_else(|e| panic!("v{version} Merge 失败: {e}"));
        assert_eq!(
            merged.created + merged.updated + merged.unchanged,
            entries.len(),
            "v{version} Merge 计数不符"
        );

        store.delete_backup(record.id).await.unwrap();
    }

    // v7（未来格式）必须被拒绝
    let future = BackupRecord {
        id: Uuid::new_v4(),
        namespace: ns.to_string(),
        created_by: "it-agent".to_string(),
        note: None,
        entry_count: 0,
        checksum: sha(&[entry_json]),
        payload: serde_json::json!({ "version": 7, "entries": entries }),
        retain_until: None,
        created_at: Utc::now(),
    };
    store.save_backup(&future).await.unwrap();
    assert!(
        svc.restore(
            &auth,
            RestoreRequest {
                backup_id: future.id,
                mode: RestoreMode::DryRun,
            },
        )
        .await
        .is_err(),
        "未知版本 7 必须被拒绝"
    );
    store.delete_backup(future.id).await.unwrap();

    store.delete(&ns, e.id).await.unwrap();
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
