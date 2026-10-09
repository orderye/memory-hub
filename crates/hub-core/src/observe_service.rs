//! 行为回流与循环一致性验证服务（设计规范 4, 7.1, 17.2 与 18.2 WP-16 节）

use crate::algorithm::update_confidence_on_event;
use crate::algorithm::ConfidenceEvent;
use crate::domain::ItemStatus;
use crate::service::{ObserveRequest, ObserveResponse, ObserveService, RejectedItem};
use async_trait::async_trait;
use common::{AuthCtx, Result};
use std::collections::HashMap;
use std::sync::RwLock;

/// 存储的假设条目状态
#[derive(Debug, Clone)]
pub struct StoredHypothesis {
    pub kind: String,
    pub content: String,
    pub confidence: f32,
    pub predicted_reaction: Option<String>,
    pub status: String, // "open", "matched", "missed"
}

/// 默认行为回流与循环一致性服务
pub struct DefaultObserveService {
    // 按照 (user_id, turn_id) 索引存储假设记录
    hypotheses: RwLock<HashMap<(common::UserId, String), Vec<StoredHypothesis>>>,
}

impl Default for DefaultObserveService {
    fn default() -> Self {
        Self::new()
    }
}

impl DefaultObserveService {
    pub fn new() -> Self {
        Self {
            hypotheses: RwLock::new(HashMap::new()),
        }
    }

    /// 获取特定轮次的假设状态（供单元测试断言）
    pub fn get_hypotheses(&self, user_id: common::UserId, turn_id: &str) -> Vec<StoredHypothesis> {
        let read = self.hypotheses.read().unwrap();
        read.get(&(user_id, turn_id.to_string()))
            .cloned()
            .unwrap_or_default()
    }
}

#[async_trait]
impl ObserveService for DefaultObserveService {
    async fn observe(&self, auth: &AuthCtx, req: ObserveRequest) -> Result<ObserveResponse> {
        let mut write = self.hypotheses.write().unwrap();
        let mut accepted = 0;
        let mut rejected = Vec::new();

        // 1. 若有事件回流，检查上一轮假设并执行循环一致性比对
        if !req.events.is_empty() {
            for event in &req.events {
                for hypotheses_list in write.values_mut() {
                    for hyp in hypotheses_list.iter_mut() {
                        if hyp.status == "open" {
                            if let Some(ref pred) = hyp.predicted_reaction {
                                if pred == &event.r#type {
                                    // 预测吻合：升权 c <- c + 0.2*(1 - c)
                                    let res = update_confidence_on_event(
                                        hyp.confidence,
                                        ItemStatus::Active,
                                        ConfidenceEvent::PredictionHit,
                                    );
                                    hyp.confidence = res.confidence;
                                    hyp.status = "matched".to_string();
                                } else {
                                    // 预测未命中：降权 c <- 0.7 * c
                                    let res = update_confidence_on_event(
                                        hyp.confidence,
                                        ItemStatus::Active,
                                        ConfidenceEvent::PredictionMiss,
                                    );
                                    hyp.confidence = res.confidence;
                                    hyp.status = "missed".to_string();
                                }
                            }
                        }
                    }
                }
            }
        }

        // 2. 接收并暂存本轮新的假设
        let mut new_list = Vec::new();
        let pred_kind = req.predicted_reaction.as_ref().map(|p| p.kind.clone());

        for (idx, hyp_in) in req.hypotheses.into_iter().enumerate() {
            // 校验置信度范围与内容合法性
            if hyp_in.confidence < 0.0 || hyp_in.confidence > 1.0 {
                rejected.push(RejectedItem {
                    index: idx,
                    reason: "Confidence must be within 0.0 to 1.0".to_string(),
                });
                continue;
            }

            new_list.push(StoredHypothesis {
                kind: hyp_in.kind,
                content: hyp_in.content,
                confidence: hyp_in.confidence,
                predicted_reaction: pred_kind.clone(),
                status: "open".to_string(),
            });
            accepted += 1;
        }

        if !new_list.is_empty() {
            write.insert((auth.user_id, req.turn_id), new_list);
        }

        Ok(ObserveResponse { accepted, rejected })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::ObserveEventInput;
    use common::{Acl, Scope};
    use uuid::Uuid;

    #[tokio::test]
    async fn test_circular_consistency_hit_and_miss() {
        let observe_svc = DefaultObserveService::new();
        let user_id = Uuid::new_v4();
        let auth = AuthCtx {
            user_id,
            client_id: "test_client".to_string(),
            scopes: vec![Scope::Write],
            acl: Acl::default(),
        };

        // 第 1 轮：提交假设，预测用户下一步会 adopt (采纳)
        let req1 = ObserveRequest {
            conversation_id: Some("c1".to_string()),
            turn_id: "t1".to_string(),
            hypotheses: vec![crate::service::HypothesisInput {
                kind: "intent".to_string(),
                content: "用户偏好直接指令".to_string(),
                confidence: 0.5,
                evidence: vec![],
            }],
            predicted_reaction: Some(crate::service::PredictedReaction {
                kind: "adopt".to_string(),
                confidence: 0.8,
            }),
            events: vec![],
        };

        let res1 = observe_svc.observe(&auth, req1).await.unwrap();
        assert_eq!(res1.accepted, 1);

        // 第 2 轮：用户实际行为到达，实际事件是 adopt -> 命中循环一致性！
        let req2 = ObserveRequest {
            conversation_id: Some("c1".to_string()),
            turn_id: "t2".to_string(),
            hypotheses: vec![],
            predicted_reaction: None,
            events: vec![ObserveEventInput {
                r#type: "adopt".to_string(),
                ts: None,
            }],
        };

        observe_svc.observe(&auth, req2).await.unwrap();

        // 验证 t1 假设的状态流转为 matched，置信度上升 0.5 -> 0.6
        let stored = observe_svc.get_hypotheses(user_id, "t1");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].status, "matched");
        assert!((stored[0].confidence - 0.6).abs() < 1e-4);
    }
}
