//! 检索打分与多样性重排算法
//!
//! 五维加权（sim 向量相似度, kw 关键词命中, conf 条目置信度,
//! rec 近期性（事件时间 + 按类型衰减）, prior 认知层级先验）后接 MMR 去冗余重排。

use crate::domain::Namespace;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// 五维权重顺序
pub const SCORE_WEIGHT_LEN: usize = 5;

/// 余弦相似度（假定输入已归一化；点积即余弦值）
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    dot.clamp(-1.0, 1.0)
}

/// L2 归一化（写入前调用，保证点积即余弦）
pub fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// 提取检索词元（与文档侧同源：ASCII 词 + CJK 1-gram/2-gram），按序去重
///
/// 查询侧与文档侧**必须**用同一套词法：文档侧词元由
/// [`crate::cjk::fts_doc_tokens`] 产生（标点分隔 + CJK 1-gram/2-gram），
/// 打分时用 [`document_tokens`] 取文档词元集合。
///
/// 此前查询侧按**空白**切词并额外产出"整词"词元，带来两个真实缺陷：
///
/// 1. 带标点的词元永远匹配不上文档——`"os:linux】"` 作为一个整词进入分母，
///    而文档侧切出的是 `os` / `linux`，于是环境标记（`【环境 redis:7】`）
///    对打分完全不敏感；
/// 2. 3 字以上的 CJK 整词（`"目标查询"`）同样在文档里不存在（文档只有
///    1-gram / 2-gram），白白占住 IDF 分母，把 kw 系统性压低。
///
/// 死词元只进分母不进分子，等于给所有查询的匹配度打折。
pub fn extract_terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for t in crate::cjk::fts_doc_tokens(query) {
        if t.is_empty() {
            continue;
        }
        if seen.insert(t.clone()) {
            terms.push(t);
        }
    }
    terms
}

/// 文档词元集合（与 FTS 索引使用同一套词法，保证三处语义一致）
pub fn document_tokens(content: &str) -> std::collections::HashSet<String> {
    crate::cjk::fts_doc_tokens(content).into_iter().collect()
}

/// 池内平滑 IDF：`ln(1 + pool_size / (df + 1))`，恒大于 0。
///
/// 稀有词元权重高、常见词元（含高频 bigram）权重低；缺少全局语料统计时，
/// 以候选池内的文档频次作为语料统计的局部近似。
pub fn inverse_document_frequency(pool_size: usize, doc_freq: usize) -> f32 {
    let pool = pool_size.max(1) as f32;
    let df = doc_freq as f32;
    (1.0 + pool / (df + 1.0)).ln()
}

/// 关键词命中度（0.0 ~ 1.0）：查询词元在内容中的命中比例。
///
/// - 命中判定基于**词元集合**（与 FTS 同源），不做裸子串匹配：
///   英文 `api` 不再误命中 `rapid`；
/// - 条目自身 tags 不参与分母（标签是独立信号，由 [`matches_tag_filter`] 负责）；
/// - 平权版本（`idf` 恒为 1）用于无池上下文的对拍与测试。
pub fn keyword_score(content: &str, query: &str) -> f32 {
    let tokens = document_tokens(content);
    let terms = extract_terms(query);
    keyword_score_weighted(&tokens, &terms, &|_| 1.0)
}

/// 关键词命中度（IDF 加权版）：`Σ idf(命中词元) / Σ idf(全部查询词元)`
pub fn keyword_score_weighted(
    doc_tokens: &std::collections::HashSet<String>,
    terms: &[String],
    idf: &dyn Fn(&str) -> f32,
) -> f32 {
    let mut numerator = 0.0f32;
    let mut denominator = 0.0f32;
    for term in terms {
        let weight = idf(term).max(0.0);
        denominator += weight;
        if doc_tokens.contains(term) {
            numerator += weight;
        }
    }
    if denominator <= f32::EPSILON {
        0.0
    } else {
        (numerator / denominator).clamp(0.0, 1.0)
    }
}

/// 检索参考时点：显式 `valid_at` 优先，否则取当前时刻。
///
/// 时间有效性过滤与失效惩罚必须使用同一参考时点，避免"按历史时点过滤、
/// 却按墙上时钟惩罚"导致历史版本查询排序倒挂。
pub fn retrieval_reference_time(
    valid_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> DateTime<Utc> {
    valid_at.unwrap_or(now)
}

/// 时间有效性过滤（三套检索路径共用的统一语义）
///
/// - 显式给出 `valid_at` 时一律按 `valid_from <= t < valid_until` 过滤，
///   `include_superseded` 不绕过该约束（它只放宽"当前已失效"的排除）；
/// - 未显式给出时，仅在 `include_superseded` 为假时排除已失效条目。
pub fn passes_validity_filter(
    item: &crate::domain::MemoryEntry,
    valid_at: Option<DateTime<Utc>>,
    include_superseded: bool,
    now: DateTime<Utc>,
) -> bool {
    match validity_filter_time(valid_at, include_superseded, now) {
        Some(t) => item.is_valid_at(t),
        None => true,
    }
}

/// 与 [`passes_validity_filter`] 配套的**过滤下推**：
/// 把同一语义表达为存储层的 `MemoryFilter.valid_at` 约束。
///
/// - 显式 `valid_at` → 用该时刻；
/// - 未给出且不要求包含失效条目 → 用当前时刻（"当前有效"= `valid_from <= now < valid_until`，
///   同时排除尚未生效的未来事实）；
/// - 要求包含失效条目 → 不做时间约束。
pub fn validity_filter_time(
    valid_at: Option<DateTime<Utc>>,
    include_superseded: bool,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    match valid_at {
        Some(t) => Some(t),
        None if include_superseded => None,
        None => Some(now),
    }
}

/// 失效惩罚判定：以检索参考时点为基准（而非墙上时钟）
pub fn is_superseded_at(item: &crate::domain::MemoryEntry, reference_time: DateTime<Utc>) -> bool {
    item.is_superseded_or_expired(reference_time)
}

/// 查询侧标签筛选：请求中给出的标签必须全部命中条目标签
pub fn matches_tag_filter(item: &crate::domain::MemoryEntry, tags: &Option<Vec<String>>) -> bool {
    match tags {
        Some(tags) if !tags.is_empty() => tags.iter().all(|t| item.tags.contains(t)),
        _ => true,
    }
}

/// 近期性打分：时间常数 `tau_days` 天（指数衰减，`tau` 非半衰期），返回 0.0 ~ 1.0。
///
/// 调用方应传入**事件时间**（`occurred_at`），缺失时才回落到写入时间
/// （`updated_at`）：记忆的"近期性"取决于事件何时发生，而不是何时被记录。
pub fn recency_score(now: DateTime<Utc>, at: DateTime<Utc>, tau_days: f32) -> f32 {
    let days = (now - at).num_seconds().max(0) as f32 / 86_400.0;
    (-days / tau_days.max(1.0)).exp().clamp(0.0, 1.0)
}

/// 按记忆类型区分的时间常数（天）：稳定型记忆衰减慢，短期型衰减快。
///
/// 统一 `tau=30` 会把"长期偏好"和"一次性事件"同等衰减；这里按类型给出
/// 不同时间尺度，半衰期 ≈ `tau·ln2` 天。
pub fn recency_tau_days_for(kind: crate::domain::MemoryKind) -> f32 {
    use crate::domain::MemoryKind;
    match kind {
        MemoryKind::Preference => 180.0,
        MemoryKind::Skill | MemoryKind::Procedure => 90.0,
        MemoryKind::Fact => 60.0,
        MemoryKind::Episode => 14.0,
        MemoryKind::Note => 7.0,
    }
}

/// 认知层级先验（0.0 ~ 1.0）：同等相关性下，结构化知识优先于原始观测。
///
/// 取代原先的"版本新鲜度"二值信号（`version > 1 → 1.0 / 0.5`）——编辑次数多
/// 并不代表记忆更新鲜，该信号与"新鲜"无关。
pub fn layer_prior(layer: Option<crate::domain::CognitiveLayer>) -> f32 {
    use crate::domain::CognitiveLayer;
    match layer {
        Some(CognitiveLayer::Identity) | Some(CognitiveLayer::Schema) => 1.0,
        Some(CognitiveLayer::Knowledge) | Some(CognitiveLayer::Fact) => 0.8,
        Some(CognitiveLayer::Intention) => 0.7,
        Some(CognitiveLayer::Summary) => 0.6,
        Some(CognitiveLayer::Raw) | None => 0.5,
    }
}

/// 单条目五维打分参数
#[derive(Debug, Clone, Copy)]
pub struct ItemScoreParams {
    pub sim: f32,
    pub kw: f32,
    pub conf: f32,
    pub rec: f32,
    /// 认知层级先验（见 [`layer_prior`]），此前为无意义的版本新鲜度信号
    pub prior: f32,
}

/// 加权综合分 = Σ wᵢ·sᵢ
pub fn calculate_item_score(p: &ItemScoreParams, w: &[f32; SCORE_WEIGHT_LEN]) -> f32 {
    let raw = clamped_signals(p);
    raw.iter().zip(w.iter()).map(|(s, w)| s * w).sum()
}

/// 加权综合分（缺失维度按可用信号重新归一化）
///
/// 当某维度不可用（如嵌入服务故障、条目缺失向量、嵌入模型不匹配）时，
/// 其权重不应简单按 0 计——否则所有分数被整体压低，`min_score` 阈值失真、
/// 无向量条目被系统性压制。此处按可用维度的权重之和归一化。
pub fn calculate_item_score_with_availability(
    p: &ItemScoreParams,
    w: &[f32; SCORE_WEIGHT_LEN],
    available: [bool; SCORE_WEIGHT_LEN],
) -> f32 {
    let raw = clamped_signals(p);
    let mut numerator = 0.0f32;
    let mut weight_sum = 0.0f32;
    for i in 0..SCORE_WEIGHT_LEN {
        if available[i] {
            numerator += raw[i] * w[i];
            weight_sum += w[i];
        }
    }
    if weight_sum <= f32::EPSILON {
        0.0
    } else {
        numerator / weight_sum
    }
}

fn clamped_signals(p: &ItemScoreParams) -> [f32; SCORE_WEIGHT_LEN] {
    [
        p.sim.clamp(0.0, 1.0),
        p.kw.clamp(0.0, 1.0),
        p.conf.clamp(0.0, 1.0),
        p.rec.clamp(0.0, 1.0),
        p.prior.clamp(0.0, 1.0),
    ]
}

/// MMR（最大边际相关性）重排——矩阵版
///
/// `scores` 为相关性分，`sim_matrix[i][j]` 为条目 i 与 j 的相似度。
/// 返回至多 `k` 个索引，按选中顺序排列。
///
/// 内部委托 [`mmr_rerank_by`]。矩阵版要求调用方先做 O(n²) 相似度预计算，
/// 大候选池下这比选择循环本身更贵；新代码建议直接用惰性版。
pub fn mmr_rerank(scores: &[f32], sim_matrix: &[Vec<f32>], k: usize, lambda: f32) -> Vec<usize> {
    mmr_rerank_by(scores, k, lambda, |i, j| {
        sim_matrix
            .get(i)
            .and_then(|row| row.get(j))
            .copied()
            .unwrap_or(0.0)
    })
}

/// MMR（最大边际相关性）重排——惰性相似度版
///
/// `sim(i, j)` 按需返回条目 i 与 j 的相似度（负值按 0 处理，与矩阵版
/// `fold(0.0, max)` 的语义一致）。选择循环只查询"新选中 × 剩余"的相似度，
/// 总计算量 O(k·n) 而非 O(n²)：候选池为 top_k 的 4 倍时，top_k=100、
/// 高维向量场景下省掉一个数量级的余弦计算。结果与矩阵版逐条一致
/// （`max_sim` 是同一集合 max 的增量维护，遍历顺序与比较符不变）。
pub fn mmr_rerank_by<F>(scores: &[f32], k: usize, lambda: f32, sim: F) -> Vec<usize>
where
    F: Fn(usize, usize) -> f32,
{
    let n = scores.len();
    if n == 0 || k == 0 {
        return Vec::new();
    }
    let lambda = lambda.clamp(0.0, 1.0);
    let k = k.min(n);

    let mut selected: Vec<usize> = Vec::with_capacity(k);
    let mut remaining: Vec<usize> = (0..n).collect();
    // max_sim[i] = max(0, 与已选中条目的相似度)
    let mut max_sim: Vec<f32> = vec![0.0; n];

    while selected.len() < k && !remaining.is_empty() {
        let mut best_idx = remaining[0];
        let mut best_score = f32::MIN;
        for &i in &remaining {
            let mmr = lambda * scores[i] - (1.0 - lambda) * max_sim[i];
            if mmr > best_score {
                best_score = mmr;
                best_idx = i;
            }
        }
        let chosen = best_idx;
        selected.push(chosen);
        remaining.retain(|&i| i != chosen);
        for &i in &remaining {
            let s = sim(i, chosen);
            if s > max_sim[i] {
                max_sim[i] = s;
            }
        }
    }
    selected
}

/// 场景过滤模式
///
/// - `Strict`（默认）：指定 `scene` 时硬过滤，其它场景与未标注场景的条目都不召回，
///   保持既有行为不变；
/// - `Prefer`：不排除其它场景，改为"同场景加成、其它场景降权、未标注场景中性"，
///   让跨场景的强相关条目也能被 Agent 看到。开启前需经评测确认收益。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SceneMode {
    #[default]
    Strict,
    Prefer,
}

/// 场景偏好加成（prefer 模式下同场景命中）
pub const SCENE_PREFER_BONUS: f32 = 0.05;
/// 场景偏好降权（prefer 模式下其它已标注场景命中）
pub const SCENE_PREFER_PENALTY: f32 = 0.05;

/// 应用场景偏好：返回调整后的综合分（`None` 场景视为中性，不做任何调整）
fn apply_scene_preference(
    score: f32,
    requested: Option<crate::domain::MemoryScene>,
    item_scene: Option<crate::domain::MemoryScene>,
) -> f32 {
    match (requested, item_scene) {
        (Some(want), Some(got)) if want == got => (score + SCENE_PREFER_BONUS).min(1.0),
        (Some(_), Some(_)) => (score - SCENE_PREFER_PENALTY).max(0.0),
        _ => score,
    }
}

/// 场景偏好打分（供检索器在 prefer 模式下使用）
pub fn scene_preference_score(
    score: f32,
    requested: Option<crate::domain::MemoryScene>,
    item_scene: Option<crate::domain::MemoryScene>,
) -> f32 {
    apply_scene_preference(score, requested, item_scene)
}

/// 检索查询（core 定义，retrieval crate 实现）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchQuery {
    pub namespace: Namespace,
    /// 自然语言查询（可空：纯列表召回）
    #[serde(default)]
    pub query_text: String,
    #[serde(default)]
    pub kinds: Option<Vec<crate::domain::MemoryKind>>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    pub top_k: usize,
    /// 是否包含已归档条目
    #[serde(default)]
    pub include_archived: bool,
    /// 最小综合分阈值（[0.0, 1.0]，低于此分的条目被过滤）
    #[serde(default)]
    pub min_score: Option<f32>,
    /// 认知场景过滤（Chat / Working / Coding）
    #[serde(default)]
    pub scene: Option<crate::domain::MemoryScene>,
    /// 认知层级过滤
    #[serde(default)]
    pub layer: Option<crate::domain::CognitiveLayer>,
    /// 时间有效性断言：查询在特定时刻有效的事实（valid_from <= t < valid_until）
    #[serde(default)]
    pub valid_at: Option<chrono::DateTime<chrono::Utc>>,
    /// 是否返回已失效（superseded）的旧事实以供演化回溯
    #[serde(default)]
    pub include_superseded: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_similarity_basic() {
        let a = vec![1.0, 0.0];
        let b = vec![1.0, 0.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-6);

        let c = vec![0.0, 1.0];
        assert!((cosine_similarity(&a, &c)).abs() < 1e-6);

        // 维度不匹配返回 0
        assert_eq!(cosine_similarity(&a, &[1.0, 0.0, 0.0]), 0.0);
    }

    #[test]
    fn test_l2_normalize() {
        let mut v = vec![3.0, 4.0];
        l2_normalize(&mut v);
        assert!((v[0] - 0.6).abs() < 1e-6);
        assert!((v[1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn test_keyword_score() {
        let content = "用户偏好简短直接的中文回复";
        let s = keyword_score(content, "简短");
        assert!(s > 0.4 && s <= 1.0);

        // 空查询：0
        assert_eq!(keyword_score(content, ""), 0.0);
    }

    #[test]
    fn test_keyword_score_ignores_entry_tags() {
        // 标签是独立信号，不再计入正文覆盖分母（此前标签越多 kw 越低）
        let content = "服务部署在东京区域";
        assert_eq!(keyword_score(content, "东京"), 1.0);
        assert_eq!(keyword_score(content, "东京 架构"), 0.5);
    }

    #[test]
    fn test_cjk_keyword_matching() {
        let content = "用户偏好简短直接的中文回复";
        // 未分词的长中文查询通过 2-gram 依然能精确召回命中子串
        let s = keyword_score(content, "偏好直接");
        assert!(s > 0.0, "CJK 2-gram 应成功命中 '偏好' 与 '直接'");
    }

    #[test]
    fn test_ascii_terms_match_on_token_boundary() {
        // 回归：裸子串匹配让 "api" 命中 "rapid"
        assert_eq!(keyword_score("rapid prototyping", "api"), 0.0);
        assert_eq!(keyword_score("api gateway", "api"), 1.0);
    }

    #[test]
    fn test_idf_weighting_prefers_rare_terms() {
        let rare_doc = document_tokens("东京节点上线");
        let common_doc = document_tokens("缓存策略说明");
        let terms = extract_terms("东京 缓存");
        // 查询侧与文档侧同源："东京" → 1-gram 东/京 + 2-gram 东京，共 3 个词元
        assert_eq!(terms.len(), 6);
        assert!(terms.contains(&"东京".to_string()));
        assert!(terms.contains(&"东".to_string()));

        // 池内：东京 1 篇、缓存 7 篇
        let idf = |term: &str| match term {
            "东京" => inverse_document_frequency(8, 1),
            "缓存" => inverse_document_frequency(8, 7),
            _ => 1.0,
        };
        let rare = keyword_score_weighted(&rare_doc, &terms, &idf);
        let common = keyword_score_weighted(&common_doc, &terms, &idf);
        assert!(
            rare > common,
            "稀有词元命中应高于常见词元命中：{rare} vs {common}"
        );
        assert!(inverse_document_frequency(8, 1) > inverse_document_frequency(8, 7));
    }

    /// 回归：查询侧词元必须与文档侧同源
    ///
    /// 此前查询侧按空白切词，`"os:linux】"` 这样的带标点词元在文档侧
    /// 根本不存在（文档切出的是 `os` / `linux`），于是它只进分母不进分子，
    /// 把 kw 压低，也让环境标记对打分完全失效。
    #[test]
    fn test_query_terms_share_document_tokenizer() {
        let terms = extract_terms("DNS 解析超时排查 dig 【环境 os:linux】");
        for expected in ["dns", "dig", "os", "linux", "环境", "解析"] {
            assert!(
                terms.iter().any(|t| t == expected),
                "查询词元应包含 {expected}（实际: {terms:?}）"
            );
        }
        // 标点不得粘进词元
        assert!(
            !terms
                .iter()
                .any(|t| t.contains('【') || t.contains('】') || t.contains(':')),
            "词元不应包含标点：{terms:?}"
        );
        // 3 字以上 CJK 整词在文档侧不存在，不应作为死词元进入分母
        assert!(
            !terms.contains(&"目标查询".to_string()),
            "多字整词不应进入查询词元：{terms:?}"
        );
    }

    /// 回归：带标点的查询必须能命中对应文档
    #[test]
    fn test_punctuated_query_matches_document_token() {
        let doc = document_tokens("【适用环境 os:linux】先做本机 dig 对比公共 DNS");
        let terms = extract_terms("【环境 os:linux】");
        let score = keyword_score_weighted(&doc, &terms, &|_| 1.0);
        assert!(score > 0.0, "带标点的环境标记必须能命中文档：{score}");
    }

    #[test]
    fn test_kind_specific_recency_tau_and_layer_prior() {
        use crate::domain::{CognitiveLayer, MemoryKind};
        assert!(
            recency_tau_days_for(MemoryKind::Preference) > recency_tau_days_for(MemoryKind::Fact)
        );
        assert!(recency_tau_days_for(MemoryKind::Fact) > recency_tau_days_for(MemoryKind::Note));

        // 层级先验：结构化知识 > 摘要 > 原始观测
        assert!(
            layer_prior(Some(CognitiveLayer::Identity))
                > layer_prior(Some(CognitiveLayer::Summary))
        );
        assert!(
            layer_prior(Some(CognitiveLayer::Summary)) > layer_prior(Some(CognitiveLayer::Raw))
        );
        assert_eq!(layer_prior(None), layer_prior(Some(CognitiveLayer::Raw)));
    }

    #[test]
    fn test_validity_filter_time_matches_rust_side_semantics() {
        use chrono::TimeZone;
        let now = Utc.with_ymd_and_hms(2026, 10, 19, 0, 0, 0).unwrap();
        let t = Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap();
        // 显式 valid_at 优先
        assert_eq!(validity_filter_time(Some(t), false, now), Some(t));
        assert_eq!(validity_filter_time(Some(t), true, now), Some(t));
        // 未给出且不包含失效条目 → 以当前时刻过滤
        assert_eq!(validity_filter_time(None, false, now), Some(now));
        // 包含失效条目 → 不做时间约束
        assert_eq!(validity_filter_time(None, true, now), None);
    }

    #[test]
    fn test_score_renormalizes_when_sim_unavailable() {
        let w = [0.5, 0.2, 0.1, 0.1, 0.1];
        let p = ItemScoreParams {
            sim: 0.0,
            kw: 1.0,
            conf: 1.0,
            rec: 1.0,
            prior: 1.0,
        };
        // sim 不可用时不应整体缩水（旧口径上限 0.5），而应按剩余信号归一化
        let renormalized =
            calculate_item_score_with_availability(&p, &w, [false, true, true, true, true]);
        assert!(
            (renormalized - 1.0).abs() < 1e-6,
            "其余信号全满时应得 1.0：{renormalized}"
        );

        // sim 可用时与旧口径一致
        let full = calculate_item_score_with_availability(&p, &w, [true; 5]);
        let legacy = calculate_item_score(&p, &w);
        assert!((full - legacy).abs() < 1e-6);
    }

    #[test]
    fn test_recency_score_decays() {
        let now = Utc::now();
        let recent = recency_score(now, now, 30.0);
        let old = recency_score(now, now - chrono::Duration::days(30), 30.0);
        assert!(recent > 0.99);
        assert!(old < 0.4);
    }

    #[test]
    fn test_weighted_score() {
        let w = [0.5, 0.2, 0.1, 0.1, 0.1];
        let p = ItemScoreParams {
            sim: 1.0,
            kw: 0.0,
            conf: 0.0,
            rec: 0.0,
            prior: 0.0,
        };
        assert!((calculate_item_score(&p, &w) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn test_mmr_rerank_diversifies() {
        // 两条完全相同的冗余结果：MMR 应只选一条
        let scores = vec![1.0, 0.95];
        let sim = vec![vec![1.0, 1.0], vec![1.0, 1.0]];
        let picked = mmr_rerank(&scores, &sim, 1, 0.7);
        assert_eq!(picked.len(), 1);

        // 两条完全不同：两条都选
        let sim2 = vec![vec![1.0, 0.0], vec![0.0, 1.0]];
        let picked2 = mmr_rerank(&scores, &sim2, 2, 0.7);
        assert_eq!(picked2.len(), 2);
        assert_eq!(picked2[0], 0); // 相关性高的先选
    }

    #[test]
    fn test_mmr_empty() {
        assert!(mmr_rerank(&[], &[], 5, 0.7).is_empty());
        assert!(mmr_rerank(&[1.0], &[vec![1.0]], 0, 0.7).is_empty());
    }

    /// 确定性伪随机流（xorshift64*），无外部依赖
    struct XorShift(u64);
    impl XorShift {
        fn next_f32(&mut self) -> f32 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            let v = self.0.wrapping_mul(0x2545_F491_4F6C_DD1D);
            ((v >> 40) as f32) / ((1u64 << 24) as f32) * 2.0 - 1.0
        }
    }

    /// 回归：惰性 MMR 与矩阵 MMR 输出必须逐条一致
    #[test]
    fn test_mmr_lazy_matches_matrix() {
        let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
        for (n, k, lambda) in [(30usize, 7usize, 0.7f32), (1, 5, 0.5), (8, 8, 1.0), (12, 3, 0.0)] {
            let scores: Vec<f32> = (0..n).map(|_| rng.next_f32()).collect();
            let matrix: Vec<Vec<f32>> = (0..n)
                .map(|_| (0..n).map(|_| rng.next_f32()).collect())
                .collect();
            let by_matrix = mmr_rerank(&scores, &matrix, k, lambda);
            let by_lazy = mmr_rerank_by(&scores, k, lambda, |i, j| matrix[i][j]);
            assert_eq!(
                by_matrix, by_lazy,
                "n={n} k={k} lambda={lambda}: 惰性版与矩阵版选择顺序必须一致"
            );
        }
    }

    /// 微基准（手动运行）：O(n²) 矩阵预计算 vs 惰性相似度的重排总耗时。
    /// ```bash
    /// cargo test -p mem-core --lib algorithm::tests::test_mmr_benchmark -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "微基准：手动运行"]
    fn test_mmr_benchmark() {
        let n = 400usize; // top_k=100 × MMR_POOL_FACTOR=4
        let dim = 1024usize;
        let k = 100usize;
        let mut rng = XorShift(0xDEADBEEF_CAFEF00D);
        let vecs: Vec<Vec<f32>> = (0..n).map(|_| (0..dim).map(|_| rng.next_f32()).collect()).collect();
        let scores: Vec<f32> = (0..n).map(|i| 1.0 - i as f32 / n as f32).collect();
        let cosine = |a: &Vec<f32>, b: &Vec<f32>| -> f32 {
            a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>()
        };

        let t0 = std::time::Instant::now();
        let matrix: Vec<Vec<f32>> = vecs
            .iter()
            .map(|a| vecs.iter().map(|b| cosine(a, b).max(0.0)).collect())
            .collect();
        let matrix_build = t0.elapsed();
        let t1 = std::time::Instant::now();
        let order = mmr_rerank(&scores, &matrix, k, 0.7);
        let matrix_total = t1.elapsed();

        let t2 = std::time::Instant::now();
        let order_lazy = mmr_rerank_by(&scores, k, 0.7, |i, j| cosine(&vecs[i], &vecs[j]).max(0.0));
        let lazy_total = t2.elapsed();

        assert_eq!(order, order_lazy, "等价性：选择顺序必须一致");
        println!(
            "n={n} dim={dim} k={k}：矩阵版 = 预计算 {matrix_build:?} + 选择 {matrix_total:?}；惰性版总计 {lazy_total:?}"
        );
    }
}
