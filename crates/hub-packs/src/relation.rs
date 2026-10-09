//! 天理与人欲：关系规范集三阶段流水线（基于设计规范 8.5 节）
//!
//! 包含：
//! 1. 差序格局（费孝通）：圈层判定与关系类型归纳
//! 2. 人情与面子（黄光国）：法则策略选择（需求法则、人情法则、公平法则）
//! 3. 礼貌准则（顾曰国）：称呼、谦敬与措辞修饰建议

use serde::{Deserialize, Serialize};

/// 差序格局圈层
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationRing {
    SelfRing,
    Family,
    Kin,
    Acquaintance,
    Colleague,
    Stranger,
}

/// 关系类型（黄光国社会学模型）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationType {
    /// 情感性关系（家人、挚友）-> 适用需求法则
    Emotional,
    /// 混合性关系（熟人、同事、亲戚）-> 适用人情法则
    Mixed,
    /// 工具性关系（商务、陌生人、纯事务往来）-> 适用公平法则
    Instrumental,
}

/// 关系情境推断输入
#[derive(Debug, Clone)]
pub struct RelationInput {
    pub role: String,
    pub status_diff: Option<String>, // "superior", "peer", "subordinate"
    pub scene: Option<String>,       // "qingtuo", "daily", "business"
}

/// 流水线决策结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelationDecision {
    pub ring: RelationRing,
    pub relation_type: RelationType,
    pub applicable_rule: &'static str,
    pub expression_advice: String,
}

/// 关系规范集流水线执行器
pub struct RelationPipeline;

impl RelationPipeline {
    /// 执行三阶段推断与策略生成
    pub fn process(input: &RelationInput) -> RelationDecision {
        // 阶段 1：差序格局判定圈层与关系类型
        let (ring, relation_type) = match input.role.as_str() {
            "家人" | "父母" | "子女" | "配偶" => {
                (RelationRing::Family, RelationType::Emotional)
            }
            "亲戚" | "长辈" => (RelationRing::Kin, RelationType::Mixed),
            "同事" | "熟人" | "朋友" => (RelationRing::Colleague, RelationType::Mixed),
            _ => (RelationRing::Stranger, RelationType::Instrumental),
        };

        // 阶段 2 & 3：人情法则与礼貌准则策略结合
        let (rule_name, advice) = match relation_type {
            RelationType::Emotional => (
                "需求法则 (Need Rule)",
                "注重共情与情感支持，真诚关切，少讲繁文缛节与形式客套。".to_string(),
            ),
            RelationType::Mixed => {
                if input.scene.as_deref() == Some("请托")
                    || input.scene.as_deref() == Some("qingtuo")
                {
                    (
                        "人情与面子法则 (Renqing & Mianzi)",
                        "混合性关系请托拒绝需给台阶：说明客观难处，表达遗憾与关怀，适度提供替代方案，不当面生硬拒绝。".to_string(),
                    )
                } else {
                    (
                        "人情法则 (Renqing Rule)",
                        "遵循礼貌准则，措辞谦和周到，注意长幼与平级称谓尊称。".to_string(),
                    )
                }
            }
            RelationType::Instrumental => (
                "公平法则 (Equity Rule)",
                "就事论事，清晰陈述事实与规则边界，讲求效率与准确性，避免过度寒暄。".to_string(),
            ),
        };

        RelationDecision {
            ring,
            relation_type,
            applicable_rule: rule_name,
            expression_advice: advice,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_colleague_refusal_pipeline() {
        // 附录 B 示例场景：婉拒同事代班请托
        let input = RelationInput {
            role: "同事".to_string(),
            status_diff: Some("peer".to_string()),
            scene: Some("请托".to_string()),
        };

        let decision = RelationPipeline::process(&input);
        assert_eq!(decision.ring, RelationRing::Colleague);
        assert_eq!(decision.relation_type, RelationType::Mixed);
        assert_eq!(
            decision.applicable_rule,
            "人情与面子法则 (Renqing & Mianzi)"
        );
        assert!(decision.expression_advice.contains("给台阶"));
    }

    #[test]
    fn test_family_emotional_pipeline() {
        let input = RelationInput {
            role: "父母".to_string(),
            status_diff: Some("superior".to_string()),
            scene: Some("daily".to_string()),
        };

        let decision = RelationPipeline::process(&input);
        assert_eq!(decision.ring, RelationRing::Family);
        assert_eq!(decision.relation_type, RelationType::Emotional);
        assert_eq!(decision.applicable_rule, "需求法则 (Need Rule)");
    }
}
