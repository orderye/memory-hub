//! 创界山智能接入层全链路集成测试
//! 涵盖：
//! - Milestone 1: 客户端指纹探测与零侵入穿透
//! - Milestone 2: 智能命名空间映射与虚拟双路召回
//! - Milestone 3: 启发式 Kind 自萃取与 AIGC 合规水印
//! - Milestone 4: 角色扮演整合防冲突与备份清单联动
//! - Milestone 5: 多租户沙箱隔离验证

use axum::http::{HeaderMap, HeaderValue};
use common::{Acl, AuthCtx, Scope};
use consolidation::DefaultIntegrationService;
use mcp::adapter::cjs::{CjsAdapter, CjsAdapterConfig};
use mcp::tools::ToolDispatcher;
use mcp::transport::{handle_jsonrpc_ext, JsonRpcRequest, McpTransportState};
use mem_core::{DefaultBackupService, DefaultMemoryService, GrantChecker};
use serde_json::{json, Value};
use std::sync::Arc;
use store::InMemoryHubStore;
use uuid::Uuid;

struct StubIntrospector;

#[async_trait::async_trait]
impl mem_core::TokenIntrospector for StubIntrospector {
    async fn introspect(&self, _token: &str) -> common::Result<AuthCtx> {
        Ok(AuthCtx {
            agent_id: "test-bot".to_string(),
            account_id: common::BOOTSTRAP_ACCOUNT_ID,
            scopes: vec![Scope::Read, Scope::Write, Scope::Admin],
            acl: Acl::default(),
        })
    }
}

fn create_test_state(config: CjsAdapterConfig) -> McpTransportState {
    let store = Arc::new(InMemoryHubStore::new());
    let access = Arc::new(GrantChecker::new(store.clone()));
    let embedder = Arc::new(mem_core::TestEmbedder::default());
    let retriever = Arc::new(retrieval::HybridRetriever::new(
        store.clone() as Arc<dyn mem_core::MemoryStore>,
        embedder.clone(),
        100,
        100,
        [0.5, 0.2, 0.1, 0.1, 0.1],
        0.7,
        500,
    ));
    let memory_svc = Arc::new(
        DefaultMemoryService::new(store.clone(), embedder, access.clone())
            .with_retriever(retriever),
    );
    let backup_svc = Arc::new(DefaultBackupService::new(store.clone(), access.clone()));
    let integration_svc = Arc::new(DefaultIntegrationService::new(
        store.clone(),
        access,
        0.99,
        100,
    ));

    let dispatcher = Arc::new(ToolDispatcher::new(
        memory_svc,
        backup_svc,
        integration_svc,
        store,
    ));

    let interceptor = Arc::new(CjsAdapter::new(config));

    McpTransportState {
        dispatcher,
        introspector: Arc::new(StubIntrospector),
        allowed_origins: vec![],
        max_request_bytes: 65536,
        rate_limiter: None,
        interceptor: Some(interceptor),
    }
}

fn auth_ctx(account_id: Uuid, agent_id: &str) -> AuthCtx {
    AuthCtx {
        agent_id: agent_id.to_string(),
        account_id,
        scopes: vec![Scope::Read, Scope::Write, Scope::Admin],
        acl: Acl::default(),
    }
}

#[tokio::test]
async fn test_milestone1_non_cjs_client_bypasses_untouched() {
    let state = create_test_state(CjsAdapterConfig::default());
    let auth = auth_ctx(common::BOOTSTRAP_ACCOUNT_ID, "bot_normal");

    // 普通请求：未携带任何创界山 Header 或元数据
    let req = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(json!(1)),
        method: "tools/call".to_string(),
        params: Some(json!({
            "name": "memory_put",
            "arguments": {
                "namespace": "general_notes",
                "kind": "fact",
                "content": "普通客户端写入的常规数据"
            }
        })),
    };

    let resp = handle_jsonrpc_ext(&state, Some(&auth), None, req).await;
    assert!(resp.error.is_none());
    let res_val = resp.result.unwrap();
    let memory = res_val
        .get("structuredContent")
        .and_then(|sc| sc.get("memory"))
        .unwrap();

    // 命名空间保持原样未被改写为 w_*
    assert_eq!(
        memory.get("namespace").and_then(Value::as_str),
        Some("general_notes")
    );
    // 未被注入 AIGC 水印
    let meta = memory.get("metadata").and_then(Value::as_object);
    if let Some(m) = meta {
        assert!(!m.contains_key("AIGC_Source"));
    }
}

#[tokio::test]
async fn test_dual_search_dedups_near_duplicate_content_across_namespaces() {
    // 回归：shared 与 bot 私有记忆中的近重复内容此前会同时返回
    // （只按 id/key 去重，跨命名空间的内容重复原样保留）
    let state = create_test_state(CjsAdapterConfig::default());
    let auth = auth_ctx(common::BOOTSTRAP_ACCOUNT_ID, "bot_hero");

    let mut headers = HeaderMap::new();
    headers.insert("X-Client-Source", HeaderValue::from_static("chuangjieshan"));
    headers.insert("X-CJS-World-Id", HeaderValue::from_static("world_dedup"));
    headers.insert("X-CJS-Bot-Id", HeaderValue::from_static("hero"));

    for (id, namespace, text) in [
        (1, "shared", "生产数据库主库部署在上海可用区 A"),
        (2, "bot", "生产数据库主库部署在上海可用区 A（副本）"),
    ] {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(id)),
            method: "tools/call".to_string(),
            params: Some(json!({
                "name": "memory_put",
                "arguments": {"namespace": namespace, "text": text}
            })),
        };
        let resp = handle_jsonrpc_ext(&state, Some(&auth), Some(&headers), req).await;
        assert!(resp.error.is_none(), "写入失败: {:?}", resp.error);
    }

    let req_search = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(json!(3)),
        method: "tools/call".to_string(),
        params: Some(json!({
            "name": "memory_search",
            "arguments": {"query_text": "生产数据库主库部署在上海", "top_k": 10}
        })),
    };
    let resp = handle_jsonrpc_ext(&state, Some(&auth), Some(&headers), req_search).await;
    assert!(resp.error.is_none());
    let results = resp.result.unwrap()["structuredContent"]["results"]
        .as_array()
        .unwrap()
        .clone();

    let copies = results
        .iter()
        .filter(|r| {
            r["memory"]["content"]
                .as_str()
                .unwrap_or_default()
                .contains("生产数据库主库部署")
        })
        .count();
    assert_eq!(
        copies, 1,
        "跨命名空间的近重复内容必须只保留一条：{results:#?}"
    );
}

#[tokio::test]
async fn test_milestone2_namespace_routing_and_dual_search() {
    let state = create_test_state(CjsAdapterConfig::default());
    let auth = auth_ctx(common::BOOTSTRAP_ACCOUNT_ID, "bot_hero");

    let mut headers = HeaderMap::new();
    headers.insert("X-Client-Source", HeaderValue::from_static("chuangjieshan"));
    headers.insert("X-CJS-World-Id", HeaderValue::from_static("world_alpha"));
    headers.insert("X-CJS-Bot-Id", HeaderValue::from_static("hero"));

    // 1. 写入世界公共记忆 (namespace 显式为 "shared")
    let req_shared = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(json!(1)),
        method: "tools/call".to_string(),
        params: Some(json!({
            "name": "memory_put",
            "arguments": {
                "namespace": "shared",
                "key": "world_lore_1",
                "text": "创界山第一层是神木之界，法则严禁砍伐巨树。"
            }
        })),
    };
    let resp1 = handle_jsonrpc_ext(&state, Some(&auth), Some(&headers), req_shared).await;
    assert!(resp1.error.is_none());
    let res1_val = resp1.result.as_ref().unwrap();
    assert_eq!(
        res1_val.get("isError").and_then(Value::as_bool),
        Some(false)
    );
    let mem1 = res1_val["structuredContent"]["memory"].clone();
    assert_eq!(mem1["namespace"], "w_world_alpha_shared");

    // 2. 写入 Bot 私有记忆 (namespace 显式为 "bot")
    let req_bot = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(json!(2)),
        method: "tools/call".to_string(),
        params: Some(json!({
            "name": "memory_put",
            "arguments": {
                "namespace": "bot",
                "key": "private_secret_1",
                "text": "我偷偷收藏了神木核心碎片，不能告诉别人。"
            }
        })),
    };
    let resp2 = handle_jsonrpc_ext(&state, Some(&auth), Some(&headers), req_bot).await;
    assert!(resp2.error.is_none());
    let mem2 = resp2.result.unwrap()["structuredContent"]["memory"].clone();
    assert_eq!(mem2["namespace"], "w_world_alpha_bot_hero");

    // 3. 角色发起检索（不指定物理命名空间，触发虚拟并发双路召回）
    let req_search = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(json!(3)),
        method: "tools/call".to_string(),
        params: Some(json!({
            "name": "memory_search",
            "arguments": {
                "query_text": "神木",
                "top_k": 10
            }
        })),
    };

    let resp_search = handle_jsonrpc_ext(&state, Some(&auth), Some(&headers), req_search).await;
    assert!(resp_search.error.is_none());
    let results = resp_search.result.unwrap()["structuredContent"]["results"]
        .as_array()
        .unwrap()
        .clone();

    // 验证：双路召回同时返回了公共记忆与私有记忆！
    assert!(
        results.len() >= 2,
        "双路召回应同时召回公共与私有条目，实际召回数: {}",
        results.len()
    );

    let namespaces: Vec<String> = results
        .iter()
        .map(|r| r["memory"]["namespace"].as_str().unwrap().to_string())
        .collect();

    assert!(
        namespaces.contains(&"w_world_alpha_shared".to_string()),
        "结果应包含世界公共记忆"
    );
    assert!(
        namespaces.contains(&"w_world_alpha_bot_hero".to_string()),
        "结果应包含 Bot 私有记忆"
    );
}

#[tokio::test]
async fn test_milestone3_auto_classifier_and_aigc_watermark() {
    let state = create_test_state(CjsAdapterConfig::default());
    let auth = auth_ctx(common::BOOTSTRAP_ACCOUNT_ID, "bot_classify");

    let mut headers = HeaderMap::new();
    headers.insert("X-Client-Source", HeaderValue::from_static("chuangjieshan"));
    headers.insert("X-CJS-World-Id", HeaderValue::from_static("world_w"));

    // 写入偏好内容（未指定 kind）
    let req_pref = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(json!(1)),
        method: "tools/call".to_string(),
        params: Some(json!({
            "name": "memory_put",
            "arguments": {
                "text": "极度讨厌雨天，偏好在干燥洞穴中休息"
            }
        })),
    };

    let resp_pref = handle_jsonrpc_ext(&state, Some(&auth), Some(&headers), req_pref).await;
    assert!(resp_pref.error.is_none());
    let mem = resp_pref.result.unwrap()["structuredContent"]["memory"].clone();

    // 验证：自动推导为 preference，并追加 persona_pref 标签
    assert_eq!(mem["kind"], "preference");
    let tags = mem["tags"].as_array().unwrap();
    assert!(tags.iter().any(|t| t == "persona_pref"));

    // 验证：自动注入 AIGC 国家规范水印
    let meta = mem["metadata"].as_object().unwrap();
    assert_eq!(
        meta.get("AIGC_Source").and_then(Value::as_str),
        Some("chuangjieshan")
    );
    assert_eq!(
        meta.get("AIGC_Compliant").and_then(Value::as_bool),
        Some(true)
    );
    assert!(meta.contains_key("AIGC_Timestamp"));
}

#[tokio::test]
async fn test_milestone4_roleplay_consolidation_and_backup_manifest() {
    let state = create_test_state(CjsAdapterConfig::default());
    let auth = auth_ctx(common::BOOTSTRAP_ACCOUNT_ID, "bot_cons");

    let mut headers = HeaderMap::new();
    headers.insert("X-Client-Source", HeaderValue::from_static("chuangjieshan"));
    headers.insert("X-CJS-World-Id", HeaderValue::from_static("world_c"));
    headers.insert("X-CJS-Bot-Id", HeaderValue::from_static("mage"));

    // 1. 测试整合防冲突：在私有空间传入 auto_merge: true
    let req_cons = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(json!(1)),
        method: "tools/call".to_string(),
        params: Some(json!({
            "name": "memory_consolidate",
            "arguments": {
                "namespace": "w_world_c_bot_mage",
                "auto_merge": true,
                "dry_run": true
            }
        })),
    };
    // 执行不会失败，且 auto_merge 会被中间层降级为 false
    let resp_cons = handle_jsonrpc_ext(&state, Some(&auth), Some(&headers), req_cons).await;
    assert!(resp_cons.error.is_none());

    // 2. 测试备份清单联动：创建备份并携带端侧 manifest
    let req_backup = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(json!(2)),
        method: "tools/call".to_string(),
        params: Some(json!({
            "name": "memory_backup_create",
            "arguments": {
                "namespace": "*",
                "client_manifest": {
                    "appVersion": "1.0.4",
                    "schemaVersion": 2,
                    "cjsbak_sha256": "expected_checksum_12345"
                }
            }
        })),
    };

    let resp_backup = handle_jsonrpc_ext(&state, Some(&auth), Some(&headers), req_backup).await;
    assert!(resp_backup.error.is_none());
    let backup_id_str = resp_backup.result.unwrap()["structuredContent"]["backup"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // 3. 测试恢复时哈希强校验：提供不匹配的哈希应当被拒绝
    let req_restore_bad = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(json!(3)),
        method: "tools/call".to_string(),
        params: Some(json!({
            "name": "memory_restore",
            "arguments": {
                "backup_id": backup_id_str,
                "cjsbak_sha256": "tampered_wrong_hash"
            }
        })),
    };
    let resp_restore_bad =
        handle_jsonrpc_ext(&state, Some(&auth), Some(&headers), req_restore_bad).await;
    let err = resp_restore_bad.result.unwrap();
    assert_eq!(err["isError"], true);
    let err_msg = err["content"][0]["text"].as_str().unwrap();
    assert!(
        err_msg.contains("哈希不匹配"),
        "应拒绝不匹配的备份校验和: {}",
        err_msg
    );
}

#[tokio::test]
async fn test_milestone5_multi_tenant_sandbox_isolation() {
    let state = create_test_state(CjsAdapterConfig::default());

    let account_a = Uuid::new_v4();
    let account_b = Uuid::new_v4();

    let auth_a = auth_ctx(account_a, "bot_shared_name");
    let auth_b = auth_ctx(account_b, "bot_shared_name");

    let mut headers = HeaderMap::new();
    headers.insert("X-Client-Source", HeaderValue::from_static("chuangjieshan"));
    headers.insert("X-CJS-World-Id", HeaderValue::from_static("same_world"));
    headers.insert("X-CJS-Bot-Id", HeaderValue::from_static("same_bot"));

    // 账户 A 写入秘密
    let req_put = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(json!(1)),
        method: "tools/call".to_string(),
        params: Some(json!({
            "name": "memory_put",
            "arguments": {
                "namespace": "bot",
                "key": "account_a_secret",
                "text": "账户A的绝对隐私数据"
            }
        })),
    };
    let resp_a = handle_jsonrpc_ext(&state, Some(&auth_a), Some(&headers), req_put).await;
    assert!(resp_a.error.is_none());

    // 账户 B 使用完全相同的 world_id 和 bot_id 发起检索
    let req_search = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(json!(2)),
        method: "tools/call".to_string(),
        params: Some(json!({
            "name": "memory_search",
            "arguments": {
                "query_text": "隐私数据"
            }
        })),
    };
    let resp_b = handle_jsonrpc_ext(&state, Some(&auth_b), Some(&headers), req_search).await;
    assert!(resp_b.error.is_none());
    let results_b = resp_b.result.unwrap()["structuredContent"]["results"]
        .as_array()
        .unwrap()
        .clone();

    // 验证：账户 B 查到的结果数量为 0，绝对无法穿透沙箱读取账户 A 的数据！
    assert_eq!(
        results_b.len(),
        0,
        "账户 B 绝不能查到账户 A 的任何数据，租户沙箱必须 100% 隔离"
    );
}
