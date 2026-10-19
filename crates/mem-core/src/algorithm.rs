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

/// 提取检索词元（英文单词 + CJK 整词与 2-gram），剔除低信息量功能词并按序去重
pub fn extract_terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |t: String| {
        if seen.insert(t.clone()) {
            terms.push(t);
        }
    };

    for word in query.split_whitespace() {
        let has_cjk = word.chars().any(crate::cjk::is_cjk_char);
        if !has_cjk {
            if word.chars().count() >= 2 {
                push(word.to_lowercase());
            }
        } else {
            let chars: Vec<char> = word.chars().collect();
            if chars.len() <= 4 {
                let whole = word.to_lowercase();
                if !crate::cjk::is_low_signal_bigram(&whole)
                    && !whole.chars().all(crate::cjk::is_function_char)
                {
                    push(whole);
                }
            }
            if chars.len() > 2 {
                for window in chars.windows(2) {
                    let gram: String = window.iter().collect();
                    if crate::cjk::is_low_signal_bigram(&gram) {
                        continue;
                    }
                    push(gram.to_lowercase());
                }
            }
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

/// MMR（最大边际相关性）重排
///
/// `scores` 为相关性分，`sim_matrix[i][j]` 为条目 i 与 j 的相似度。
/// 返回至多 `k` 个索引，按选中顺序排列。
pub fn mmr_rerank(scores: &[f32], sim_matrix: &[Vec<f32>], k: usize, lambda: f32) -> Vec<usize> {
    let n = scores.len();
    if n == 0 || k == 0 {
        return Vec::new();
    }
    let lambda = lambda.clamp(0.0, 1.0);
    let k = k.min(n);

    let mut selected: Vec<usize> = Vec::with_capacity(k);
    let mut remaining: Vec<usize> = (0..n).collect();

    while selected.len() < k && !remaining.is_empty() {
        let mut best_idx = remaining[0];
        let mut best_score = f32::MIN;
        for &i in &remaining {
            let max_sim = selected
                .iter()
                .map(|&j| sim_matrix[i][j])
                .fold(0.0f32, f32::max);
            let mmr = lambda * scores[i] - (1.0 - lambda) * max_sim;
            if mmr > best_score {
                best_score = mmr;
                best_idx = i;
            }
        }
        selected.push(best_idx);
        remaining.retain(|&i| i != best_idx);
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
        assert_eq!(terms.len(), 2);

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
}
