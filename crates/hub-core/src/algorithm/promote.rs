//! 算法 C：推断条目升格为长期信念（设计规范 17.4 C）

use crate::domain::Sensitivity;
use chrono::Duration;

/// 候选升格条目特征
#[derive(Debug, Clone)]
pub struct PromotionCandidate {
    /// 观察次数
    pub n_obs: u32,
    /// 不同的场景标签/上下文数量
    pub distinct_contexts: u32,
    /// 观察跨度时间
    pub time_span: Duration,
    /// 当前置信度
    pub confidence: f32,
    /// 敏感级别
    pub sensitivity: Sensitivity,
}

/// 升格校验阈值配置
#[derive(Debug, Clone)]
pub struct PromotionConfig {
    pub min_obs: u32,
    pub min_contexts: u32,
    pub min_span_days: i64,
    pub min_conf: f32,
}

impl Default for PromotionConfig {
    fn default() -> Self {
        Self {
            min_obs: 3,
            min_contexts: 2,
            min_span_days: 7,
            min_conf: 0.7,
        }
    }
}

/// 判定是否允许升格为长期信念条目
///
/// 约束规则：
/// - 严禁升格 Blocked 敏感条目
/// - 必须同时满足: n_obs >= 3, distinct_contexts >= 2, time_span >= 7天, confidence >= 0.7
/// - 单次行为绝对不得升格
pub fn can_promote_to_belief(
    candidate: &PromotionCandidate,
    config: &PromotionConfig,
) -> Result<(), &'static str> {
    if candidate.sensitivity == Sensitivity::Blocked {
        return Err("Sensitive blocked content can never be promoted or stored");
    }

    if candidate.n_obs < config.min_obs {
        return Err("Insufficient observation count (must be >= 3)");
    }

    if candidate.distinct_contexts < config.min_contexts {
        return Err("Insufficient context diversity (must be across >= 2 contexts)");
    }

    if candidate.time_span < Duration::days(config.min_span_days) {
        return Err("Observation time span is too short (must be >= 7 days)");
    }

    if candidate.confidence < config.min_conf {
        return Err("Confidence score is too low (must be >= 0.7)");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_promotion() {
        let candidate = PromotionCandidate {
            n_obs: 4,
            distinct_contexts: 2,
            time_span: Duration::days(8),
            confidence: 0.75,
            sensitivity: Sensitivity::Normal,
        };
        assert!(can_promote_to_belief(&candidate, &PromotionConfig::default()).is_ok());
    }

    #[test]
    fn test_blocked_item_rejected() {
        let candidate = PromotionCandidate {
            n_obs: 10,
            distinct_contexts: 5,
            time_span: Duration::days(30),
            confidence: 0.99,
            sensitivity: Sensitivity::Blocked,
        };
        assert!(can_promote_to_belief(&candidate, &PromotionConfig::default()).is_err());
    }

    #[test]
    fn test_insufficient_criteria() {
        let config = PromotionConfig::default();

        // 观察不足
        let c1 = PromotionCandidate {
            n_obs: 2,
            distinct_contexts: 2,
            time_span: Duration::days(10),
            confidence: 0.8,
            sensitivity: Sensitivity::Normal,
        };
        assert!(can_promote_to_belief(&c1, &config).is_err());

        // 场景单一
        let c2 = PromotionCandidate {
            n_obs: 5,
            distinct_contexts: 1,
            time_span: Duration::days(10),
            confidence: 0.8,
            sensitivity: Sensitivity::Normal,
        };
        assert!(can_promote_to_belief(&c2, &config).is_err());

        // 时间跨度不足
        let c3 = PromotionCandidate {
            n_obs: 5,
            distinct_contexts: 3,
            time_span: Duration::days(3),
            confidence: 0.8,
            sensitivity: Sensitivity::Normal,
        };
        assert!(can_promote_to_belief(&c3, &config).is_err());

        // 置信度不足
        let c4 = PromotionCandidate {
            n_obs: 5,
            distinct_contexts: 3,
            time_span: Duration::days(10),
            confidence: 0.65,
            sensitivity: Sensitivity::Normal,
        };
        assert!(can_promote_to_belief(&c4, &config).is_err());
    }
}
