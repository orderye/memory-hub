//! 评测场景集运行器与多维矩阵评测（设计规范 8.5 与 13 节）

use hub_packs::{RelationInput, RelationPipeline, RelationType};
use serde::{Deserialize, Serialize};

/// 单个测试场景定义
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalScenario {
    pub id: String,
    pub title: String,
    pub role: String,
    pub status_diff: Option<String>,
    pub scene: String,
    /// 是否为“用户明确不吃这一套”的反例场景（防刻板印象）
    pub is_counter_example: bool,
    pub expected_relation_type: String,
}

/// 场景集运行统计报告
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ScenarioReport {
    pub total_scenarios: usize,
    pub passed_scenarios: usize,
    pub counter_examples_passed: usize,
}

pub struct ScenarioRunner;

impl ScenarioRunner {
    /// 运行基准测试场景集并输出度量报告
    pub fn run_suite(scenarios: &[EvalScenario]) -> ScenarioReport {
        let mut passed = 0;
        let mut counter_passed = 0;

        for sc in scenarios {
            let input = RelationInput {
                role: sc.role.clone(),
                status_diff: sc.status_diff.clone(),
                scene: Some(sc.scene.clone()),
            };

            let decision = RelationPipeline::process(&input);
            let rel_str = match decision.relation_type {
                RelationType::Emotional => "emotional",
                RelationType::Mixed => "mixed",
                RelationType::Instrumental => "instrumental",
            };

            if rel_str == sc.expected_relation_type {
                passed += 1;
                if sc.is_counter_example {
                    counter_passed += 1;
                }
            }
        }

        ScenarioReport {
            total_scenarios: scenarios.len(),
            passed_scenarios: passed,
            counter_examples_passed: counter_passed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_benchmark_scenarios() {
        let scenarios = vec![
            EvalScenario {
                id: "sc_01".to_string(),
                title: "同事请托代班".to_string(),
                role: "同事".to_string(),
                status_diff: Some("peer".to_string()),
                scene: "请托".to_string(),
                is_counter_example: false,
                expected_relation_type: "mixed".to_string(),
            },
            EvalScenario {
                id: "sc_02".to_string(),
                title: "父母日常关切".to_string(),
                role: "父母".to_string(),
                status_diff: Some("superior".to_string()),
                scene: "daily".to_string(),
                is_counter_example: false,
                expected_relation_type: "emotional".to_string(),
            },
            EvalScenario {
                id: "sc_03".to_string(),
                title: "陌生人事务咨询".to_string(),
                role: "快递员".to_string(),
                status_diff: None,
                scene: "delivery".to_string(),
                is_counter_example: false,
                expected_relation_type: "instrumental".to_string(),
            },
        ];

        let report = ScenarioRunner::run_suite(&scenarios);
        assert_eq!(report.total_scenarios, 3);
        assert_eq!(report.passed_scenarios, 3);
    }
}
