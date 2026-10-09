//! 算法 A：记忆检索打分与 MMR 多样性重排（设计规范 17.4 A）

use crate::domain::Provenance;

/// 记忆评分输入参数
#[derive(Debug, Clone)]
pub struct ItemScoreParams {
    /// 向量余弦相似度归一化得分 (0.0 ~ 1.0)
    pub sim: f32,
    /// 全文关键词匹配得分归一化 (0.0 ~ 1.0)
    pub kw: f32,
    /// 置信度 (0.0 ~ 1.0)
    pub conf: f32,
    /// 时间就近性衰减得分 rec = exp(-Δt / τ) (0.0 ~ 1.0)
    pub rec: f32,
    /// 来源可信度类型
    pub prov: Provenance,
}

/// 计算综合相关性得分
///
/// 权重公式：
/// `score = 0.40·sim + 0.20·kw + 0.15·conf + 0.10·rec + 0.15·prov`
/// 其中 prov 得分映射：
/// - Stated: 1.0
/// - Observed: 0.6
/// - Inferred: 0.3
pub fn calculate_item_score(params: &ItemScoreParams, weights: &[f32; 5]) -> f32 {
    let prov_score = match params.prov {
        Provenance::Stated => 1.0,
        Provenance::Observed => 0.6,
        Provenance::Inferred => 0.3,
    };

    let w_sim = weights[0];
    let w_kw = weights[1];
    let w_conf = weights[2];
    let w_rec = weights[3];
    let w_prov = weights[4];

    (w_sim * params.sim.clamp(0.0, 1.0)
        + w_kw * params.kw.clamp(0.0, 1.0)
        + w_conf * params.conf.clamp(0.0, 1.0)
        + w_rec * params.rec.clamp(0.0, 1.0)
        + w_prov * prov_score)
        .clamp(0.0, 1.0)
}

/// MMR (Maximal Marginal Relevance) 多样性重排
///
/// 公式：
/// `MMR = argmax_{D_i \in R \setminus S} [ λ · Sim(D_i, Q) - (1 - λ) · max_{D_j \in S} Sim(D_i, D_j) ]`
///
/// - `relevance_scores`: 各候选条目的基准相关性得分
/// - `similarity_matrix`: 候选条目相互之间的相似度矩阵 (NxN)
/// - `top_k`: 最终保留数量
/// - `lambda`: 相关性与多样性权衡因子（默认 0.7）
pub fn mmr_rerank(
    relevance_scores: &[f32],
    similarity_matrix: &[Vec<f32>],
    top_k: usize,
    lambda: f32,
) -> Vec<usize> {
    let n = relevance_scores.len();
    if n == 0 || top_k == 0 {
        return Vec::new();
    }

    let mut selected: Vec<usize> = Vec::with_capacity(top_k);
    let mut remaining: Vec<usize> = (0..n).collect();

    // 第一步：选出相关性最高的条目
    let mut best_first_idx = 0;
    let mut best_first_score = f32::NEG_INFINITY;
    for (idx, &score) in relevance_scores.iter().enumerate() {
        if score > best_first_score {
            best_first_score = score;
            best_first_idx = idx;
        }
    }

    selected.push(best_first_idx);
    remaining.retain(|&idx| idx != best_first_idx);

    // 迭代选出剩余条目
    while selected.len() < top_k && !remaining.is_empty() {
        let mut best_candidate = remaining[0];
        let mut best_mmr_score = f32::NEG_INFINITY;

        for &cand_idx in &remaining {
            let rel = relevance_scores[cand_idx];

            // 计算该候选与已选集合中的最大相似度
            let max_sim = selected
                .iter()
                .map(|&sel_idx| similarity_matrix[cand_idx][sel_idx])
                .fold(f32::NEG_INFINITY, f32::max);

            let mmr_score = lambda * rel - (1.0 - lambda) * max_sim;

            if mmr_score > best_mmr_score {
                best_mmr_score = mmr_score;
                best_candidate = cand_idx;
            }
        }

        selected.push(best_candidate);
        remaining.retain(|&idx| idx != best_candidate);
    }

    selected
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_item_score() {
        let weights = [0.40, 0.20, 0.15, 0.10, 0.15];
        let params = ItemScoreParams {
            sim: 0.9,
            kw: 0.8,
            conf: 0.7,
            rec: 0.6,
            prov: Provenance::Stated, // 1.0
        };
        // 0.40*0.9 + 0.20*0.8 + 0.15*0.7 + 0.10*0.6 + 0.15*1.0
        // = 0.36 + 0.16 + 0.105 + 0.06 + 0.15 = 0.835
        let score = calculate_item_score(&params, &weights);
        assert!((score - 0.835).abs() < 1e-4);
    }

    #[test]
    fn test_mmr_diversity() {
        // 3 个条目：
        // 0 和 1 文本极其相似 (sim = 0.95)，但 0 得分略高于 1
        // 2 文本与 0/1 完全不同 (sim = 0.1)，但得分稍低
        let rel = vec![0.9, 0.88, 0.75];
        let sim = vec![
            vec![1.0, 0.95, 0.1],
            vec![0.95, 1.0, 0.1],
            vec![0.1, 0.1, 1.0],
        ];

        let selected = mmr_rerank(&rel, &sim, 2, 0.7);
        // MMR 应该先选 0，然后由于 1 与 0 极其重复，第二个应该偏向多样性的 2
        assert_eq!(selected, vec![0, 2]);
    }
}
