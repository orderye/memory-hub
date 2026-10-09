//! 多助理记忆融合与来源撤销证据剥离引擎（设计规范 10, 11.1 与 18.2 WP-21 节）

use crate::domain::{MemoryItem, Provenance};
use serde::{Deserialize, Serialize};

/// 记忆条目支撑证据项
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEvidenceItem {
    pub evidence_id: String,
    pub source_client: String,
    pub observed_at: chrono::DateTime<chrono::Utc>,
    pub payload: serde_json::Value,
}

/// 多助理融合与证据管理引擎
pub struct MemoryFusionEngine;

impl MemoryFusionEngine {
    /// 依据消解规则 C6 对冲突记忆进行仲裁与融合
    ///
    /// 优先级规则：stated > observed > inferred；同级近期优先；低置信不用
    pub fn arbitrate_c6<'a>(candidates: &'a [&'a MemoryItem]) -> Option<&'a MemoryItem> {
        if candidates.is_empty() {
            return None;
        }

        let prov_rank = |p: Provenance| match p {
            Provenance::Stated => 3,
            Provenance::Observed => 2,
            Provenance::Inferred => 1,
        };

        let mut sorted = candidates.to_vec();
        sorted.sort_by(|a, b| {
            let rank_a = prov_rank(a.provenance);
            let rank_b = prov_rank(b.provenance);
            if rank_a != rank_b {
                rank_b.cmp(&rank_a)
            } else {
                // 同级按时间近期优先
                b.updated_at.cmp(&a.updated_at)
            }
        });

        sorted.first().copied()
    }

    /// 解绑单家助理时的证据剥离与条目动态重算（设计规范 11.1 与 17.4 H 节）
    ///
    /// - 剥离属于 revoked_client 的证据
    /// - 若该条目仅由该助理独家提供且非 Stated，返回 None（指示删除该条目）
    /// - 若仍有其他助理的证据留存，返回更新后的剩余证据
    pub fn prune_evidence_on_unbind(
        evidences: &[MemoryEvidenceItem],
        revoked_client: &str,
        provenance: Provenance,
    ) -> Option<Vec<MemoryEvidenceItem>> {
        let remaining: Vec<MemoryEvidenceItem> = evidences
            .iter()
            .filter(|e| e.source_client != revoked_client)
            .cloned()
            .collect();

        if remaining.is_empty() {
            if provenance == Provenance::Stated {
                // 用户明说条目即使解绑特定录入客户端，仍作为用户自身事实保留
                Some(remaining)
            } else {
                // 仅靠该助理观察或推断派生的特征，彻底清除
                None
            }
        } else {
            Some(remaining)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ItemStatus, Layer, Sensitivity, UsePolicy};
    use chrono::{Duration, Utc};
    use uuid::Uuid;

    #[test]
    fn test_c6_arbitration_priority() {
        let now = Utc::now();
        let user_id = Uuid::new_v4();

        // 助理 A 明说（权重最高）
        let item_stated = MemoryItem {
            id: Uuid::new_v4(),
            user_id,
            layer: Layer::Preference,
            text: "偏好简短直接".to_string(),
            provenance: Provenance::Stated,
            confidence: 0.9,
            sensitivity: Sensitivity::Normal,
            use_policy: UsePolicy::Free,
            source_client: "asst_a".to_string(),
            status: ItemStatus::Active,
            ttl: None,
            attrs: serde_json::json!({}),
            created_at: now - Duration::days(5),
            updated_at: now - Duration::days(5),
        };

        // 助理 B 较近时间的推断（虽然时间近，但 Stated 优先）
        let item_inferred = MemoryItem {
            id: Uuid::new_v4(),
            user_id,
            layer: Layer::Preference,
            text: "偏好详细解释".to_string(),
            provenance: Provenance::Inferred,
            confidence: 0.8,
            sensitivity: Sensitivity::Normal,
            use_policy: UsePolicy::Free,
            source_client: "asst_b".to_string(),
            status: ItemStatus::Active,
            ttl: None,
            attrs: serde_json::json!({}),
            created_at: now,
            updated_at: now,
        };

        let slice = [&item_stated, &item_inferred];
        let winner = MemoryFusionEngine::arbitrate_c6(&slice).unwrap();
        assert_eq!(winner.text, "偏好简短直接");
        assert_eq!(winner.provenance, Provenance::Stated);
    }

    #[test]
    fn test_prune_evidence_on_unbind() {
        let ev_a = MemoryEvidenceItem {
            evidence_id: "e1".to_string(),
            source_client: "asst_a".to_string(),
            observed_at: Utc::now(),
            payload: serde_json::json!({}),
        };
        let ev_b = MemoryEvidenceItem {
            evidence_id: "e2".to_string(),
            source_client: "asst_b".to_string(),
            observed_at: Utc::now(),
            payload: serde_json::json!({}),
        };

        // 包含 A 和 B 证据的推断条目，解绑 A -> 依然保留 B 的证据
        let res_multi = MemoryFusionEngine::prune_evidence_on_unbind(
            &[ev_a.clone(), ev_b],
            "asst_a",
            Provenance::Inferred,
        );
        assert!(res_multi.is_some());
        assert_eq!(res_multi.unwrap().len(), 1);

        // 仅包含 A 证据的推断条目，解绑 A -> 返回 None（级联清除条目）
        let res_sole =
            MemoryFusionEngine::prune_evidence_on_unbind(&[ev_a], "asst_a", Provenance::Inferred);
        assert!(res_sole.is_none());
    }
}
