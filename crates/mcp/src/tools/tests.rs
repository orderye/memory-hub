use super::*;
use consolidation::DefaultIntegrationService;
use mem_core::{DefaultBackupService, GrantChecker};
use store::InMemoryHubStore;
use uuid::Uuid;

fn harness() -> (ToolDispatcher, AuthCtx) {
    harness_with_min_score(None)
}

/// 构造带默认拒绝阈值的分发器（与生产默认一致）
fn harness_with_min_score(min_score: Option<f32>) -> (ToolDispatcher, AuthCtx) {
    let store = Arc::new(InMemoryHubStore::new());
    let access = Arc::new(GrantChecker::new(store.clone()));
    let embedder = Arc::new(mem_core::TestEmbedder::default());
    let retriever = Arc::new(
        retrieval::HybridRetriever::new(
            store.clone() as Arc<dyn mem_core::MemoryStore>,
            embedder.clone(),
            100,
            100,
            [0.5, 0.2, 0.1, 0.1, 0.1],
            0.7,
            500,
        )
        .with_default_min_score(min_score),
    );
    let dispatcher = ToolDispatcher::new(
        Arc::new(
            mem_core::DefaultMemoryService::new(store.clone(), embedder, access.clone())
                .with_retriever(retriever),
        ),
        Arc::new(DefaultBackupService::new(store.clone(), access.clone())),
        Arc::new(DefaultIntegrationService::new(
            store.clone(),
            access,
            0.99,
            100,
        )),
        store.clone(),
    );
    let auth = AuthCtx {
        agent_id: "agent-a".to_string(),
        account_id: common::BOOTSTRAP_ACCOUNT_ID,
        scopes: vec![Scope::Read, Scope::Write, Scope::Admin],
        acl: Default::default(),
    };
    (dispatcher, auth)
}

fn arg(v: Value) -> Value {
    v
}

/// 检索无结果时必须**显式**表达"无可靠匹配"
///
/// 回归：此前只返回空数组，调用方会把"没找到可靠的"误读成
/// "库里没有这条记忆"或"检索坏了"，进而自行编造事实。
#[tokio::test]
async fn test_search_reports_no_reliable_match() {
    // 生产默认：调用方不传 min_score 时由服务端阈值把关
    let (d, auth) = harness_with_min_score(Some(0.65));
    // 先写一条注册命名空间（命名空间不存在时检索是 fail-closed 的）
    let put = d
        .dispatch(
            &auth,
            "memory_put",
            arg(json!({
                "namespace": "team-x",
                "kind": "fact",
                "content": "服务端口 3000",
            })),
        )
        .await;
    assert!(!put.is_error);

    // 查询一个完全无关的主题
    let res = d
        .dispatch(
            &auth,
            "memory_search",
            arg(json!({
                "namespace": "team-x",
                "query_text": "完全不相关的主题：火星基地氧气循环",
            })),
        )
        .await;
    assert!(!res.is_error, "检索不应报错：{:?}", res.content);
    let body = res.structured_content.unwrap();
    assert_eq!(body["results"], json!([]));
    assert_eq!(
        body["no_reliable_match"],
        json!(true),
        "空结果必须显式声明无可靠匹配：{body}"
    );
    assert!(
        body["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("可靠匹配"),
        "应给出可操作的说明：{body}"
    );
}

/// 检索有结果时不得声明"无可靠匹配"
#[tokio::test]
async fn test_search_with_hit_does_not_claim_no_reliable_match() {
    let (d, auth) = harness();
    let put = d
        .dispatch(
            &auth,
            "memory_put",
            arg(json!({
                "namespace": "team-x",
                "kind": "fact",
                "content": "服务端口 3000",
            })),
        )
        .await;
    assert!(!put.is_error);

    let res = d
        .dispatch(
            &auth,
            "memory_search",
            arg(json!({ "namespace": "team-x", "query_text": "服务端口" })),
        )
        .await;
    assert!(!res.is_error);
    let body = res.structured_content.unwrap();
    assert_eq!(body["no_reliable_match"], json!(false));
    assert!(!body["results"].as_array().unwrap().is_empty());
}

/// 检索响应必须自报能力状态：没有向量能力时 keyword_only=true。
/// 调用方据此区分"这就是纯关键词模式"与"向量服务临时故障降级"。
#[tokio::test]
async fn test_search_reports_capability_state() {
    let (d, auth) = harness();
    let put = d
        .dispatch(
            &auth,
            "memory_put",
            arg(json!({
                "namespace": "team-x",
                "kind": "fact",
                "content": "服务端口 3000",
            })),
        )
        .await;
    assert!(!put.is_error);

    let res = d
        .dispatch(
            &auth,
            "memory_search",
            arg(json!({ "namespace": "team-x", "query_text": "服务端口" })),
        )
        .await;
    assert!(!res.is_error);
    let body = res.structured_content.unwrap();
    assert_eq!(
        body["capabilities"]["vector_path_enabled"],
        json!(false),
        "未注入嵌入器时向量路必须声明为关闭：{body}"
    );
    assert_eq!(body["capabilities"]["keyword_only"], json!(true));
    assert_eq!(body["capabilities"]["semantic_ready"], json!(false));
}

/// 整合统计缓存：第二次调用必须命中缓存并如实标注 cached=true
#[tokio::test]
async fn test_consolidation_stats_are_cached_and_marked() {
    let (d, auth) = harness();
    let put = d
        .dispatch(
            &auth,
            "memory_put",
            arg(json!({
                "namespace": "team-x",
                "kind": "fact",
                "content": "服务端口 3000",
            })),
        )
        .await;
    assert!(!put.is_error);

    let first = d
        .dispatch(
            &auth,
            "memory_consolidation_stats",
            arg(json!({ "namespace": "team-x" })),
        )
        .await;
    assert!(!first.is_error);
    let first_body = first.structured_content.unwrap();
    assert_eq!(first_body["cached"], json!(false), "首次必须实际计算");

    let second = d
        .dispatch(
            &auth,
            "memory_consolidation_stats",
            arg(json!({ "namespace": "team-x" })),
        )
        .await;
    let second_body = second.structured_content.unwrap();
    assert_eq!(second_body["cached"], json!(true), "TTL 内必须命中缓存");
    assert_eq!(
        second_body["total_active"], first_body["total_active"],
        "缓存命中不得改变统计内容"
    );
}

#[tokio::test]
async fn test_put_get_list_roundtrip() {
    let (d, auth) = harness();
    let put = d
        .dispatch(
            &auth,
            "memory_put",
            arg(json!({
                "namespace": "team-x",
                "kind": "fact",
                "content": "服务端口 3000",
                "tags": ["ops"],
            })),
        )
        .await;
    assert!(!put.is_error, "put 应成功: {:?}", put);
    let id = put.structured_content.unwrap()["memory"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let get = d
        .dispatch(
            &auth,
            "memory_get",
            arg(json!({ "namespace": "team-x", "id": id })),
        )
        .await;
    assert!(!get.is_error);
    assert_eq!(
        get.structured_content.unwrap()["memory"]["content"],
        json!("服务端口 3000")
    );

    let list = d
        .dispatch(&auth, "memory_list", arg(json!({ "namespace": "team-x" })))
        .await;
    assert!(!list.is_error);
    assert_eq!(
        list.structured_content.unwrap()["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn test_put_by_key_upserts() {
    let (d, auth) = harness();
    let r1 = d
        .dispatch(
            &auth,
            "memory_put",
            arg(json!({
                "namespace": "ns", "key": "db:engine", "kind": "fact", "content": "MySQL",
            })),
        )
        .await;
    let id1 = r1.structured_content.unwrap()["memory"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let r2 = d
        .dispatch(
            &auth,
            "memory_put",
            arg(json!({
                "namespace": "ns", "key": "db:engine", "kind": "fact", "content": "PostgreSQL",
            })),
        )
        .await;
    let payload = r2.structured_content.unwrap();
    assert_eq!(payload["memory"]["id"], json!(id1), "同 key 必须 upsert");
    assert_eq!(payload["memory"]["version"], json!(2));
}

#[tokio::test]
async fn test_search_and_stats() {
    let (d, auth) = harness();
    d.dispatch(
        &auth,
        "memory_put",
        arg(json!({ "namespace": "ns", "kind": "fact", "content": "用户喜欢美式咖啡" })),
    )
    .await;
    d.dispatch(
        &auth,
        "memory_put",
        arg(json!({ "namespace": "ns", "kind": "preference", "content": "偏好深色模式" })),
    )
    .await;

    let search = d
        .dispatch(
            &auth,
            "memory_search",
            arg(json!({ "namespace": "ns", "query_text": "咖啡", "top_k": 5 })),
        )
        .await;
    assert!(!search.is_error);
    let results = search.structured_content.unwrap()["results"]
        .as_array()
        .unwrap()
        .clone();
    assert!(!results.is_empty());
    // 关键词命中的条目必须排在首位（向量路可能补充语义近邻）
    assert!(results[0]["memory"]["content"]
        .as_str()
        .unwrap()
        .contains("咖啡"));

    let stats = d
        .dispatch(&auth, "memory_stats", arg(json!({ "namespace": "ns" })))
        .await;
    let stats = stats.structured_content.unwrap();
    assert_eq!(stats["total"], json!(2));
    assert_eq!(stats["by_kind"]["fact"], json!(1));
    assert_eq!(stats["by_kind"]["preference"], json!(1));
}

#[tokio::test]
async fn test_update_and_delete_flow() {
    let (d, auth) = harness();
    let put = d
        .dispatch(
            &auth,
            "memory_put",
            arg(json!({ "namespace": "ns", "kind": "note", "content": "初版" })),
        )
        .await;
    let id = put.structured_content.unwrap()["memory"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let upd = d
        .dispatch(
            &auth,
            "memory_update",
            arg(json!({ "namespace": "ns", "id": id, "content": "修订版", "confidence": 0.9 })),
        )
        .await;
    assert!(!upd.is_error);
    assert_eq!(
        upd.structured_content.unwrap()["memory"]["version"],
        json!(2)
    );

    let del = d
        .dispatch(
            &auth,
            "memory_delete",
            arg(json!({ "namespace": "ns", "id": id })),
        )
        .await;
    assert!(!del.is_error);

    // 已归档条目再次删除应报错
    let del2 = d
        .dispatch(
            &auth,
            "memory_delete",
            arg(json!({ "namespace": "ns", "id": id })),
        )
        .await;
    assert!(del2.is_error);
}

#[tokio::test]
async fn test_backup_and_restore_via_tools() {
    let (d, auth) = harness();
    d.dispatch(
        &auth,
        "memory_put",
        arg(json!({ "namespace": "ns", "kind": "fact", "content": "关键配置 A" })),
    )
    .await;

    let backup = d
        .dispatch(
            &auth,
            "memory_backup_create",
            arg(json!({ "namespace": "ns", "note": "变更前" })),
        )
        .await;
    assert!(!backup.is_error);
    let backup_id = backup.structured_content.unwrap()["backup"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let list = d
        .dispatch(
            &auth,
            "memory_backup_list",
            arg(json!({ "namespace": "ns" })),
        )
        .await;
    assert_eq!(
        list.structured_content.unwrap()["backups"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let dry = d
        .dispatch(
            &auth,
            "memory_restore",
            arg(json!({ "backup_id": backup_id, "mode": "dry_run" })),
        )
        .await;
    assert!(!dry.is_error);
    assert_eq!(
        dry.structured_content.unwrap()["unchanged"],
        json!(1),
        "dry_run 应识别 1 条未变化"
    );
}

#[tokio::test]
async fn test_integrate_scan_and_resolve_via_tools() {
    let (d, auth) = harness();
    // 两个不同 key 的相同内容 → 嵌入完全相同 → 重复提案
    d.dispatch(
        &auth,
        "memory_put",
        arg(
            json!({ "namespace": "ns", "key": "k:1", "kind": "fact", "content": "服务部署在东京" }),
        ),
    )
    .await;
    d.dispatch(
        &auth,
        "memory_put",
        arg(
            json!({ "namespace": "ns", "key": "k:2", "kind": "fact", "content": "服务部署在东京" }),
        ),
    )
    .await;

    let scan = d
        .dispatch(
            &auth,
            "memory_integrate_scan",
            arg(json!({ "namespace": "ns", "dry_run": false })),
        )
        .await;
    assert!(!scan.is_error, "扫描应成功: {:?}", scan);
    let proposals = scan.structured_content.unwrap()["proposals"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(proposals.len(), 1, "应识别出 1 个重复提案");
    let proposal_id = proposals[0]["id"].as_str().unwrap().to_string();
    let survivor = proposals[0]["member_ids"][0].as_str().unwrap().to_string();

    let resolve = d
        .dispatch(
            &auth,
            "memory_integration_resolve",
            arg(json!({
                "proposal_id": proposal_id,
                "action": "merge",
                "survivor_id": survivor,
            })),
        )
        .await;
    assert!(!resolve.is_error, "合并应成功: {:?}", resolve);

    let list = d
        .dispatch(
            &auth,
            "memory_integration_list",
            arg(json!({ "namespace": "ns" })),
        )
        .await;
    assert!(!list.is_error);
    let after = list.structured_content.unwrap()["proposals"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0]["status"], json!("merged"));
}

#[tokio::test]
async fn test_consolidation_hub_tools() {
    let (d, auth) = harness();
    d.dispatch(
            &auth,
            "memory_put",
            arg(json!({ "namespace": "team-hub", "kind": "fact", "content": "集群配置：节点规格 8C16G" })),
        )
        .await;
    d.dispatch(
            &auth,
            "memory_put",
            arg(json!({ "namespace": "team-hub", "kind": "fact", "content": "集群配置：节点规格 8C16G" })),
        )
        .await;
    let stats = d
        .dispatch(
            &auth,
            "memory_consolidation_stats",
            arg(json!({ "namespace": "team-hub" })),
        )
        .await;
    assert!(!stats.is_error, "统计应成功: {:?}", stats);
    let sc = stats.structured_content.unwrap();
    assert_eq!(sc["total_active"], 2);

    let consolidate = d
        .dispatch(
            &auth,
            "memory_consolidate",
            arg(json!({ "namespace": "team-hub", "dry_run": false, "auto_merge": true })),
        )
        .await;
    assert!(!consolidate.is_error, "整合中枢应成功: {:?}", consolidate);
    let report = consolidate.structured_content.unwrap();
    assert_eq!(report["auto_merged"], 1);
}

#[tokio::test]
async fn test_unknown_tool_returns_error() {
    let (d, auth) = harness();
    let r = d.dispatch(&auth, "no_such_tool", json!({})).await;
    assert!(r.is_error);
}

#[tokio::test]
async fn test_bad_arguments_return_error() {
    let (d, auth) = harness();
    let r = d
        .dispatch(
            &auth,
            "memory_put",
            arg(json!({ "namespace": "ns", "kind": "not-a-kind", "content": "x" })),
        )
        .await;
    assert!(r.is_error);
}

#[tokio::test]
async fn test_embedding_stripped_from_output() {
    let (d, auth) = harness();
    let put = d
        .dispatch(
            &auth,
            "memory_put",
            arg(json!({ "namespace": "ns", "kind": "fact", "content": "有向量的条目" })),
        )
        .await;
    let payload = put.structured_content.unwrap();
    assert!(
        payload["memory"].get("embedding").is_none(),
        "输出不得包含 embedding 向量"
    );
}

#[tokio::test]
async fn test_grant_and_revoke_via_tools() {
    let (d, auth) = harness();
    // 沙箱语义：授权双方都必须已注册且同属一个账户
    for id in ["agent-a", "agent-b"] {
        d.store
            .register_agent(&mem_core::AgentRegistration {
                agent_id: id.to_string(),
                name: id.to_string(),
                kind: "assistant".to_string(),
                owner_account_id: common::BOOTSTRAP_ACCOUNT_ID,
                client_secret: None,
                redirect_uris: vec![],
                max_scopes: vec![Scope::Read, Scope::Write, Scope::Admin],
            })
            .await
            .unwrap();
    }
    // 先建命名空间
    d.dispatch(
        &auth,
        "memory_put",
        arg(json!({ "namespace": "shared", "kind": "note", "content": "共享笔记" })),
    )
    .await;

    let g = d
        .dispatch(
            &auth,
            "memory_grant",
            arg(json!({
                "namespace": "shared",
                "agent_id": "agent-b",
                "can_read": true,
                "can_write": true,
            })),
        )
        .await;
    assert!(!g.is_error, "owner 应可授权: {:?}", g);

    // agent-b 现在可以读写 shared
    let b = AuthCtx {
        agent_id: "agent-b".to_string(),
        account_id: common::BOOTSTRAP_ACCOUNT_ID,
        scopes: vec![Scope::Read, Scope::Write],
        acl: Default::default(),
    };
    let put_b = d
        .dispatch(
            &b,
            "memory_put",
            arg(json!({ "namespace": "shared", "kind": "note", "content": "B 的写入" })),
        )
        .await;
    assert!(!put_b.is_error, "被授权 Agent 应可写");

    // 撤销后不可写
    let rev = d
        .dispatch(
            &auth,
            "memory_revoke",
            arg(json!({ "namespace": "shared", "agent_id": "agent-b" })),
        )
        .await;
    assert!(!rev.is_error);
    let put_b2 = d
        .dispatch(
            &b,
            "memory_put",
            arg(json!({ "namespace": "shared", "kind": "note", "content": "再次写入" })),
        )
        .await;
    assert!(put_b2.is_error, "撤销后应不可写");

    // 撤销后读与搜索同样必须被拒绝
    let get_b = d
        .dispatch(
            &b,
            "memory_get",
            arg(json!({ "namespace": "shared", "key": "any" })),
        )
        .await;
    assert!(get_b.is_error, "撤销后应不可读");

    let search_b = d
        .dispatch(
            &b,
            "memory_search",
            arg(json!({ "namespace": "shared", "query_text": "笔记" })),
        )
        .await;
    assert!(search_b.is_error, "撤销后应不可搜");
}

#[tokio::test]
async fn test_grant_rejects_cross_account_and_wildcard() {
    let (d, auth) = harness();
    d.store
        .register_agent(&mem_core::AgentRegistration {
            agent_id: "agent-a".to_string(),
            name: "A".to_string(),
            kind: "assistant".to_string(),
            owner_account_id: common::BOOTSTRAP_ACCOUNT_ID,
            client_secret: None,
            redirect_uris: vec![],
            max_scopes: vec![Scope::Read, Scope::Write, Scope::Admin],
        })
        .await
        .unwrap();
    d.dispatch(
        &auth,
        "memory_put",
        arg(json!({ "namespace": "shared", "kind": "note", "content": "笔记" })),
    )
    .await;

    // 跨账户目标：agent-other 属于另一个账户 → 拒绝
    d.store
        .register_agent(&mem_core::AgentRegistration {
            agent_id: "agent-other".to_string(),
            name: "Other".to_string(),
            kind: "assistant".to_string(),
            owner_account_id: Uuid::from_u128(999),
            client_secret: None,
            redirect_uris: vec![],
            max_scopes: vec![Scope::Read],
        })
        .await
        .unwrap();
    let cross = d
        .dispatch(
            &auth,
            "memory_grant",
            arg(json!({
                "namespace": "shared",
                "agent_id": "agent-other",
                "can_read": true,
            })),
        )
        .await;
    assert!(cross.is_error, "跨账户授权必须被拒绝: {:?}", cross);

    // 通配命名空间授权 → 拒绝（沙箱废除 `*`）
    let wildcard = d
        .dispatch(
            &auth,
            "memory_grant",
            arg(json!({
                "namespace": "*",
                "agent_id": "agent-a",
                "can_read": true,
            })),
        )
        .await;
    assert!(wildcard.is_error, "通配授权必须被拒绝");
}

#[tokio::test]
async fn test_grant_rejected_for_non_owner() {
    let (d, auth) = harness();
    d.dispatch(
        &auth,
        "memory_put",
        arg(json!({ "namespace": "owned-by-a", "kind": "note", "content": "A 的" })),
    )
    .await;

    // agent-c 虽然持有 admin scope，但不是 owner 也不能通配授权 → 拒绝
    let c = AuthCtx {
        agent_id: "agent-c".to_string(),
        account_id: common::BOOTSTRAP_ACCOUNT_ID,
        scopes: vec![Scope::Read, Scope::Write, Scope::Admin],
        acl: Default::default(),
    };
    let g = d
        .dispatch(
            &c,
            "memory_grant",
            arg(json!({
                "namespace": "owned-by-a",
                "agent_id": "agent-c",
                "can_read": true,
            })),
        )
        .await;
    assert!(g.is_error, "非 owner 不得转授");
}

#[tokio::test]
async fn test_history_tool_returns_versions() {
    let (d, auth) = harness();
    let put = d
        .dispatch(
            &auth,
            "memory_put",
            arg(json!({ "namespace": "ns", "kind": "note", "content": "v1" })),
        )
        .await;
    let id = put.structured_content.unwrap()["memory"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    d.dispatch(
        &auth,
        "memory_update",
        arg(json!({ "namespace": "ns", "id": id, "content": "v2" })),
    )
    .await;

    let h = d
        .dispatch(
            &auth,
            "memory_history",
            arg(json!({ "namespace": "ns", "id": id })),
        )
        .await;
    assert!(!h.is_error);
    let versions = h.structured_content.unwrap()["versions"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(versions.len(), 2);
}

#[tokio::test]
async fn test_working_memory_tools_lifecycle_and_scope() {
    let (d, auth) = harness();

    // 1. 无写权限时 working_task_start 应失败
    let read_only_auth = AuthCtx {
        agent_id: "agent-ro".to_string(),
        account_id: common::BOOTSTRAP_ACCOUNT_ID,
        scopes: vec![Scope::Read],
        acl: Default::default(),
    };
    let denied = d
        .dispatch(
            &read_only_auth,
            "working_task_start",
            arg(json!({
                "namespace": "work-ns",
                "task": "排查内存泄漏",
            })),
        )
        .await;
    assert!(denied.is_error, "缺少 write scope 应拒绝");

    // 2. 正常启动任务
    let start_res = d
        .dispatch(
            &auth,
            "working_task_start",
            arg(json!({
                "namespace": "work-ns",
                "task": "排查内存泄漏",
            })),
        )
        .await;
    assert!(!start_res.is_error, "任务启动失败: {:?}", start_res);
    let task_id = start_res.structured_content.unwrap()["task"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // 3. 正常结束任务
    let finish_res = d
        .dispatch(
            &auth,
            "working_task_finish",
            arg(json!({
                "namespace": "work-ns",
                "task_id": task_id,
                "status": "completed",
                "result_summary": "定位并修复循环引用",
            })),
        )
        .await;
    assert!(!finish_res.is_error, "任务结束失败: {:?}", finish_res);

    // 4. 经验提炼
    let distill_res = d
        .dispatch(
            &auth,
            "working_experience_distill",
            arg(json!({
                "namespace": "work-ns",
                "task": "排查内存泄漏",
                "solution": "使用 valgrind 或 heaptrack 抓取堆栈分析泄漏点",
                "task_id": task_id,
                "search_keys": ["memory_leak", "heaptrack"],
                "preconditions": ["linux环境"],
                "boundary_envs": ["os:linux"],
                "failure_modes": ["未安装调试符号"],
                "validation": "heaptrack 确认泄漏点并复测通过",
                "confidence": 0.95,
            })),
        )
        .await;
    assert!(!distill_res.is_error, "经验提炼失败: {:?}", distill_res);
    let exp_id = distill_res.structured_content.unwrap()["experience"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // 5. 经验效用反馈
    let fb_res = d
        .dispatch(
            &auth,
            "working_experience_feedback",
            arg(json!({
                "namespace": "work-ns",
                "experience_id": exp_id,
                "score": 1.0,
            })),
        )
        .await;
    assert!(!fb_res.is_error, "反馈失败: {:?}", fb_res);

    // 6. 经验检索：分词部分匹配 + 环境边界匹配
    //    （查询不是正文的整段子串，旧实现会整条漏召回）
    let search_res = d
        .dispatch(
            &auth,
            "working_experience_search",
            arg(json!({
                "namespace": "work-ns",
                "query": "内存泄漏排查",
                "env": "os:linux",
                "top_k": 5,
            })),
        )
        .await;
    assert!(!search_res.is_error, "经验检索失败: {:?}", search_res);
    let results = search_res.structured_content.unwrap();
    let results = results["results"].as_array().unwrap();
    assert!(
        results.iter().any(|r| r["experience"]["id"] == exp_id),
        "经验检索必须能召回刚提炼的经验：{results:?}"
    );
    assert_eq!(results[0]["env_mismatch"], json!(false));

    // 7. 环境不匹配：显式告警且降权保留
    let mismatch_res = d
        .dispatch(
            &auth,
            "working_experience_search",
            arg(json!({
                "namespace": "work-ns",
                "query": "内存泄漏排查",
                "env": "os:windows",
            })),
        )
        .await;
    let mismatched = mismatch_res.structured_content.unwrap();
    let item = mismatched["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["experience"]["id"] == exp_id)
        .expect("环境不匹配时仍应保留经验");
    assert_eq!(item["env_mismatch"], json!(true));
    assert!(item["boundary_warning"]
        .as_str()
        .unwrap_or_default()
        .contains("os:windows"));
}

#[tokio::test]
async fn test_coding_memory_tools_and_security_checks() {
    let (d, auth) = harness();

    // 1. 创建 coding thread
    let thread_res = d
        .dispatch(
            &auth,
            "coding_thread_create",
            arg(json!({
                "namespace": "code-ns",
                "repository": "company/backend",
                "branch": "feature/cognitive-v2",
                "title": "重构 MCP 认知调度器",
            })),
        )
        .await;
    assert!(!thread_res.is_error, "线程创建失败: {:?}", thread_res);
    let thread_id = thread_res.structured_content.unwrap()["thread"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // 2. 尝试记录含路径穿越与敏感凭证的代码变更 -> 必须被拒绝
    let bad_path = d
        .dispatch(
            &auth,
            "coding_revision_record",
            arg(json!({
                "namespace": "code-ns",
                "repository": "company/backend",
                "thread_id": thread_id,
                "file_path": "../../../etc/passwd",
                "diff_content": "+root:x:0:0:root:/root:/bin/bash",
                "change_kind": "modify",
            })),
        )
        .await;
    assert!(bad_path.is_error, "路径穿越必须被拒绝");

    let leak = d
        .dispatch(
            &auth,
            "coding_revision_record",
            arg(json!({
                "namespace": "code-ns",
                "repository": "company/backend",
                "thread_id": thread_id,
                "file_path": "config/key.rs",
                "diff_content": format!(
                    "+const KEY: &str = \"{}\";",
                    concat!("sk-proj-", "1234567890abcdef1234567890abcdef")
                ),
                "change_kind": "add",
            })),
        )
        .await;
    assert!(leak.is_error, "敏感凭证泄漏必须被拒绝");

    // 3. 正常记录代码变更
    let ok_rev = d
            .dispatch(
                &auth,
                "coding_revision_record",
                arg(json!({
                    "namespace": "code-ns",
                    "repository": "company/backend",
                    "thread_id": thread_id,
                    "file_path": "src/tools.rs",
                    "diff_content": "--- a/src/tools.rs\n+++ b/src/tools.rs\n@@ -1,1 +1,2 @@\n+// cognitive tool\n",
                    "change_kind": "modify",
                })),
            )
            .await;
    assert!(!ok_rev.is_error, "正常代码修订应成功: {:?}", ok_rev);
}

#[tokio::test]
async fn test_chat_cognitive_tools_propose_resolve_and_intentions() {
    let (mut d, auth) = harness();
    let mock_extractor = Arc::new(
        mem_core::testing::MockChatCognitiveExtractor::new().with_candidates(vec![
            mem_core::CognitiveCandidate {
                content: "用户偏好使用 Rust 编写后端服务".into(),
                layer: mem_core::CognitiveLayer::Intention,
                scene: mem_core::MemoryScene::Chat,
                confidence: 0.95,
                occurred_at: Some(chrono::Utc::now()),
                valid_from: None,
                valid_until: Some(chrono::Utc::now() + chrono::Duration::days(30)),
                tags: vec!["preference".into(), "rust".into()],
                target_memory_id: None,
                relation_kind: None,
                metadata: serde_json::json!({}),
            },
        ]),
    );
    let chat_svc = Arc::new(mem_core::ChatCognitiveService::new(
        d.store.clone(),
        mock_extractor,
        mem_core::CognitivePolicy {
            mode: mem_core::CognitiveGovernanceMode::Manual,
            ..Default::default()
        },
    ));
    d = d.with_chat_cognitive(chat_svc);

    // 1. 提交对话提炼
    let prop_res = d
        .dispatch(
            &auth,
            "cognitive_chat_propose",
            arg(json!({
                "namespace": "chat-ns",
                "turns": [
                    { "role": "user", "content": "以后项目后端全部使用 Rust" },
                    { "role": "assistant", "content": "好的，已记录偏好" }
                ],
            })),
        )
        .await;
    assert!(!prop_res.is_error, "对话提炼应成功: {:?}", prop_res);
    let proposals = prop_res.structured_content.unwrap()["proposals"]
        .as_array()
        .unwrap()
        .clone();
    assert!(!proposals.is_empty(), "在 Manual 模式下应生成候选提案");

    // 2. 列出意图与提案
    let list_res = d
        .dispatch(
            &auth,
            "cognitive_intentions_list",
            arg(json!({
                "namespace": "chat-ns",
            })),
        )
        .await;
    assert!(!list_res.is_error, "列出意图应成功: {:?}", list_res);
    let sc = list_res.structured_content.unwrap();
    // 提案已持久化：list 必须能看到 propose 产出的待审提案
    let listed = sc["proposals"].as_array().unwrap().clone();
    assert_eq!(listed.len(), 1, "列出意图应包含待审认知提案");
    assert_eq!(listed[0]["status"], "pending_review");
    assert_eq!(listed[0]["id"], proposals[0]["id"]);

    // 3. 带状态过滤
    let filtered = d
        .dispatch(
            &auth,
            "cognitive_intentions_list",
            arg(json!({
                "namespace": "chat-ns",
                "status": "pending_review",
            })),
        )
        .await;
    assert!(!filtered.is_error);
    assert_eq!(
        filtered.structured_content.unwrap()["proposals"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let filtered_none = d
        .dispatch(
            &auth,
            "cognitive_intentions_list",
            arg(json!({
                "namespace": "chat-ns",
                "status": "accepted",
            })),
        )
        .await;
    assert!(filtered_none.structured_content.unwrap()["proposals"]
        .as_array()
        .unwrap()
        .is_empty());
}

/// 端到端：propose → 裁决（采纳/驳回/编辑后合并）→ 提案状态推进
#[tokio::test]
async fn test_cognitive_proposal_resolve_accept_reject_modified() {
    let (mut d, auth) = harness();
    let mk_candidate = |content: &str| mem_core::CognitiveCandidate {
        content: content.into(),
        layer: mem_core::CognitiveLayer::Fact,
        scene: mem_core::MemoryScene::Chat,
        confidence: 0.9,
        occurred_at: Some(chrono::Utc::now()),
        valid_from: None,
        valid_until: None,
        tags: vec![],
        target_memory_id: None,
        relation_kind: None,
        metadata: serde_json::json!({}),
    };
    let chat_svc = Arc::new(mem_core::ChatCognitiveService::new(
        d.store.clone(),
        Arc::new(
            mem_core::testing::MockChatCognitiveExtractor::new().with_candidates(vec![
                mk_candidate("候选一：用户使用 Neovim"),
                mk_candidate("候选二：用户住在上海"),
                mk_candidate("候选三：团队用线性迭代"),
            ]),
        ),
        mem_core::CognitivePolicy {
            mode: mem_core::CognitiveGovernanceMode::Manual,
            ..Default::default()
        },
    ));
    d = d.with_chat_cognitive(chat_svc);

    let prop_res = d
        .dispatch(
            &auth,
            "cognitive_chat_propose",
            arg(json!({
                "namespace": "resolve-ns",
                "turns": [{ "role": "user", "content": "介绍偏好" }],
            })),
        )
        .await;
    assert!(!prop_res.is_error, "{:?}", prop_res);
    let proposals = prop_res.structured_content.unwrap()["proposals"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(proposals.len(), 3);
    let id_of = |i: usize| proposals[i]["id"].as_str().unwrap().to_string();

    // 驳回
    let rej = d
        .dispatch(
            &auth,
            "cognitive_proposal_resolve",
            arg(json!({
                "proposal_id": id_of(0),
                "decision": "rejected",
                "reason": "不准确",
            })),
        )
        .await;
    assert!(!rej.is_error, "驳回应成功: {:?}", rej);
    let sc = rej.structured_content.unwrap();
    assert_eq!(sc["status"], "rejected");
    assert!(sc["resulting_memory"].is_null());

    // 采纳
    let acc = d
        .dispatch(
            &auth,
            "cognitive_proposal_resolve",
            arg(json!({
                "proposal_id": id_of(1),
                "decision": "accepted",
            })),
        )
        .await;
    assert!(!acc.is_error, "采纳应成功: {:?}", acc);
    let sc = acc.structured_content.unwrap();
    assert_eq!(sc["status"], "accepted");
    assert_eq!(sc["resulting_memory"]["content"], "候选二：用户住在上海");

    // 编辑后合并
    let modi = d
        .dispatch(
            &auth,
            "cognitive_proposal_resolve",
            arg(json!({
                "proposal_id": id_of(2),
                "decision": "modified",
                "custom_content": "团队用线性迭代（两周一个周期）",
            })),
        )
        .await;
    assert!(!modi.is_error, "编辑后合并应成功: {:?}", modi);
    let sc = modi.structured_content.unwrap();
    assert_eq!(sc["status"], "modified");
    assert_eq!(
        sc["resulting_memory"]["content"],
        "团队用线性迭代（两周一个周期）"
    );

    // 已裁决提案再裁决 → Conflict
    let again = d
        .dispatch(
            &auth,
            "cognitive_proposal_resolve",
            arg(json!({
                "proposal_id": id_of(1),
                "decision": "accepted",
            })),
        )
        .await;
    assert!(again.is_error, "重复裁决必须报错: {:?}", again);
    assert_eq!(again.structured_content.unwrap()["code"], "E_CONFLICT");

    // modified 缺 custom_content → InvalidArgument
    let missing = d
        .dispatch(
            &auth,
            "cognitive_proposal_resolve",
            arg(json!({
                "proposal_id": id_of(2),
                "decision": "modified",
            })),
        )
        .await;
    assert!(missing.is_error);
}

/// modified 的 custom_content 未过候选校验（敏感凭证闸门）时：
/// 记忆不入库、提案停留 pending_review，修正后同一提案仍可合法裁决。
/// 这是 #18 原子性缺口的端到端守卫——校验失败绝不能让提案被
/// 错误推进成 accepted（那会造成"已入库但无法追溯"或重试 Conflict）。
#[tokio::test]
async fn test_cognitive_resolve_invalid_custom_content_keeps_proposal_pending() {
    let (mut d, auth) = harness();
    let mk_candidate = |content: &str| mem_core::CognitiveCandidate {
        content: content.into(),
        layer: mem_core::CognitiveLayer::Fact,
        scene: mem_core::MemoryScene::Chat,
        confidence: 0.9,
        occurred_at: Some(chrono::Utc::now()),
        valid_from: None,
        valid_until: None,
        tags: vec![],
        target_memory_id: None,
        relation_kind: None,
        metadata: serde_json::json!({}),
    };
    let chat_svc = Arc::new(mem_core::ChatCognitiveService::new(
        d.store.clone(),
        Arc::new(
            mem_core::testing::MockChatCognitiveExtractor::new()
                .with_candidates(vec![mk_candidate("候选：用户使用 Zed 编辑器")]),
        ),
        mem_core::CognitivePolicy {
            mode: mem_core::CognitiveGovernanceMode::Manual,
            ..Default::default()
        },
    ));
    d = d.with_chat_cognitive(chat_svc);

    let prop = d
        .dispatch(
            &auth,
            "cognitive_chat_propose",
            arg(json!({
                "namespace": "invalid-mod-ns",
                "turns": [{ "role": "user", "content": "介绍编辑器" }],
            })),
        )
        .await;
    assert!(!prop.is_error, "{:?}", prop);
    let proposal_id = prop.structured_content.unwrap()["proposals"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // 修订正文携带真实形态的凭证：必须被候选校验拒绝
    // （concat 拆分避免仓库级 secret scanning 误判为真实泄露）
    let leaky = format!("密钥 {}", concat!("sk-proj-", "uxg4cgpiZt9k2mQw"));
    let bad = d
        .dispatch(
            &auth,
            "cognitive_proposal_resolve",
            arg(json!({
                "proposal_id": proposal_id,
                "decision": "modified",
                "custom_content": leaky,
            })),
        )
        .await;
    assert!(bad.is_error, "敏感凭证修订必须被拒绝: {:?}", bad);
    assert_eq!(
        bad.structured_content.unwrap()["code"],
        "E_INVALID_ARGUMENT"
    );

    // 提案必须停留待审（未被错误推进），且记忆未入库
    let list = d
        .dispatch(
            &auth,
            "cognitive_intentions_list",
            arg(json!({ "namespace": "invalid-mod-ns", "status": "pending_review" })),
        )
        .await;
    let pending = list.structured_content.unwrap()["proposals"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(pending.len(), 1, "校验失败后提案必须停留 pending_review");
    assert_eq!(pending[0]["id"].as_str().unwrap(), proposal_id);

    // 修正后的同一提案仍可正常走 modified 入库——不因上次失败而 Conflict
    let fixed = d
        .dispatch(
            &auth,
            "cognitive_proposal_resolve",
            arg(json!({
                "proposal_id": proposal_id,
                "decision": "modified",
                "custom_content": "用户使用 Zed 编辑器（已修订）",
            })),
        )
        .await;
    assert!(!fixed.is_error, "修正后裁决应成功: {:?}", fixed);
    let sc = fixed.structured_content.unwrap();
    assert_eq!(sc["status"], "modified");
    assert_eq!(
        sc["resulting_memory"]["content"],
        "用户使用 Zed 编辑器（已修订）"
    );
}

/// ID 混用：整合提案 ID 传给 cognitive 裁决必须得到明确错误而非 NotFound
#[tokio::test]
async fn test_cognitive_resolve_rejects_integration_proposal_id_with_clear_error() {
    let (d, auth) = harness();

    // 制造一条真实存在的整合提案：写入两条同内容条目后扫描
    for _ in 0..2 {
        d.dispatch(
            &auth,
            "memory_put",
            arg(json!({ "namespace": "mix-ns", "kind": "fact", "content": "完全相同的重复内容" })),
        )
        .await;
    }
    let scan = d
        .dispatch(
            &auth,
            "memory_integrate_scan",
            arg(json!({ "namespace": "mix-ns", "dry_run": false })),
        )
        .await;
    assert!(!scan.is_error, "扫描应成功: {:?}", scan);
    let integration_id = scan.structured_content.unwrap()["proposals"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // 拿整合提案 ID 调认知裁决：必须得到明确错误
    let res = d
        .dispatch(
            &auth,
            "cognitive_proposal_resolve",
            arg(json!({
                "proposal_id": integration_id,
                "decision": "accepted",
            })),
        )
        .await;
    assert!(res.is_error, "整合 ID 调认知裁决必须报错: {:?}", res);
    let sc = res.structured_content.unwrap();
    assert_eq!(sc["code"], "E_INVALID_ARGUMENT");
    let msg = sc["message_zh"].as_str().unwrap();
    assert!(
        msg.contains("整合提案") && msg.contains("memory_integration_resolve"),
        "错误信息须指明提案类型与正确入口: {msg}"
    );

    // 纯随机 ID（两套空间都不存在）仍是 NotFound
    let ghost = d
        .dispatch(
            &auth,
            "cognitive_proposal_resolve",
            arg(json!({
                "proposal_id": uuid::Uuid::new_v4().to_string(),
                "decision": "accepted",
            })),
        )
        .await;
    assert!(ghost.is_error);
    assert_eq!(ghost.structured_content.unwrap()["code"], "E_NOT_FOUND");
}

// ===== 整合模式：MCP 兼容性与覆盖闸门 =====

/// 旧客户端（不传 `mode`）必须照常工作
#[tokio::test]
async fn test_consolidate_without_mode_field_keeps_working() {
    let (d, auth) = harness();
    d.dispatch(
        &auth,
        "memory_put",
        arg(json!({ "namespace": "ns", "kind": "fact", "content": "旧客户端也能整合" })),
    )
    .await;
    d.dispatch(
        &auth,
        "memory_put",
        arg(json!({ "namespace": "ns", "kind": "fact", "content": "旧客户端也能整合" })),
    )
    .await;

    let res = d
        .dispatch(
            &auth,
            "memory_consolidate",
            arg(json!({ "namespace": "ns", "dry_run": true })),
        )
        .await;
    assert!(!res.is_error, "不带 mode 的旧请求必须可用: {:?}", res);
    let sc = res.structured_content.unwrap();
    // 响应新增模式状态段，既有字段保持不变
    assert_eq!(sc["mode"]["effective_mode"], "rules");
    assert_eq!(sc["mode"]["requested_mode"], "rules");
    assert_eq!(sc["mode"]["embedding_used"], false);
    assert_eq!(sc["scanned"], 2);
}

/// 默认禁止逐请求覆盖模式：显式请求 hybrid 必须被拒绝
#[tokio::test]
async fn test_consolidate_rejects_mode_override_when_disabled() {
    let (d, auth) = harness();
    // 先建命名空间（首次写入即所有者）
    d.dispatch(
        &auth,
        "memory_put",
        arg(json!({ "namespace": "ns", "kind": "fact", "content": "用于建命名空间" })),
    )
    .await;
    let res = d
        .dispatch(
            &auth,
            "memory_consolidate",
            arg(json!({ "namespace": "ns", "dry_run": true, "mode": "hybrid" })),
        )
        .await;
    assert!(
        res.is_error,
        "未开启覆盖时必须拒绝逐请求切换模式: {:?}",
        res
    );
    let text = res
        .structured_content
        .as_ref()
        .and_then(|v| v["message_zh"].as_str())
        .unwrap_or_default()
        .to_string();
    assert!(
        text.contains("HUB_CONSOLIDATION_ALLOW_MODE_OVERRIDE"),
        "错误信息须说明如何开启：{text}"
    );
}

/// 显式请求与服务端默认一致时允许（等价于不覆盖）
#[tokio::test]
async fn test_consolidate_accepts_mode_equal_to_server_default() {
    let (d, auth) = harness();
    d.dispatch(
        &auth,
        "memory_put",
        arg(json!({ "namespace": "ns", "kind": "fact", "content": "用于建命名空间" })),
    )
    .await;
    let res = d
        .dispatch(
            &auth,
            "memory_consolidate",
            arg(json!({ "namespace": "ns", "dry_run": true, "mode": "rules" })),
        )
        .await;
    assert!(
        !res.is_error,
        "与服务端默认一致的显式模式应被接受: {:?}",
        res
    );
}

/// 开启覆盖后，显式模式被采纳
#[tokio::test]
async fn test_consolidate_accepts_mode_override_when_enabled() {
    let (d, auth) = harness();
    let d = d.with_consolidation(ConsolidationParams {
        allow_mode_override: true,
        ..ConsolidationParams::default()
    });
    d.dispatch(
        &auth,
        "memory_put",
        arg(json!({ "namespace": "ns", "kind": "fact", "content": "用于建命名空间" })),
    )
    .await;
    let res = d
        .dispatch(
            &auth,
            "memory_consolidate",
            arg(json!({ "namespace": "ns", "dry_run": true, "mode": "hybrid" })),
        )
        .await;
    assert!(!res.is_error, "开启覆盖后应接受显式模式: {:?}", res);
    let sc = res.structured_content.unwrap();
    assert_eq!(sc["mode"]["requested_mode"], "hybrid");
}

/// 未知模式字符串必须被拒绝，不得静默猜测
#[tokio::test]
async fn test_consolidate_rejects_unknown_mode_value() {
    let (d, auth) = harness();
    d.dispatch(
        &auth,
        "memory_put",
        arg(json!({ "namespace": "ns", "kind": "fact", "content": "用于建命名空间" })),
    )
    .await;
    let res = d
        .dispatch(
            &auth,
            "memory_consolidate",
            arg(json!({ "namespace": "ns", "dry_run": true, "mode": "semantic" })),
        )
        .await;
    assert!(res.is_error, "未知模式必须被拒绝: {:?}", res);
}

// 编译期检查：内存实现满足聚合契约
#[test]
fn test_in_memory_store_satisfies_contract() {
    fn assert_impl<T: mem_core::MemoryHubStore>() {}
    assert_impl::<InMemoryHubStore>();
}
