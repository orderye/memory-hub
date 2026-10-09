//! 未覆盖冲突案例聚类与规则候选生成（设计规范 10 与 18.2 WP-17 节）

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 原始未覆盖冲突日志事件
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawConflictEvent {
    pub scene_tag: String,
    pub user_intent: String,
    pub detail: String,
}

/// 规则候选条目提案（供进入编审工作台审批池）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateRuleProposal {
    pub candidate_id: String,
    pub scene: String,
    pub frequency: usize,
    pub rationale: String,
}

/// 冲突案例聚类分析器
pub struct ConflictClusterer;

impl ConflictClusterer {
    /// 对冲突事件按场景标签执行聚合，生成规则候选
    pub fn cluster(
        events: &[RawConflictEvent],
        min_frequency: usize,
    ) -> Vec<CandidateRuleProposal> {
        let mut grouped: HashMap<String, Vec<&RawConflictEvent>> = HashMap::new();

        for event in events {
            grouped
                .entry(event.scene_tag.clone())
                .or_default()
                .push(event);
        }

        let mut proposals = Vec::new();
        for (scene, group) in grouped {
            if group.len() >= min_frequency {
                let proposal = CandidateRuleProposal {
                    candidate_id: format!("cand_{}", scene),
                    scene: scene.clone(),
                    frequency: group.len(),
                    rationale: format!("该场景出现 {} 次未覆盖冲突，建议制定规范条目", group.len()),
                };
                proposals.push(proposal);
            }
        }

        proposals
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_conflict_clustering() {
        let events = vec![
            RawConflictEvent {
                scene_tag: "wedding_gift".to_string(),
                user_intent: "拒绝随礼".to_string(),
                detail: "同事婚礼随礼争议".to_string(),
            },
            RawConflictEvent {
                scene_tag: "wedding_gift".to_string(),
                user_intent: "礼金数额".to_string(),
                detail: "同事结婚礼金数额".to_string(),
            },
            RawConflictEvent {
                scene_tag: "rare_case".to_string(),
                user_intent: "生僻情况".to_string(),
                detail: "单次个例".to_string(),
            },
        ];

        // 设定聚类阈值 >= 2 次
        let proposals = ConflictClusterer::cluster(&events, 2);
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].scene, "wedding_gift");
        assert_eq!(proposals[0].frequency, 2);
    }
}
