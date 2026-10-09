//! 助理自检量表评估服务与反馈处理实现（设计规范 7.3, 17.2 与 17.4 I 节）

use crate::infra::RuleEngine;
use crate::service::{CheckRequest, CheckResponse, CheckService, FeedbackRequest, FeedbackService};
use async_trait::async_trait;
use common::{AuthCtx, HubError, Result};
use std::collections::HashMap;
use std::sync::Arc;

/// 默认自检量表校验服务
pub struct DefaultCheckService {
    rule_engine: Arc<dyn RuleEngine>,
}

impl DefaultCheckService {
    pub fn new(rule_engine: Arc<dyn RuleEngine>) -> Self {
        Self { rule_engine }
    }
}

#[async_trait]
impl CheckService for DefaultCheckService {
    async fn check(&self, _auth: &AuthCtx, req: CheckRequest) -> Result<CheckResponse> {
        let mut gate_results = HashMap::new();
        let mut advice = Vec::new();

        // 1. 复核助理汇报的自评硬门 (G1, G2, G3)
        let g1_passed = req.self_eval.gates.get("G1").copied().unwrap_or(true);
        let g2_passed = req.self_eval.gates.get("G2").copied().unwrap_or(true);
        gate_results.insert("G1".to_string(), g1_passed);
        gate_results.insert("G2".to_string(), g2_passed);

        if !g1_passed {
            advice.push("硬门 G1 未通过：涉及合规底线或人身安全风险，必须重写！".to_string());
        }
        if !g2_passed {
            advice.push("硬门 G2 未通过：涉及不可外显的敏感记忆，必须隐去后重写！".to_string());
        }

        // 2. 确定性草稿扫描（若客户端上传了草稿）
        if let Some(ref draft) = req.draft {
            // 严禁持久化！校验完毕后立即释放内存
            let input = crate::infra::CheckInput {
                draft: Some(draft.clone()),
                roles: vec![],
                scene_tags: vec![],
            };
            let check_res = self.rule_engine.check(&input, &[]);
            if !check_res.passed {
                gate_results.insert("G1".to_string(), false);
                advice.push("确定性草稿扫描命中禁用违规特征，强制要求重写".to_string());
            }
        }

        Ok(CheckResponse {
            gate_results,
            advice,
        })
    }
}

/// 默认用户显式反馈处理服务
#[derive(Default)]
pub struct DefaultFeedbackService {
    audit_sink: Option<Arc<dyn crate::infra::AuditSink>>,
}

impl DefaultFeedbackService {
    pub fn new() -> Self {
        Self { audit_sink: None }
    }

    pub fn with_audit(mut self, audit_sink: Arc<dyn crate::infra::AuditSink>) -> Self {
        self.audit_sink = Some(audit_sink);
        self
    }
}

#[async_trait]
impl FeedbackService for DefaultFeedbackService {
    async fn feedback(&self, auth: &AuthCtx, req: FeedbackRequest) -> Result<()> {
        let valid_types = [
            "adopt",
            "reject",
            "edit",
            "regenerate",
            "complaint",
            "uncovered_conflict",
        ];

        if !valid_types.contains(&req.r#type.as_str()) {
            return Err(HubError::InvalidArgument(format!(
                "Invalid feedback type: {}",
                req.r#type
            )));
        }

        // 记录脱敏审计事件
        if let Some(ref sink) = self.audit_sink {
            sink.record(crate::infra::AuditEvent {
                actor: auth.client_id.clone(),
                action: format!("feedback:{}", req.r#type),
                target: req.target,
                detail: serde_json::json!({
                    "turn_id": req.turn_id,
                    "user_id": auth.user_id,
                }),
                created_at: chrono::Utc::now(),
            });
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::{Acl, Scope};
    use uuid::Uuid;

    struct MockRuleEngine;
    impl crate::infra::RuleEngine for MockRuleEngine {
        fn check(
            &self,
            _input: &crate::infra::CheckInput,
            _rules: &[crate::infra::Rule],
        ) -> crate::infra::CheckResult {
            crate::infra::CheckResult {
                passed: true,
                violations: vec![],
            }
        }
        fn resolve(
            &self,
            _conflicts: &[crate::infra::ConflictFlag],
            _table: &crate::infra::ConflictTable,
        ) -> crate::infra::Resolution {
            crate::infra::Resolution {
                strategy: "direct".to_string(),
                advice: None,
            }
        }
    }

    #[tokio::test]
    async fn test_check_service_gates() {
        let rule_engine = Arc::new(MockRuleEngine);
        let check_svc = DefaultCheckService::new(rule_engine);
        let auth = AuthCtx {
            user_id: Uuid::new_v4(),
            client_id: "asst".to_string(),
            scopes: vec![Scope::Write],
            acl: Acl::default(),
        };

        let mut gates = HashMap::new();
        gates.insert("G1".to_string(), true);
        gates.insert("G2".to_string(), false); // 模拟自评未通过 G2

        let req = CheckRequest {
            turn_id: "turn_1".to_string(),
            self_eval: crate::service::SelfEval {
                gates,
                scores: HashMap::new(),
                revisions: Some(1),
            },
            rule_hits: vec![],
            draft: None,
        };

        let resp = check_svc.check(&auth, req).await.unwrap();
        assert_eq!(resp.gate_results.get("G1"), Some(&true));
        assert_eq!(resp.gate_results.get("G2"), Some(&false));
        assert!(!resp.advice.is_empty());
    }

    #[tokio::test]
    async fn test_feedback_service_valid_and_invalid() {
        let fb_svc = DefaultFeedbackService::new();
        let auth = AuthCtx {
            user_id: Uuid::new_v4(),
            client_id: "asst".to_string(),
            scopes: vec![Scope::Write],
            acl: Acl::default(),
        };

        let req_valid = FeedbackRequest {
            turn_id: "turn_1".to_string(),
            r#type: "adopt".to_string(),
            target: None,
            detail: None,
        };
        assert!(fb_svc.feedback(&auth, req_valid).await.is_ok());

        let req_invalid = FeedbackRequest {
            turn_id: "turn_1".to_string(),
            r#type: "illegal_type".to_string(),
            target: None,
            detail: None,
        };
        assert!(fb_svc.feedback(&auth, req_invalid).await.is_err());
    }
}
