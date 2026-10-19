//! 认知记忆检索评测套件 (Retrieval Evaluation Suite)
//!
//! 评测集与覆盖矩阵：
//! 1. 精确事实、中文关键词、英文关键词、中英混合检索精度与排位
//! 2. 事实版本演化与时间有效性（valid_at 约束与 superseded 失效事实回溯）
//! 3. 认知场景感知检索（Chat / Working / Coding 场景过滤与场景自适应加成）
//! 4. 多租户跨账户严格隔离验证（同名 namespace 零泄漏保证，Cross-Account Leakage = 0）
//! 5. 故障降级与软超时自愈（嵌入服务崩溃降级 FTS、数据库超时降级 Fallback 列表）
//! 6. 量化指标核算（Recall@1, Recall@3, Recall@5, MRR, 降级成功率）
//!
//! 局限说明（不得据本套件结论推断检索质量）：
//! - 规模极小（个位数查询、每命名空间个位数条目），Recall@5 在 top_k=5 下结构性为 100%；
//! - 每个场景的数据放在独立命名空间，跨场景干扰与场景过滤/加成的价值未被测量；
//! - 使用 `TestEmbedder` 伪向量（与语义无关），向量路只起"把候选全量带回"的作用；
//! - 查询与目标文本高度字面重叠，"语义改写"类目在样本中并不存在；
//! - 内存存储的子串匹配与 PostgreSQL 分词 AND 语义不同，FTS 行为不代表生产；
//! - 故障降级成功率在断言之后计数，任何失败会 panic，因此 100% 是结构性的而非测得的。
//!
//! 时间基准：全部用例使用固定时钟（`eval_fixed_now`），不随墙上时钟漂移，
//! 避免"现行有效事实"在 valid_until 到期后变成无结果的时间炸弹。

use chrono::{TimeZone, Utc};
use common::{AccountId, AuthCtx, Scope};
use mem_core::{
    domain::{CognitiveLayer, MemoryEntry, MemoryKind, MemoryScene, MemoryStatus},
    testing::{FixedClock, TestEmbedder},
    AccountScopedStore, Embedder, MemoryHubStore, MemoryStore, Retriever, SearchQuery,
};
use retrieval::HybridRetriever;
use serde_json::json;
use std::sync::Arc;
use store::InMemoryHubStore;
use uuid::Uuid;

/// 评测固定时钟（所有时间断言以此为基准，保证任何日期运行结果一致）
fn eval_fixed_now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 19, 0, 0, 0).unwrap()
}

/// 评测样本定义
struct EvalSample {
    query: String,
    target_content_substr: String,
    scene: Option<MemoryScene>,
    layer: Option<CognitiveLayer>,
    valid_at: Option<chrono::DateTime<Utc>>,
    include_superseded: bool,
}

/// 评测结果统计
#[derive(Debug, Default, Clone)]
struct EvalMetrics {
    total_queries: usize,
    hits_at_1: usize,
    hits_at_3: usize,
    hits_at_5: usize,
    reciprocal_ranks_sum: f64,
    leakage_count: usize,
    degradation_success_count: usize,
    degradation_total_count: usize,
}

impl EvalMetrics {
    fn recall_at_1(&self) -> f64 {
        if self.total_queries == 0 {
            0.0
        } else {
            self.hits_at_1 as f64 / self.total_queries as f64
        }
    }

    fn recall_at_3(&self) -> f64 {
        if self.total_queries == 0 {
            0.0
        } else {
            self.hits_at_3 as f64 / self.total_queries as f64
        }
    }

    fn recall_at_5(&self) -> f64 {
        if self.total_queries == 0 {
            0.0
        } else {
            self.hits_at_5 as f64 / self.total_queries as f64
        }
    }

    fn mrr(&self) -> f64 {
        if self.total_queries == 0 {
            0.0
        } else {
            self.reciprocal_ranks_sum / self.total_queries as f64
        }
    }
}

fn make_auth(account: AccountId, agent: &str) -> AuthCtx {
    AuthCtx {
        account_id: account,
        agent_id: agent.to_string(),
        scopes: vec![Scope::Read, Scope::Write],
        acl: Default::default(),
    }
}

fn create_entry(
    ns: &str,
    content: &str,
    scene: Option<MemoryScene>,
    layer: Option<CognitiveLayer>,
    valid_from: Option<chrono::DateTime<Utc>>,
    valid_until: Option<chrono::DateTime<Utc>>,
) -> MemoryEntry {
    let now = eval_fixed_now();
    MemoryEntry {
        id: Uuid::new_v4(),
        namespace: ns.to_string(),
        key: None,
        kind: MemoryKind::Fact,
        content: content.to_string(),
        tags: vec![],
        metadata: json!({}),
        embedding: None,
        embedding_model: None,
        source_agent: "eval-agent".to_string(),
        confidence: 1.0,
        status: MemoryStatus::Active,
        version: 1,
        expires_at: None,
        merged_into: None,
        created_at: now,
        updated_at: now,
        scene,
        layer,
        valid_from,
        valid_until,
        occurred_at: None,
    }
}

/// 模拟崩溃的嵌入器（模拟下游嵌入服务 500 / 网络故障）
#[derive(Debug, Clone, Default)]
struct FailingEmbedder;

#[async_trait::async_trait]
impl Embedder for FailingEmbedder {
    fn model_id(&self) -> &str {
        "failing-embedder"
    }

    fn dim(&self) -> usize {
        64
    }

    async fn embed(&self, _texts: &[String]) -> common::Result<Vec<Vec<f32>>> {
        Err(common::HubError::Internal(anyhow::anyhow!(
            "模拟嵌入服务端下游 503 异常"
        )))
    }
}

#[tokio::test]
async fn test_full_retrieval_eval_suite() {
    let hub_store = Arc::new(InMemoryHubStore::default());
    let embedder = Arc::new(TestEmbedder::new(64));

    let account_prod: AccountId = Uuid::new_v4();
    let account_tenant_b: AccountId = Uuid::new_v4();

    let auth_prod = make_auth(account_prod, "agent-prod");
    let auth_tenant_b = make_auth(account_tenant_b, "agent-b");

    // 为 Prod 租户构造 ScopedStore
    let store_prod = Arc::new(AccountScopedStore::new(
        hub_store.clone() as Arc<dyn MemoryHubStore>,
        account_prod,
    ));
    // 为 Tenant B 构造 ScopedStore
    let store_b = Arc::new(AccountScopedStore::new(
        hub_store.clone() as Arc<dyn MemoryHubStore>,
        account_tenant_b,
    ));

    // ==========================================
    // 1. 灌入基准数据集
    // ==========================================
    let t2026_01 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let t2026_06 = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
    let t2026_12 = Utc.with_ymd_and_hms(2026, 12, 31, 23, 59, 59).unwrap();

    let mut corpus = vec![
        // 精确事实与多语言
        create_entry(
            "infra-facts",
            "生产 PostgreSQL 数据库最大连接数设置为 200",
            Some(MemoryScene::Chat),
            Some(CognitiveLayer::Fact),
            None,
            None,
        ),
        create_entry(
            "infra-facts",
            "API 网关超时时间设定为 30 秒 (gateway timeout 30s)",
            Some(MemoryScene::Chat),
            Some(CognitiveLayer::Fact),
            None,
            None,
        ),
        create_entry(
            "infra-facts",
            "Auth 认证服务的 JWT 密钥轮转周期为 7 天 (JWT secret rotation cycle 7 days)",
            Some(MemoryScene::Chat),
            Some(CognitiveLayer::Fact),
            None,
            None,
        ),
        // 时间有效性与事实演化
        create_entry(
            "infra-facts",
            "Redis 缓存集群主节点为 redis-master-1.prod (历史版本)",
            Some(MemoryScene::Chat),
            Some(CognitiveLayer::Fact),
            Some(t2026_01),
            Some(t2026_06), // 已在 2026-06 失效
        ),
        create_entry(
            "infra-facts",
            "Redis 缓存集群主节点迁移至 redis-master-2.prod (现行有效)",
            Some(MemoryScene::Chat),
            Some(CognitiveLayer::Fact),
            Some(t2026_06),
            Some(t2026_12),
        ),
        // Working 经验记忆
        create_entry(
            "ops-tasks",
            "在 Linux ARM64 环境下交叉编译 jemalloc 需注入环境变量",
            Some(MemoryScene::Working),
            Some(CognitiveLayer::Knowledge),
            None,
            None,
        ),
        create_entry(
            "ops-tasks",
            "排查生产环境 Redis 连接池泄漏需通过 netstat 结合 redis-cli client list 统计空闲连接",
            Some(MemoryScene::Working),
            Some(CognitiveLayer::Knowledge),
            None,
            None,
        ),
        create_entry(
            "ops-tasks",
            "PostgreSQL 慢查询日志分析使用 pg_stat_statements 扩展定位高耗时 SQL",
            Some(MemoryScene::Working),
            Some(CognitiveLayer::Knowledge),
            None,
            None,
        ),
        // Coding 代码修订记忆
        create_entry(
            "repo-code",
            "重构 HybridRetriever 双路融合并引入 MMR 多样性重排",
            Some(MemoryScene::Coding),
            Some(CognitiveLayer::Fact),
            None,
            None,
        ),
        create_entry(
            "repo-code",
            "实现 Composite Foreign Key 迁移以强化跨租户命名空间外键约束",
            Some(MemoryScene::Coding),
            Some(CognitiveLayer::Fact),
            None,
            None,
        ),
        create_entry(
            "repo-code",
            "扩展 MCP ToolDispatcher 支持 9 个专用认知增强工具分发",
            Some(MemoryScene::Coding),
            Some(CognitiveLayer::Fact),
            None,
            None,
        ),
    ];

    // 为每条条目预计算嵌入向量并存入 store_prod
    for entry in corpus.drain(..) {
        let mut e = entry;
        let v = embedder
            .embed(&[e.content.clone()])
            .await
            .unwrap()
            .remove(0);
        e.embedding = Some(v);
        store_prod.insert(&e).await.unwrap();
    }

    // 在 Tenant B 的同名 namespace 中注入隔离测试数据
    let mut b_entry = create_entry(
        "infra-facts",
        "租户 B 的私有数据库连接密码为 SecretPassword123",
        Some(MemoryScene::Chat),
        Some(CognitiveLayer::Fact),
        None,
        None,
    );
    let v_b = embedder
        .embed(&[b_entry.content.clone()])
        .await
        .unwrap()
        .remove(0);
    b_entry.embedding = Some(v_b);
    store_b.insert(&b_entry).await.unwrap();

    // 构建 Prod 的检索器
    let retriever_prod = HybridRetriever::new(
        store_prod.clone() as Arc<dyn MemoryStore>,
        embedder.clone(),
        20,
        20,
        [0.5, 0.2, 0.1, 0.1, 0.1],
        0.7,
        1000,
    )
    .with_clock(Arc::new(FixedClock::new(eval_fixed_now())));

    // ==========================================
    // 2. 执行精度与排位评测样本集
    // ==========================================
    let eval_samples = vec![
        EvalSample {
            query: "PostgreSQL 数据库最大连接数".into(),
            target_content_substr: "最大连接数设置为 200".into(),
            scene: Some(MemoryScene::Chat),
            layer: None,
            valid_at: None,
            include_superseded: false,
        },
        EvalSample {
            query: "gateway timeout 30s".into(),
            target_content_substr: "网关超时时间设定为 30 秒".into(),
            scene: Some(MemoryScene::Chat),
            layer: None,
            valid_at: None,
            include_superseded: false,
        },
        EvalSample {
            query: "JWT secret rotation cycle".into(),
            target_content_substr: "JWT 密钥轮转周期为 7 天".into(),
            scene: Some(MemoryScene::Chat),
            layer: None,
            valid_at: None,
            include_superseded: false,
        },
        EvalSample {
            query: "Redis 缓存集群主节点".into(),
            target_content_substr: "redis-master-2.prod".into(),
            scene: Some(MemoryScene::Chat),
            layer: None,
            valid_at: None, // 默认当前时间：必须命中现行有效事实 v2，过滤已失效 v1
            include_superseded: false,
        },
        EvalSample {
            query: "Redis 缓存集群主节点".into(),
            target_content_substr: "redis-master-1.prod".into(),
            scene: Some(MemoryScene::Chat),
            layer: None,
            valid_at: Some(Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap()), // 历史查询
            include_superseded: false,
        },
        EvalSample {
            query: "Linux ARM64 jemalloc".into(),
            target_content_substr: "交叉编译 jemalloc".into(),
            scene: Some(MemoryScene::Working),
            layer: Some(CognitiveLayer::Knowledge),
            valid_at: None,
            include_superseded: false,
        },
        EvalSample {
            query: "HybridRetriever MMR 重排".into(),
            target_content_substr: "MMR 多样性重排".into(),
            scene: Some(MemoryScene::Coding),
            layer: None,
            valid_at: None,
            include_superseded: false,
        },
        EvalSample {
            query: "Redis 连接池泄漏排查 netstat".into(),
            target_content_substr: "通过 netstat 结合 redis-cli".into(),
            scene: Some(MemoryScene::Working),
            layer: Some(CognitiveLayer::Knowledge),
            valid_at: None,
            include_superseded: false,
        },
        EvalSample {
            query: "PostgreSQL 慢查询定位 pg_stat_statements".into(),
            target_content_substr: "pg_stat_statements 扩展定位高耗时".into(),
            scene: Some(MemoryScene::Working),
            layer: Some(CognitiveLayer::Knowledge),
            valid_at: None,
            include_superseded: false,
        },
        EvalSample {
            query: "Composite Foreign Key 跨租户命名空间".into(),
            target_content_substr: "强化跨租户命名空间外键约束".into(),
            scene: Some(MemoryScene::Coding),
            layer: None,
            valid_at: None,
            include_superseded: false,
        },
        EvalSample {
            query: "MCP ToolDispatcher 认知增强工具".into(),
            target_content_substr: "9 个专用认知增强工具分发".into(),
            scene: Some(MemoryScene::Coding),
            layer: None,
            valid_at: None,
            include_superseded: false,
        },
    ];

    let mut metrics = EvalMetrics::default();

    for sample in &eval_samples {
        metrics.total_queries += 1;
        let q = SearchQuery {
            namespace: if sample.scene == Some(MemoryScene::Working) {
                "ops-tasks".into()
            } else if sample.scene == Some(MemoryScene::Coding) {
                "repo-code".into()
            } else {
                "infra-facts".into()
            },
            query_text: sample.query.clone(),
            kinds: None,
            tags: None,
            top_k: 5,
            include_archived: false,
            min_score: None,
            scene: sample.scene,
            layer: sample.layer,
            valid_at: sample.valid_at,
            include_superseded: sample.include_superseded,
        };

        let res = retriever_prod.search(&auth_prod, &q).await.unwrap();

        let mut hit_rank: Option<usize> = None;
        for (idx, scored_item) in res.iter().enumerate() {
            if scored_item
                .item
                .content
                .contains(&sample.target_content_substr)
            {
                hit_rank = Some(idx + 1);
                break;
            }
        }

        if let Some(rank) = hit_rank {
            if rank <= 1 {
                metrics.hits_at_1 += 1;
            }
            if rank <= 3 {
                metrics.hits_at_3 += 1;
            }
            if rank <= 5 {
                metrics.hits_at_5 += 1;
            }
            metrics.reciprocal_ranks_sum += 1.0 / (rank as f64);
        }
    }

    // ==========================================
    // 3. 多租户跨账户严格隔离验证 (Zero-Tolerance)
    // ==========================================
    let retriever_b = HybridRetriever::new(
        store_b.clone() as Arc<dyn MemoryStore>,
        embedder.clone(),
        20,
        20,
        [0.5, 0.2, 0.1, 0.1, 0.1],
        0.7,
        1000,
    )
    .with_clock(Arc::new(FixedClock::new(eval_fixed_now())));

    // 3.1 Prod 租户在 "infra-facts" 下搜索关键词 "SecretPassword" 或 "租户 B"
    let cross_check_query = SearchQuery {
        namespace: "infra-facts".into(),
        query_text: "SecretPassword 密码 租户 B".into(),
        kinds: None,
        tags: None,
        top_k: 10,
        include_archived: true,
        min_score: None,
        scene: None,
        layer: None,
        valid_at: None,
        include_superseded: true,
    };
    let prod_cross_res = retriever_prod
        .search(&auth_prod, &cross_check_query)
        .await
        .unwrap();
    for hit in prod_cross_res {
        if hit.item.content.contains("SecretPassword") || hit.item.content.contains("租户 B") {
            metrics.leakage_count += 1;
        }
    }

    // 3.2 Tenant B 在 "infra-facts" 下搜索 Prod 的敏感配置 "PostgreSQL 最大连接数"
    let cross_check_b_query = SearchQuery {
        namespace: "infra-facts".into(),
        query_text: "PostgreSQL 数据库最大连接数".into(),
        kinds: None,
        tags: None,
        top_k: 10,
        include_archived: true,
        min_score: None,
        scene: None,
        layer: None,
        valid_at: None,
        include_superseded: true,
    };
    let b_cross_res = retriever_b
        .search(&auth_tenant_b, &cross_check_b_query)
        .await
        .unwrap();
    for hit in b_cross_res {
        if hit.item.content.contains("PostgreSQL") || hit.item.content.contains("200") {
            metrics.leakage_count += 1;
        }
    }

    // 3.3 Tenant B 检索自己的数据应正常召回
    let b_self_query = SearchQuery {
        namespace: "infra-facts".into(),
        query_text: "SecretPassword".into(),
        kinds: None,
        tags: None,
        top_k: 5,
        include_archived: false,
        min_score: None,
        scene: None,
        layer: None,
        valid_at: None,
        include_superseded: false,
    };
    let b_self_res = retriever_b
        .search(&auth_tenant_b, &b_self_query)
        .await
        .unwrap();
    assert_eq!(b_self_res.len(), 1, "Tenant B 必须能够召回自身的私有数据");
    assert!(b_self_res[0].item.content.contains("SecretPassword123"));

    assert_eq!(
        metrics.leakage_count, 0,
        "跨账户数据隔离失败！绝对不允许检索到其他账户的数据"
    );

    // ==========================================
    // 4. 故障自愈与降级评测
    // ==========================================
    metrics.degradation_total_count += 2;

    // 4.1 嵌入服务崩溃降级 FTS
    let failing_retriever = HybridRetriever::new(
        store_prod.clone() as Arc<dyn MemoryStore>,
        Arc::new(FailingEmbedder),
        20,
        20,
        [0.5, 0.2, 0.1, 0.1, 0.1],
        0.7,
        1000,
    )
    .with_clock(Arc::new(FixedClock::new(eval_fixed_now())));
    let fts_degrade_query = SearchQuery {
        namespace: "infra-facts".into(),
        query_text: "PostgreSQL".into(),
        kinds: None,
        tags: None,
        top_k: 5,
        include_archived: false,
        min_score: None,
        scene: None,
        layer: None,
        valid_at: None,
        include_superseded: false,
    };
    let degrade_res = failing_retriever
        .search(&auth_prod, &fts_degrade_query)
        .await;
    assert!(
        degrade_res.is_ok(),
        "嵌入服务故障时检索不应报错，应降级成功"
    );
    let hits = degrade_res.unwrap();
    assert!(!hits.is_empty(), "FTS 关键词路应正常召回候选");
    assert!(hits[0].item.content.contains("PostgreSQL"));
    metrics.degradation_success_count += 1;

    // 4.2 软超时保护与降级回退
    let timeout_retriever = HybridRetriever::new(
        store_prod.clone() as Arc<dyn MemoryStore>,
        embedder.clone(),
        20,
        20,
        [0.5, 0.2, 0.1, 0.1, 0.1],
        0.7,
        1, // 1ms 极限超时触发降级
    )
    .with_clock(Arc::new(FixedClock::new(eval_fixed_now())));
    tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
    let timeout_query = SearchQuery {
        namespace: "infra-facts".into(),
        query_text: "数据库连接数".into(),
        kinds: None,
        tags: None,
        top_k: 5,
        include_archived: false,
        min_score: None,
        scene: None,
        layer: None,
        valid_at: None,
        include_superseded: false,
    };
    let timeout_res = timeout_retriever.search(&auth_prod, &timeout_query).await;
    assert!(
        timeout_res.is_ok(),
        "软超时触发时必须降级为列表兜底，不得无限挂起或 panic"
    );
    metrics.degradation_success_count += 1;

    // ==========================================
    // 5. 打印量化评测报告
    // ==========================================
    println!("\n========================================================");
    println!("        MEMORY HUB COGNITIVE RETRIEVAL BENCHMARK        ");
    println!("========================================================");
    println!("Total Evaluation Queries : {}", metrics.total_queries);
    println!(
        "Recall@1                 : {:.2}% ({}/{})",
        metrics.recall_at_1() * 100.0,
        metrics.hits_at_1,
        metrics.total_queries
    );
    println!(
        "Recall@3                 : {:.2}% ({}/{})",
        metrics.recall_at_3() * 100.0,
        metrics.hits_at_3,
        metrics.total_queries
    );
    println!(
        "Recall@5                 : {:.2}% ({}/{})",
        metrics.recall_at_5() * 100.0,
        metrics.hits_at_5,
        metrics.total_queries
    );
    println!("MRR (Mean Reciprocal Rnk): {:.4}", metrics.mrr());
    println!(
        "Cross-Account Leakage    : {} (Zero-Tolerance: PASSED)",
        metrics.leakage_count
    );
    println!(
        "Fault Degradation Success: {:.2}% ({}/{})",
        (metrics.degradation_success_count as f64 / metrics.degradation_total_count as f64) * 100.0,
        metrics.degradation_success_count,
        metrics.degradation_total_count
    );
    println!("========================================================\n");

    // 指标门禁断言
    assert!(metrics.recall_at_1() >= 0.85, "Recall@1 须达到或超过 85%");
    assert_eq!(metrics.recall_at_5(), 1.0, "Recall@5 须达到 100%");
    assert!(metrics.mrr() >= 0.90, "MRR 须达到或超过 0.90");
    assert_eq!(metrics.leakage_count, 0, "跨账户越权泄漏数必须为 0");
    assert_eq!(
        metrics.degradation_success_count, metrics.degradation_total_count,
        "故障降级成功率必须达到 100%"
    );
}

/// 真实 PostgreSQL + pgvector 独立基准评测（显式配置数据库后通过 `--ignored` 执行）
#[ignore]
#[tokio::test]
async fn test_postgres_retrieval_eval_suite() {
    let db_url = match std::env::var("DATABASE_URL") {
        Ok(url) if !url.trim().is_empty() => url,
        _ => {
            println!(
                "\n[SKIPPED] 未配置 DATABASE_URL 环境变量，跳过真实 PostgreSQL 集成/性能评测。\n"
            );
            return;
        }
    };
    println!("\n========================================================");
    println!("    POSTGRESQL + PGVECTOR RETRIEVAL BENCHMARK           ");
    println!("========================================================");
    let store = match store::PostgresHubStore::connect(&db_url, 5, 10).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            panic!("连接 PostgreSQL 数据库失败: {}", e);
        }
    };
    let embedder = Arc::new(TestEmbedder::new(64));
    let account_id = Uuid::new_v4();
    let auth = make_auth(account_id, "pg-eval-agent");
    let scoped_store = Arc::new(AccountScopedStore::new(
        store.clone() as Arc<dyn MemoryHubStore>,
        account_id,
    ));
    let retriever = HybridRetriever::new(
        scoped_store.clone() as Arc<dyn MemoryStore>,
        embedder.clone(),
        20,
        20,
        [0.5, 0.2, 0.1, 0.1, 0.1],
        0.7,
        1000,
    )
    .with_clock(Arc::new(FixedClock::new(eval_fixed_now())));

    // 灌入测试数据
    let mut entry = create_entry(
        "pg-eval-ns",
        "PostgreSQL 真实数据库与 pgvector 语义检索基准测试条目",
        Some(MemoryScene::Chat),
        Some(CognitiveLayer::Fact),
        None,
        None,
    );
    let v = embedder
        .embed(&[entry.content.clone()])
        .await
        .unwrap()
        .remove(0);
    entry.embedding = Some(v);
    scoped_store.insert(&entry).await.unwrap();

    let query = SearchQuery {
        namespace: "pg-eval-ns".into(),
        query_text: "pgvector 语义检索基准测试".into(),
        kinds: None,
        tags: None,
        top_k: 5,
        include_archived: false,
        min_score: None,
        scene: None,
        layer: None,
        valid_at: None,
        include_superseded: false,
    };
    let start = std::time::Instant::now();
    let hits = retriever.search(&auth, &query).await.unwrap();
    let duration = start.elapsed();

    assert!(!hits.is_empty(), "真实 PG 检索应召回命中条目");
    assert!(hits[0].item.content.contains("pgvector"));
    println!("PostgreSQL Query Latency : {:?}", duration);
    println!("Hits Count               : {}", hits.len());
    println!("Top Result Content       : {}", hits[0].item.content);
    println!("========================================================\n");
}
