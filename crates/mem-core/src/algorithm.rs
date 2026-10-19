//! 检索打分与多样性重排算法
//!
//! 五维加权（sim 向量相似度, kw 关键词命中, conf 条目置信度,
//! rec 近期性, fresh 版本新鲜度）后接 MMR 去冗余重排。

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

fn is_cjk_char(c: char) -> bool {
    matches!(c,
        '\u{4e00}'..='\u{9fff}' |
        '\u{3400}'..='\u{4dbf}' |
        '\u{f900}'..='\u{faff}'
    )
}

/// 提取检索词元（支持英文单词与 CJK 中文 2-gram 切分）
pub fn extract_terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    for word in query.split_whitespace() {
        let has_cjk = word.chars().any(is_cjk_char);
        if !has_cjk {
            if word.chars().count() >= 2 {
                terms.push(word.to_lowercase());
            }
        } else {
            let chars: Vec<char> = word.chars().collect();
            if chars.len() <= 4 {
                terms.push(word.to_lowercase());
            }
            if chars.len() > 2 {
                for window in chars.windows(2) {
                    let gram: String = window.iter().collect();
                    terms.push(gram.to_lowercase());
                }
            }
        }
    }
    terms
}

/// 关键词命中度（0.0 ~ 1.0）：查询词在内容中的覆盖比例
///
/// 条目自身的 tags 不参与分母：标签是独立信号，标签多寡不应稀释正文命中。
/// 查询侧的标签筛选由 [`matches_tag_filter`] 负责。
pub fn keyword_score(content: &str, query: &str) -> f32 {
    let haystack = content.to_lowercase();
    let mut hits = 0usize;
    let mut total = 0usize;

    for term in extract_terms(query) {
        total += 1;
        if haystack.contains(&term) {
            hits += 1;
        }
    }
    if total == 0 {
        return 0.0;
    }
    hits as f32 / total as f32
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
    match valid_at {
        Some(t) => item.is_valid_at(t),
        None => include_superseded || !item.is_superseded_or_expired(now),
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

/// 近期性打分：时间常数 `tau_days` 天（指数衰减，`tau` 非半衰期），返回 0.0 ~ 1.0
pub fn recency_score(now: DateTime<Utc>, updated_at: DateTime<Utc>, tau_days: f32) -> f32 {
    let days = (now - updated_at).num_seconds().max(0) as f32 / 86_400.0;
    (-days / tau_days.max(1.0)).exp().clamp(0.0, 1.0)
}

/// 单条目五维打分参数
#[derive(Debug, Clone, Copy)]
pub struct ItemScoreParams {
    pub sim: f32,
    pub kw: f32,
    pub conf: f32,
    pub rec: f32,
    pub fresh: f32,
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
        p.fresh.clamp(0.0, 1.0),
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
    fn test_score_renormalizes_when_sim_unavailable() {
        let w = [0.5, 0.2, 0.1, 0.1, 0.1];
        let p = ItemScoreParams {
            sim: 0.0,
            kw: 1.0,
            conf: 1.0,
            rec: 1.0,
            fresh: 1.0,
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
        let fresh = recency_score(now, now, 30.0);
        let old = recency_score(now, now - chrono::Duration::days(30), 30.0);
        assert!(fresh > 0.99);
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
            fresh: 0.0,
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
