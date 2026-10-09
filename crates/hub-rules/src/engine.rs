//! 确定性规则引擎与冲突消解器（基于设计规范 8.4a 与 17.4 F 节）

use hub_core::{
    CheckInput, CheckResult, ConflictFlag, ConflictTable, Resolution, Rule, RuleEngine,
};

/// 默认确定性规则引擎
#[derive(Debug, Default, Clone)]
pub struct DefaultRuleEngine;

impl DefaultRuleEngine {
    pub fn new() -> Self {
        Self
    }
}

impl RuleEngine for DefaultRuleEngine {
    fn check(&self, input: &CheckInput, rules: &[Rule]) -> CheckResult {
        let mut violations = Vec::new();
        let mut passed = true;

        if let Some(ref draft) = input.draft {
            for rule in rules {
                // 确定性关键词 / 模式违规检查示例
                if rule.when_expr.starts_with("contains:") {
                    let needle = &rule.when_expr["contains:".len()..];
                    if draft.contains(needle) {
                        passed = false;
                        violations.push(rule.id.clone());
                    }
                }
            }
        }

        CheckResult { passed, violations }
    }

    fn resolve(&self, conflicts: &[ConflictFlag], _table: &ConflictTable) -> Resolution {
        if conflicts.is_empty() {
            return Resolution {
                strategy: "direct".to_string(),
                advice: None,
            };
        }

        // 依 C1 到 C6 优先级判定
        if conflicts.contains(&ConflictFlag::C1) {
            return Resolution {
                strategy: "bottom_line_priority".to_string(),
                advice: Some(
                    "用户偏好与法律伦理底线冲突：底线优先，温和表达并给替代方案".to_string(),
                ),
            };
        }
        if conflicts.contains(&ConflictFlag::C2) {
            return Resolution {
                strategy: "user_preference_priority".to_string(),
                advice: Some(
                    "用户偏好与传统礼俗冲突：用户自主优先，最低限度保留礼貌，不说教".to_string(),
                ),
            };
        }
        if conflicts.contains(&ConflictFlag::C3) {
            return Resolution {
                strategy: "weighted_context".to_string(),
                advice: Some(
                    "传统规范内部冲突：按场景与关系圈层加权，无法判定时给两面由用户自选"
                        .to_string(),
                ),
            };
        }
        if conflicts.contains(&ConflictFlag::C4) {
            return Resolution {
                strategy: "personal_narrative_priority".to_string(),
                advice: Some(
                    "集体叙事与个人亲历冲突：个人叙事优先，不泛化，事实层不改".to_string(),
                ),
            };
        }
        if conflicts.contains(&ConflictFlag::C5) {
            return Resolution {
                strategy: "honesty_with_tact".to_string(),
                advice: Some(
                    "保面子与诚实冲突：事实不可编造，采用委婉措辞，不用虚假陈述".to_string(),
                ),
            };
        }
        if conflicts.contains(&ConflictFlag::C6) {
            return Resolution {
                strategy: "provenance_cascade".to_string(),
                advice: Some(
                    "多来源记忆冲突：stated > observed > inferred，近期优先，低置信不用"
                        .to_string(),
                ),
            };
        }

        // 未在表中的未知冲突：下发保守策略 (hedge)
        Resolution {
            strategy: "hedge".to_string(),
            advice: Some("未覆盖冲突类型：下发保守委婉策略，并记录未覆盖事件".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_conflict_resolution_c1_and_c2() {
        let engine = DefaultRuleEngine::new();
        let table = ConflictTable {
            version: 1,
            entries: serde_json::json!([]),
        };

        // C1 冲突应该最高优先级底线优先
        let res_c1 = engine.resolve(&[ConflictFlag::C1, ConflictFlag::C2], &table);
        assert_eq!(res_c1.strategy, "bottom_line_priority");

        // 单独 C2 冲突应该用户偏好优先
        let res_c2 = engine.resolve(&[ConflictFlag::C2], &table);
        assert_eq!(res_c2.strategy, "user_preference_priority");
    }

    #[test]
    fn test_rule_check_draft() {
        let engine = DefaultRuleEngine::new();
        let rules = vec![Rule {
            id: "r_no_swear".to_string(),
            level: "hard".to_string(),
            when_expr: "contains:违规词".to_string(),
            then_action: "reject".to_string(),
            core: true,
        }];

        let input_clean = CheckInput {
            draft: Some("这是一段正常文明的对话草稿".to_string()),
            roles: vec![],
            scene_tags: vec![],
        };
        assert!(engine.check(&input_clean, &rules).passed);

        let input_violated = CheckInput {
            draft: Some("这里包含了违规词内容".to_string()),
            roles: vec![],
            scene_tags: vec![],
        };
        let check_res = engine.check(&input_violated, &rules);
        assert!(!check_res.passed);
        assert_eq!(check_res.violations, vec!["r_no_swear"]);
    }
}
