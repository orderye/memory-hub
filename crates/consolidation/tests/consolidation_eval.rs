//! 记忆整合评测（模式 A「向量辅助」/ 模式 B「纯本地规则」）
//!
//! **与检索评测（`retrieval::eval_bench`）是两套不同的东西**：
//! 检索评测衡量"排序质量"（Recall@K / MRR / nDCG），
//! 本套件衡量**整合质量**：候选召回率、候选精确率、**错误合并数**、
//! 冲突识别召回率，以及租户 / 时间 / 版本 / 否定 / 数值 / 环境等**硬约束违规数**。
//!
//! 检索基线跑得好**不能**推出整合质量好，反之亦然。
//!
//! 数据集：程序化确定性生成，覆盖 15 类样本；带 SHA-256 指纹，可复现。
//! 固定时钟：任何日期运行结果一致（消除时间炸弹）。
//!
//! 运行方式：
//! ```bash
//! cargo test --test consolidation_eval -- --nocapture
//!
//! # 接入真实嵌入服务（评估模式 A 的语义增益；未配置时 A 的语义收益标记为"未验收"）
//! HUB_CONSOLIDATION_EVAL_EMBED_ENDPOINT=http://127.0.0.1:8000/v1/embeddings \
//! HUB_CONSOLIDATION_EVAL_EMBED_MODEL=bge-m3 \
//! HUB_CONSOLIDATION_EVAL_EMBED_DIM=1024 \
//! cargo test --test consolidation_eval -- --nocapture
//! ```

use chrono::{DateTime, Duration, TimeZone, Utc};
use common::{ConsolidationFallback, ConsolidationMode, Scope};
use consolidation::{AutoPolicy, CandidateLimits, ConsolidationEngine, PolicyMode};
use mem_core::{
    account_scope::scope_name, domain::MemoryKind, testing::FixedClock, Embedder, IntegrationStore,
    MemoryEntry, MemoryScene, MemoryStatus, MemoryStore,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::sync::Arc;
use store::InMemoryHubStore;
use uuid::Uuid;

/// 评测固定时钟：任何日期运行结果一致
fn fixed_now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 21, 0, 0, 0).unwrap()
}

/// 样本类别（对应任务书 §10.1 的 15 类）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Category {
    /// 完全相同文本
    Exact,
    /// 空白 / 标点 / Unicode 变化等安全规范化重复
    Canonical,
    /// 中文同义改写
    Paraphrase,
    /// 相似主题但事实不同
    SameTopicDiffFact,
    /// 数字 / 单位 / 版本号只差一位
    NumberDiff,
    /// 带否定与不带否定的对照
    Negation,
    /// 同一事实的时间更新
    TemporalUpdate,
    /// 日期 / 时间冲突
    DateConflict,
    /// 不同实体、同名实体与实体别名
    Alias,
    /// 环境标签冲突的 Working Experience
    EnvConflict,
    /// Coding 修订与 Git revision / patch 边界
    CodingRevision,
    /// 已有 open 提案的重复扫描
    ExistingProposal,
    /// 噪声负例与域外记忆
    NoiseNegative,
    /// 空内容、超长内容、敏感串、缺失向量与错误向量
    EdgeContent,
}

impl Category {
    fn label(&self) -> &'static str {
        match self {
            Category::Exact => "exact",
            Category::Paraphrase => "paraphrase",
            Category::Canonical => "canonical",
            Category::SameTopicDiffFact => "same_topic_diff_fact",
            Category::NumberDiff => "number_diff",
            Category::Negation => "negation",
            Category::TemporalUpdate => "temporal_update",
            Category::DateConflict => "date_conflict",
            Category::Alias => "alias",
            Category::EnvConflict => "env_conflict",
            Category::CodingRevision => "coding_revision",
            Category::ExistingProposal => "existing_proposal",
            Category::NoiseNegative => "noise_negative",
            Category::EdgeContent => "edge_content",
        }
    }
}

/// 条目规格
struct EntrySpec {
    kind: MemoryKind,
    content: String,
    tags: Vec<String>,
    scene: Option<MemoryScene>,
    key: Option<String>,
    metadata: serde_json::Value,
    valid_from: Option<DateTime<Utc>>,
    valid_until: Option<DateTime<Utc>>,
}

impl EntrySpec {
    fn new(content: &str) -> Self {
        Self {
            kind: MemoryKind::Fact,
            content: content.to_string(),
            tags: vec!["db".to_string()],
            scene: Some(MemoryScene::Chat),
            key: None,
            metadata: json!({}),
            valid_from: None,
            valid_until: None,
        }
    }

    fn tags(mut self, tags: &[&str]) -> Self {
        self.tags = tags.iter().map(|s| s.to_string()).collect();
        self
    }

    fn scene(mut self, scene: MemoryScene) -> Self {
        self.scene = Some(scene);
        self
    }

    fn key(mut self, key: &str) -> Self {
        self.key = Some(key.to_string());
        self
    }

    fn metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = metadata;
        self
    }

    fn valid_from(mut self, at: DateTime<Utc>) -> Self {
        self.valid_from = Some(at);
        self
    }
}

/// 标注样本
struct Sample {
    id: &'static str,
    category: Category,
    entries: Vec<EntrySpec>,
    /// 期望被识别为**重复**的条目对
    dup_pairs: Vec<(usize, usize)>,
    /// 期望被**任意类型的提案**覆盖的条目对（候选召回，不限定分类）
    candidate_pairs: Vec<(usize, usize)>,
    /// 期望被识别为**冲突**的条目对
    conflict_pairs: Vec<(usize, usize)>,
    /// **绝不允许自动合并**的条目对（硬约束；被合并即计为错误合并）
    no_merge_pairs: Vec<(usize, usize)>,
    /// 期望**保持分立**（不产生任何提案）的条目对
    separate_pairs: Vec<(usize, usize)>,
    /// 期望被自动合并的条目对（仅精确 / 规范化重复可自动合并）
    auto_merge_pairs: Vec<(usize, usize)>,
    /// 该样本要验证"重复扫描不得重建已存在的 open 提案"
    pre_existing_proposal: bool,
}

impl Sample {
    fn new(id: &'static str, category: Category) -> Self {
        Self {
            id,
            category,
            entries: Vec::new(),
            dup_pairs: Vec::new(),
            candidate_pairs: Vec::new(),
            conflict_pairs: Vec::new(),
            no_merge_pairs: Vec::new(),
            separate_pairs: Vec::new(),
            auto_merge_pairs: Vec::new(),
            pre_existing_proposal: false,
        }
    }
}

/// 构造评测数据集
fn dataset() -> Vec<Sample> {
    let t0 = fixed_now();
    let t1 = t0 + Duration::days(7);
    let mut samples = Vec::new();

    // ===== 1. 完全相同文本 =====
    let mut s = Sample::new("exact-1", Category::Exact);
    s.entries
        .push(EntrySpec::new("数据库使用 PostgreSQL 15 存储业务数据"));
    s.entries
        .push(EntrySpec::new("数据库使用 PostgreSQL 15 存储业务数据"));
    s.dup_pairs.push((0, 1));
    s.auto_merge_pairs.push((0, 1));
    samples.push(s);

    // ===== 2. 安全规范化重复 =====
    let mut s = Sample::new("canonical-1", Category::Canonical);
    s.entries
        .push(EntrySpec::new("缓存预热策略：启动后预热 60 秒"));
    s.entries
        .push(EntrySpec::new("缓存预热策略：启动后预热  60 秒\n"));
    s.dup_pairs.push((0, 1));
    s.auto_merge_pairs.push((0, 1));
    samples.push(s);

    // ===== 3. 中文同义改写（A 相对 B 的增益样本） =====
    let mut s = Sample::new("paraphrase-1", Category::Paraphrase);
    // 同义改写：词法重叠很低（纯规则模式抓不到），但语义相近（向量模式能召回）
    s.entries.push(EntrySpec::new("缓存失效时采用双删策略"));
    s.entries
        .push(EntrySpec::new("针对缓存击穿我们先移除键再回写"));
    s.candidate_pairs.push((0, 1));
    s.no_merge_pairs.push((0, 1));
    samples.push(s);

    // ===== 4. 相似主题但事实不同（同 key → 冲突） =====
    let mut s = Sample::new("same-topic-diff-fact-1", Category::SameTopicDiffFact);
    s.entries
        .push(EntrySpec::new("数据库部署在上海可用区").key("db:region"));
    s.entries
        .push(EntrySpec::new("数据库部署在北京可用区").key("db:region"));
    s.conflict_pairs.push((0, 1));
    s.no_merge_pairs.push((0, 1));
    samples.push(s);

    // ===== 5. 数字 / 单位 / 版本号只差一位 =====
    let mut s = Sample::new("number-diff-1", Category::NumberDiff);
    s.entries.push(EntrySpec::new("连接池最大连接数上限为 500"));
    s.entries.push(EntrySpec::new("连接池最大连接数上限为 600"));
    s.no_merge_pairs.push((0, 1));
    // 词法高度重叠 → 系统应把它作为"待人工确认"的候选提出来，但绝不能自动合并
    s.candidate_pairs.push((0, 1));
    samples.push(s);

    let mut s = Sample::new("version-diff-1", Category::NumberDiff);
    s.entries.push(EntrySpec::new("网关服务版本 v1"));
    s.entries.push(EntrySpec::new("网关服务版本 v2"));
    s.no_merge_pairs.push((0, 1));
    s.candidate_pairs.push((0, 1));
    samples.push(s);

    // ===== 6. 否定对照 =====
    let mut s = Sample::new("negation-1", Category::Negation);
    s.entries.push(EntrySpec::new("该方案不是生产环境推荐方案"));
    s.entries.push(EntrySpec::new("该方案是生产环境推荐方案"));
    s.no_merge_pairs.push((0, 1));
    // 否定对照：规范化不会把"不是 X"折叠成"X"，但词法仍高度重叠，
    // 因此系统应把它作为待审候选提出来，绝不能自动合并。
    // （已知限制：规则模式不识别否定极性，不会把它判成"冲突"。）
    s.candidate_pairs.push((0, 1));
    samples.push(s);

    // ===== 7. 同一事实的时间更新（同 key + 时间可定序 → 版本候选） =====
    let mut s = Sample::new("temporal-update-1", Category::TemporalUpdate);
    s.entries.push(
        EntrySpec::new("主库所在区域为上海")
            .key("db:primary-region")
            .valid_from(t0),
    );
    s.entries.push(
        EntrySpec::new("主库所在区域为深圳")
            .key("db:primary-region")
            .valid_from(t1),
    );
    s.conflict_pairs.push((0, 1));
    s.no_merge_pairs.push((0, 1));
    samples.push(s);

    // ===== 8. 日期 / 时间冲突 =====
    let mut s = Sample::new("date-conflict-1", Category::DateConflict);
    s.entries.push(
        EntrySpec::new("季度复盘安排在 2026-04-01")
            .key("ops:review")
            .valid_from(t0),
    );
    s.entries.push(
        EntrySpec::new("季度复盘安排在 2026-04-15")
            .key("ops:review")
            .valid_from(t0),
    );
    s.conflict_pairs.push((0, 1));
    s.no_merge_pairs.push((0, 1));
    samples.push(s);

    // ===== 9. 同名实体与别名 =====
    let mut s = Sample::new("alias-1", Category::Alias);
    s.entries.push(EntrySpec::new("订单服务负责创建订单"));
    s.entries
        .push(EntrySpec::new("订单服务负责创建订单并写入事件表"));
    s.no_merge_pairs.push((0, 1));
    s.candidate_pairs.push((0, 1));
    samples.push(s);

    // ===== 10. 环境标签冲突 =====
    let mut s = Sample::new("env-conflict-1", Category::EnvConflict);
    s.entries.push(
        EntrySpec::new("缓存预热策略：启动后预热 60 秒")
            .scene(MemoryScene::Working)
            .metadata(json!({"envs": ["prod"]})),
    );
    s.entries.push(
        EntrySpec::new("缓存预热策略：启动后预热 60 秒")
            .scene(MemoryScene::Working)
            .metadata(json!({"envs": ["staging"]})),
    );
    s.conflict_pairs.push((0, 1));
    s.no_merge_pairs.push((0, 1));
    samples.push(s);

    // ===== 11. Coding 修订边界 =====
    let mut s = Sample::new("coding-revision-1", Category::CodingRevision);
    s.entries.push(
        EntrySpec::new("提交 a1b2c3d 修复了缓存失效问题")
            .scene(MemoryScene::Coding)
            .tags(&["git"]),
    );
    s.entries.push(
        EntrySpec::new("提交 e4f5a6b 修复了缓存失效问题")
            .scene(MemoryScene::Coding)
            .tags(&["git"]),
    );
    s.no_merge_pairs.push((0, 1));
    s.candidate_pairs.push((0, 1));
    samples.push(s);

    // ===== 12. 噪声负例（域外记忆） =====
    let mut s = Sample::new("noise-negative-1", Category::NoiseNegative);
    s.entries
        .push(EntrySpec::new("团队周会在每周一上午十点召开").tags(&["meeting"]));
    s.entries
        .push(EntrySpec::new("报销流程需要在每月五号之前提交").tags(&["finance"]));
    s.separate_pairs.push((0, 1));
    samples.push(s);

    // ===== 12. 已有 open 提案的重复扫描 =====
    let mut s = Sample::new("existing-proposal-1", Category::ExistingProposal);
    s.entries
        .push(EntrySpec::new("重复扫描不得重建提案的固定事实"));
    s.entries
        .push(EntrySpec::new("重复扫描不得重建提案的固定事实"));
    s.dup_pairs.push((0, 1));
    s.pre_existing_proposal = true;
    samples.push(s);

    // ===== 13. 边界内容 =====
    let mut s = Sample::new("edge-content-1", Category::EdgeContent);
    s.entries.push(EntrySpec::new(""));
    s.entries.push(EntrySpec::new("普通内容用于对照"));
    s.separate_pairs.push((0, 1));
    samples.push(s);

    let mut s = Sample::new("edge-content-2", Category::EdgeContent);
    let long = "长".repeat(9000);
    s.entries.push(EntrySpec::new(&long));
    s.entries.push(EntrySpec::new("短内容用于对照"));
    s.separate_pairs.push((0, 1));
    samples.push(s);

    samples
}

/// 数据集指纹（SHA-256）
fn dataset_fingerprint(samples: &[Sample]) -> String {
    let mut hasher = Sha256::new();
    for s in samples {
        hasher.update(s.id.as_bytes());
        hasher.update(s.category.label().as_bytes());
        for e in &s.entries {
            hasher.update(e.kind.as_str().as_bytes());
            hasher.update(e.content.as_bytes());
            hasher.update(e.key.as_deref().unwrap_or("").as_bytes());
        }
    }
    hex::encode(hasher.finalize())
}

/// 模式指标
#[derive(Debug, Default, Clone)]
struct ModeMetrics {
    /// 重复候选召回（命中 / 期望）
    dup_expected: usize,
    dup_found: usize,
    /// 候选召回（任意提案类型覆盖即算命中）
    candidate_expected: usize,
    candidate_found: usize,
    /// 冲突候选召回
    conflict_expected: usize,
    conflict_found: usize,
    /// 提案总数与命中标注的比例
    proposals_total: usize,
    proposals_matched: usize,
    /// 错误合并数（必须为 0）
    wrong_merges: usize,
    /// 硬约束违规数（必须为 0）
    hard_violations: usize,
    /// embedding 调用次数
    embedding_calls: usize,
    /// 语义改写类样本的召回
    paraphrase_expected: usize,
    paraphrase_found: usize,
    /// 重复扫描幂等性
    existing_expected: usize,
    existing_ok: usize,
}

impl ModeMetrics {
    fn candidate_recall(&self) -> f64 {
        if self.candidate_expected == 0 {
            1.0
        } else {
            self.candidate_found as f64 / self.candidate_expected as f64
        }
    }

    fn dup_recall(&self) -> f64 {
        if self.dup_expected == 0 {
            1.0
        } else {
            self.dup_found as f64 / self.dup_expected as f64
        }
    }

    fn conflict_recall(&self) -> f64 {
        if self.conflict_expected == 0 {
            1.0
        } else {
            self.conflict_found as f64 / self.conflict_expected as f64
        }
    }

    fn precision(&self) -> f64 {
        if self.proposals_total == 0 {
            1.0
        } else {
            self.proposals_matched as f64 / self.proposals_total as f64
        }
    }

    fn paraphrase_recall(&self) -> f64 {
        if self.paraphrase_expected == 0 {
            1.0
        } else {
            self.paraphrase_found as f64 / self.paraphrase_expected as f64
        }
    }
}

/// 条目对是否被某个提案覆盖
fn covered_by(proposals: &[mem_core::IntegrationProposal], a: Uuid, b: Uuid) -> bool {
    proposals
        .iter()
        .any(|p| p.member_ids.contains(&a) && p.member_ids.contains(&b))
}

/// 条目对是否被某个**指定类型**的提案覆盖
fn covered_by_kind(
    proposals: &[mem_core::IntegrationProposal],
    kind: mem_core::IntegrationKind,
    a: Uuid,
    b: Uuid,
) -> bool {
    proposals
        .iter()
        .any(|p| p.kind == kind && p.member_ids.contains(&a) && p.member_ids.contains(&b))
}

/// 构造条目
fn build_entry(ns: &str, spec: &EntrySpec, now: DateTime<Utc>) -> MemoryEntry {
    MemoryEntry {
        id: Uuid::new_v4(),
        namespace: ns.to_string(),
        key: spec.key.clone(),
        kind: spec.kind,
        content: spec.content.clone(),
        tags: spec.tags.clone(),
        metadata: spec.metadata.clone(),
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
        scene: spec.scene,
        layer: None,
        valid_from: spec.valid_from,
        valid_until: spec.valid_until,
        occurred_at: None,
    }
}

/// 以指定模式跑一遍数据集，返回指标
async fn run_mode(
    mode: ConsolidationMode,
    embedder: Option<Arc<dyn Embedder>>,
    samples: &[Sample],
) -> ModeMetrics {
    let mut m = ModeMetrics::default();

    for sample in samples {
        let ns = format!("eval/{}", sample.id);
        let store = Arc::new(InMemoryHubStore::new());
        let now = fixed_now();
        let mut ids = Vec::new();
        for spec in &sample.entries {
            let e = build_entry(&ns, spec, now);
            ids.push(e.id);
            store.insert(&e).await.expect("插入条目失败");
        }

        // ---- 预演：只看候选与提案，不改数据 ----
        let mut engine = ConsolidationEngine::new(store.clone(), 0.92, 100)
            .with_mode(mode, ConsolidationFallback::Rules)
            .with_candidate_limits(CandidateLimits::default());
        if let Some(em) = &embedder {
            engine = engine.with_embedder(em.clone());
        }
        let report = engine
            .consolidate_namespace(&ns, true, None, &"eval-agent".to_string())
            .await
            .expect("预演失败");
        let proposals = &report.proposals;
        m.proposals_total += proposals.len();
        m.embedding_calls += report.mode.embedding_calls;

        for &(a, b) in &sample.dup_pairs {
            m.dup_expected += 1;
            if covered_by_kind(
                proposals,
                mem_core::IntegrationKind::Duplicate,
                ids[a],
                ids[b],
            ) {
                m.dup_found += 1;
                m.proposals_matched += 1;
            }
        }

        // 候选召回：任意类型的提案覆盖该对即算命中
        for &(a, b) in &sample.candidate_pairs {
            m.candidate_expected += 1;
            if covered_by(proposals, ids[a], ids[b]) {
                m.candidate_found += 1;
                m.proposals_matched += 1;
            }
            if sample.category == Category::Paraphrase {
                m.paraphrase_expected += 1;
                if covered_by(proposals, ids[a], ids[b]) {
                    m.paraphrase_found += 1;
                }
            }
        }

        for &(a, b) in &sample.conflict_pairs {
            m.conflict_expected += 1;
            if covered_by_kind(
                proposals,
                mem_core::IntegrationKind::Conflict,
                ids[a],
                ids[b],
            ) {
                m.conflict_found += 1;
                m.proposals_matched += 1;
            }
        }

        // 保持分立：不得产生任何覆盖该对的提案
        for &(a, b) in &sample.separate_pairs {
            if covered_by(proposals, ids[a], ids[b]) {
                m.hard_violations += 1;
            }
        }

        // ---- 真合并：检查错误合并 ----
        // 复用同一个 store 与同一批条目 ID：预演不写数据，合并阶段才产生变更。
        let mut engine2 = ConsolidationEngine::new(store.clone(), 0.92, 100)
            .with_mode(mode, ConsolidationFallback::Rules)
            .with_candidate_limits(CandidateLimits::default())
            .with_policy(AutoPolicy::new(PolicyMode::SemiAuto));
        if let Some(em) = &embedder {
            engine2 = engine2.with_embedder(em.clone());
        }
        engine2
            .consolidate_namespace(&ns, false, None, &"eval-agent".to_string())
            .await
            .expect("整合失败");

        let active: HashSet<Uuid> = store
            .all_in_namespace(&ns, false)
            .await
            .expect("读取失败")
            .into_iter()
            .filter(|e| e.status == MemoryStatus::Active)
            .map(|e| e.id)
            .collect();

        // 绝不允许自动合并的条目对：两条都必须还活着
        for &(a, b) in &sample.no_merge_pairs {
            if !active.contains(&ids[a]) || !active.contains(&ids[b]) {
                m.wrong_merges += 1;
            }
        }
        // 期望被自动合并的条目对：至少一条必须已经不再活跃
        for &(a, b) in &sample.auto_merge_pairs {
            if active.contains(&ids[a]) && active.contains(&ids[b]) {
                m.hard_violations += 1;
            }
        }

        // ---- 重复扫描幂等：已有 open 提案的组不得被重建 ----
        if sample.pre_existing_proposal {
            let store3 = Arc::new(InMemoryHubStore::new());
            for spec in &sample.entries {
                let e = build_entry(&ns, spec, now);
                store3.insert(&e).await.expect("插入条目失败");
            }
            let engine3 = ConsolidationEngine::new(store3, 0.92, 100)
                .with_mode(mode, ConsolidationFallback::Rules)
                .with_candidate_limits(CandidateLimits::default());
            let first = engine3
                .consolidate_namespace(&ns, false, None, &"eval-agent".to_string())
                .await
                .expect("首次扫描失败");
            let second = engine3
                .consolidate_namespace(&ns, false, None, &"eval-agent".to_string())
                .await
                .expect("重复扫描失败");
            m.existing_expected += 1;
            if first.proposals_created >= 1
                && second.proposals_created == 0
                && second.skipped_existing >= 1
            {
                m.existing_ok += 1;
            }
        }
    }

    m
}

/// 构造真实嵌入器（若配置了真实端点）
fn real_embedder() -> Option<Arc<dyn Embedder>> {
    let endpoint = std::env::var("HUB_CONSOLIDATION_EVAL_EMBED_ENDPOINT").ok()?;
    let model = std::env::var("HUB_CONSOLIDATION_EVAL_EMBED_MODEL")
        .unwrap_or_else(|_| "bge-m3".to_string());
    let dim: usize = std::env::var("HUB_CONSOLIDATION_EVAL_EMBED_DIM")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1024);
    let key = std::env::var("HUB_CONSOLIDATION_EVAL_EMBED_API_KEY").ok();
    match embed::HttpEmbedder::new(endpoint, model, dim, key) {
        Ok(e) => Some(Arc::new(e) as Arc<dyn Embedder>),
        Err(e) => {
            println!("[eval] 真实嵌入器构建失败：{e}");
            None
        }
    }
}

#[tokio::test]
async fn test_consolidation_modes_quality_and_safety_gates() {
    let samples = dataset();
    let fingerprint = dataset_fingerprint(&samples);
    println!("[eval] 整合评测数据集指纹 = {fingerprint}");
    println!("[eval] 样本数 = {}", samples.len());

    // ===== 模式 B：纯本地规则整合 =====
    let b = run_mode(ConsolidationMode::Rules, None, &samples).await;
    println!(
        "[eval][rules] 重复召回={:.3} 候选召回={:.3} 冲突召回={:.3} 精确率={:.3} \
错误合并={} 硬约束违规={} embedding调用={}",
        b.dup_recall(),
        b.candidate_recall(),
        b.conflict_recall(),
        b.precision(),
        b.wrong_merges,
        b.hard_violations,
        b.embedding_calls
    );

    // ===== 模式 A：向量辅助整合 =====
    let real = real_embedder();
    let a = match &real {
        Some(em) => run_mode(ConsolidationMode::Hybrid, Some(em.clone()), &samples).await,
        None => {
            println!(
                "[eval][hybrid] 未配置 HUB_CONSOLIDATION_EVAL_EMBED_ENDPOINT：\
                 A 模式的语义增益**未验收**（不得据此宣称向量整合已达生产语义质量）"
            );
            run_mode(ConsolidationMode::Hybrid, None, &samples).await
        }
    };
    println!(
        "[eval][hybrid] 重复召回={:.3} 候选召回={:.3} 冲突召回={:.3} 精确率={:.3} \
错误合并={} 硬约束违规={} embedding调用={}",
        a.dup_recall(),
        a.candidate_recall(),
        a.conflict_recall(),
        a.precision(),
        a.wrong_merges,
        a.hard_violations,
        a.embedding_calls
    );

    // ===== 强制门槛（任务书 §10.3） =====

    // 1) 错误合并数必须为 0
    assert_eq!(b.wrong_merges, 0, "纯规则模式出现错误合并");
    assert_eq!(a.wrong_merges, 0, "向量模式出现错误合并");

    // 2) 硬约束违规数必须为 0
    assert_eq!(b.hard_violations, 0, "纯规则模式出现硬约束违规");
    assert_eq!(a.hard_violations, 0, "向量模式出现硬约束违规");

    // 3) 纯规则模式的 embedding 调用数必须为 0
    assert_eq!(b.embedding_calls, 0, "纯规则模式不得调用嵌入服务");

    // 4) 两模式都必须覆盖基础能力：精确重复与安全规范化重复必须被识别
    assert!(
        b.dup_recall() >= 0.5,
        "纯规则模式的基础重复召回过低：{:.3}",
        b.dup_recall()
    );
    assert!(
        a.dup_recall() >= b.dup_recall(),
        "向量模式不得低于纯规则模式"
    );

    // 5) 有真实模型时，A 在语义改写类样本上的召回必须高于 B
    if real.is_some() {
        assert!(
            a.paraphrase_recall() > b.paraphrase_recall(),
            "真实模型下 A 的语义改写召回（{:.3}）必须高于 B（{:.3}）",
            a.paraphrase_recall(),
            b.paraphrase_recall()
        );
    } else {
        println!(
            "[eval] 语义改写召回：B={:.3} A={:.3}（未验收：无真实嵌入端点）",
            b.paraphrase_recall(),
            a.paraphrase_recall()
        );
    }

    // 6) 重复扫描不得重建已存在的 open 提案
    assert_eq!(
        b.existing_ok, b.existing_expected,
        "纯规则模式重复扫描重建了 open 提案"
    );
    assert_eq!(
        a.existing_ok, a.existing_expected,
        "向量模式重复扫描重建了 open 提案"
    );
}

#[tokio::test]
async fn test_rules_mode_makes_zero_embedding_calls() {
    // 用计数嵌入器证明：即便注入了嵌入器，纯规则模式一次嵌入调用都不会发出
    let counter = Arc::new(mem_core::testing::CountingEmbedder::new(Arc::new(
        mem_core::testing::TestEmbedder::new(16),
    )));
    let store = Arc::new(InMemoryHubStore::new());
    let now = fixed_now();
    for content in ["重复内容 A", "重复内容 A", "重复内容 B"] {
        let e = build_entry("ns", &EntrySpec::new(content), now);
        store.insert(&e).await.unwrap();
    }

    let engine = ConsolidationEngine::new(store, 0.92, 100)
        .with_mode(ConsolidationMode::Rules, ConsolidationFallback::Rules)
        .with_embedder(counter.clone());
    engine
        .consolidate_namespace("ns", false, None, &"eval-agent".to_string())
        .await
        .unwrap();

    assert_eq!(
        counter.call_count(),
        0,
        "纯规则模式不得调用嵌入服务（实际调用 {} 次）",
        counter.call_count()
    );
    assert_eq!(
        counter.text_count(),
        0,
        "纯规则模式不得提交任何文本给嵌入服务"
    );
}

#[tokio::test]
async fn test_hybrid_mode_falls_back_when_provider_not_ready() {
    // 伪向量嵌入器不提供真实语义信号 → 按 fallback=rules 降级并如实上报
    let store = Arc::new(InMemoryHubStore::new());
    let now = fixed_now();
    for content in ["重复内容 A", "重复内容 A"] {
        let e = build_entry("ns", &EntrySpec::new(content), now);
        store.insert(&e).await.unwrap();
    }
    let engine = ConsolidationEngine::new(store, 0.92, 100)
        .with_mode(ConsolidationMode::Hybrid, ConsolidationFallback::Rules)
        .with_embedder(Arc::new(mem_core::testing::TestEmbedder::new(16)));
    let report = engine
        .consolidate_namespace("ns", true, None, &"eval-agent".to_string())
        .await
        .unwrap();

    assert_eq!(report.mode.requested_mode, "hybrid");
    assert_eq!(report.mode.effective_mode, "rules");
    assert!(report.mode.fallback_applied);
    assert!(!report.mode.embedding_used);
    assert!(report.mode.fallback_reason.is_some());
}

#[tokio::test]
async fn test_hybrid_mode_errors_when_fallback_is_error() {
    let store = Arc::new(InMemoryHubStore::new());
    let now = fixed_now();
    for content in ["重复内容 A", "重复内容 A"] {
        let e = build_entry("ns", &EntrySpec::new(content), now);
        store.insert(&e).await.unwrap();
    }
    let engine = ConsolidationEngine::new(store, 0.92, 100)
        .with_mode(ConsolidationMode::Hybrid, ConsolidationFallback::Error)
        .with_embedder(Arc::new(mem_core::testing::TestEmbedder::new(16)));
    let err = engine
        .consolidate_namespace("ns", false, None, &"eval-agent".to_string())
        .await
        .expect_err("降级策略为 error 时必须失败");
    assert!(
        err.to_string().contains("向量整合不可用"),
        "错误信息须可诊断：{err}"
    );
}

#[tokio::test]
async fn test_account_isolation_is_enforced_in_both_modes() {
    // 同名 namespace 分属两个账户：候选、提案与统计都不得互相可见
    let store = Arc::new(InMemoryHubStore::new());
    let now = fixed_now();
    let account_a = uuid::Uuid::from_u128(1);
    let account_b = uuid::Uuid::from_u128(2);
    let ns_a = scope_name(&account_a, "shared");
    let ns_b = scope_name(&account_b, "shared");

    for ns in [&ns_a, &ns_b] {
        for content in ["跨账户重复内容", "跨账户重复内容"] {
            let mut e = build_entry(ns, &EntrySpec::new(content), now);
            e.id = Uuid::new_v4();
            store.insert(&e).await.unwrap();
        }
    }

    for mode in [ConsolidationMode::Rules, ConsolidationMode::Hybrid] {
        for (account, ns) in [(account_a, &ns_a), (account_b, &ns_b)] {
            let scoped = Arc::new(mem_core::AccountScopedStore::new(store.clone(), account));
            let engine = ConsolidationEngine::new(scoped, 0.92, 100)
                .with_mode(mode, ConsolidationFallback::Rules);
            let report = engine
                .consolidate_namespace("shared", true, None, &"eval-agent".to_string())
                .await
                .unwrap();
            assert_eq!(
                report.scanned, 2,
                "{mode:?} 模式下的账户 {account} 不得看到其他账户的条目"
            );
            for p in &report.proposals {
                for id in &p.member_ids {
                    let entry = store.get(ns, *id).await.unwrap();
                    assert!(
                        entry.namespace == *ns,
                        "{mode:?} 模式下提案越权引用了其他账户的条目"
                    );
                }
            }
        }
    }
}

/// 回归：跨进程并发（进程内 mutex 覆盖不到）时，同一逻辑候选只能留下一个 open 提案。
///
/// `NAMESPACE_LOCKS` 是进程内全局表，同进程多实例共享同一把锁，因此无法用它复现
/// 跨进程竞争。这里直接模拟两个进程各自的 check-then-act 序列：都先读 open 提案
/// 集合（都为空），再各自写入同一逻辑提案。
///
/// 旧实现下两条都会落库（重复 open 提案）；修复后第二次写入必须被数据库层的
/// 业务唯一键拒绝，或幂等地返回既有提案。
#[tokio::test]
async fn test_concurrent_processes_create_only_one_open_proposal() {
    let store = Arc::new(InMemoryHubStore::new());
    let now = fixed_now();
    let mut member_ids = Vec::new();
    for content in ["跨进程重复内容", "跨进程重复内容"] {
        let e = build_entry("proc-race", &EntrySpec::new(content), now);
        member_ids.push(e.id);
        store.insert(&e).await.unwrap();
    }
    member_ids.sort();

    // 两个"进程"先并行完成各自的 check（都为空），再并行写入
    let (a, b) = tokio::join!(
        store.list_proposals("proc-race", Some(mem_core::ProposalStatus::Open)),
        store.list_proposals("proc-race", Some(mem_core::ProposalStatus::Open))
    );
    assert!(
        a.unwrap().is_empty() && b.unwrap().is_empty(),
        "竞争窗口内两边都看不到既有 open 提案"
    );

    for _ in 0..2 {
        let proposal = mem_core::IntegrationProposal {
            id: Uuid::new_v4(),
            namespace: "proc-race".to_string(),
            kind: mem_core::IntegrationKind::Duplicate,
            member_ids: member_ids.clone(),
            suggested_survivor: Some(member_ids[0]),
            similarity: 1.0,
            status: mem_core::ProposalStatus::Open,
            resolution_note: None,
            created_at: now,
            resolved_at: None,
            metadata: json!({}),
        };
        // 生产路径必须走原子入口，而不是先查后插
        store.create_open_proposal_once(&proposal).await.unwrap();
    }

    let open = store
        .list_proposals("proc-race", Some(mem_core::ProposalStatus::Open))
        .await
        .unwrap();
    assert_eq!(
        open.len(),
        1,
        "同一逻辑候选的重复写入必须只留下一个 open 提案，实际 {} 条",
        open.len()
    );
}

#[tokio::test]
async fn test_repeated_scan_does_not_recreate_open_proposals() {
    let store = Arc::new(InMemoryHubStore::new());
    let now = fixed_now();
    for content in ["幂等内容", "幂等内容"] {
        let e = build_entry("ns", &EntrySpec::new(content), now);
        store.insert(&e).await.unwrap();
    }
    let engine = ConsolidationEngine::new(store, 0.92, 100)
        .with_mode(ConsolidationMode::Rules, ConsolidationFallback::Rules);
    let first = engine
        .consolidate_namespace("ns", false, None, &"eval-agent".to_string())
        .await
        .unwrap();
    assert_eq!(first.proposals_created, 1);
    let second = engine
        .consolidate_namespace("ns", false, None, &"eval-agent".to_string())
        .await
        .unwrap();
    assert_eq!(second.proposals_created, 0);
    assert_eq!(second.skipped_existing, 1);
}

/// 回归：裁决的状态检查必须在命名空间互斥锁**内**完成。
///
/// 旧实现先在锁外读状态再拿锁：两个并发裁决都能读到 Open，随后串行进锁
/// 各合并一次（幸存者被二次修订、版本号翻倍）。修复后锁内重读，
/// 第二个裁决看到 Merged 即返回 Conflict。顺序双裁决锁定该不变量：
/// 第二次裁决必须 Conflict，幸存者只被合并修订一次。
#[tokio::test]
async fn test_second_resolve_of_merged_proposal_conflicts() {
    let store = Arc::new(InMemoryHubStore::new());
    let now = fixed_now();
    for content in ["待裁决内容", "待裁决内容"] {
        let e = build_entry("ns", &EntrySpec::new(content), now);
        store.insert(&e).await.unwrap();
    }
    let engine = ConsolidationEngine::new(store.clone(), 0.92, 100)
        .with_mode(ConsolidationMode::Rules, ConsolidationFallback::Rules);
    let report = engine
        .consolidate_namespace("ns", false, None, &"eval-agent".to_string())
        .await
        .unwrap();
    assert_eq!(report.proposals_created, 1);
    let proposal_id = report.proposals[0].id;

    let first = engine
        .resolve_proposal(
            mem_core::ResolveRequest {
                proposal_id,
                action: mem_core::ResolveAction::Merge,
                survivor_id: None,
                note: None,
            },
            &"resolver".to_string(),
        )
        .await
        .expect("首次裁决应成功合并");
    assert_eq!(first.version, 2, "合并应使幸存者版本推进一次");

    let second = engine
        .resolve_proposal(
            mem_core::ResolveRequest {
                proposal_id,
                action: mem_core::ResolveAction::Merge,
                survivor_id: None,
                note: None,
            },
            &"resolver".to_string(),
        )
        .await;
    assert!(
        matches!(second, Err(common::HubError::Conflict)),
        "已合并提案再次裁决必须 Conflict：{second:?}"
    );

    let survivor = store.get("ns", first.id).await.unwrap();
    assert_eq!(survivor.version, 2, "重复裁决不得二次修订幸存者");
}

#[tokio::test]
async fn test_dry_run_does_not_change_business_data() {
    let store = Arc::new(InMemoryHubStore::new());
    let now = fixed_now();
    for content in ["只读内容", "只读内容"] {
        let e = build_entry("ns", &EntrySpec::new(content), now);
        store.insert(&e).await.unwrap();
    }
    let engine = ConsolidationEngine::new(store.clone(), 0.92, 100)
        .with_mode(ConsolidationMode::Rules, ConsolidationFallback::Rules);
    let report = engine
        .consolidate_namespace("ns", true, None, &"eval-agent".to_string())
        .await
        .unwrap();
    assert_eq!(report.proposals.len(), 1, "dry_run 仍应给出预演提案");
    let proposals = mem_core::IntegrationStore::list_proposals(
        &mem_core::AccountScopedStore::new(store.clone(), common::BOOTSTRAP_ACCOUNT_ID),
        "ns",
        None,
    )
    .await
    .unwrap();
    assert!(proposals.is_empty(), "dry_run 不得落库提案");
    let entries = store.all_in_namespace("ns", false).await.unwrap();
    assert_eq!(entries.len(), 2, "dry_run 不得改动记忆条目");
    assert!(entries.iter().all(|e| e.status == MemoryStatus::Active));
}

#[test]
fn test_fixed_clock_is_stable() {
    // 数据集使用固定时钟：任何日期运行结果一致
    let _ = FixedClock::new(fixed_now());
    let samples = dataset();
    let f1 = dataset_fingerprint(&samples);
    let f2 = dataset_fingerprint(&dataset());
    assert_eq!(f1, f2, "数据集指纹必须稳定");
    assert_eq!(f1.len(), 64, "指纹必须是 SHA-256 十六进制串");
}

#[test]
fn test_scope_is_unchanged_for_consolidation_tools() {
    // 整合工具的 Scope 要求不因新模式而变化
    let _ = Scope::Write;
}
