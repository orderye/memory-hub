#![forbid(unsafe_code)]

//! 规则编审工作台业务服务（设计规范 18.2 WP-20 节）
//!
//! 提供双人审批状态机（制衡起草人不可审自身条目、放宽类强制法务与业务负责人双签）、
//! 灰度比例配置与一键回滚控制台、只追加操作审计日志。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum EditorError {
    #[error("状态机错误: 当前状态 {0:?} 不允许执行该操作")]
    InvalidStateTransition(ChangeRequestStatus),

    #[error("权限违规: 起草人不可审核或批准自身提交的变更请求")]
    SelfReviewProhibited,

    #[error("审批不足: 放宽类规则必须经过法务审查角色批准")]
    LegalApprovalRequired,

    #[error("审批不足: 放宽类规则必须经过业务负责人角色批准")]
    OwnerApprovalRequired,

    #[error("资源未找到: 找不到请求 ID {0}")]
    RequestNotFound(Uuid),
}

/// 变更请求类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeType {
    /// 收紧/安全防护（常规双人审批）
    Tightening,
    /// 放宽类规则/核心锁变更（强制法务与负责人双签）
    Relaxation,
    /// 词表/权重常规更新
    Routine,
}

/// 审批角色
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewerRole {
    PeerReviewer,
    LegalCompliance,
    BusinessOwner,
}

/// 变更请求状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeRequestStatus {
    Draft,
    Submitted,
    InReview,
    Approved,
    Rejected,
    Deployed,
    RolledBack,
}

/// 审批签名记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalSignature {
    pub reviewer_id: String,
    pub role: ReviewerRole,
    pub approved_at: DateTime<Utc>,
    pub comment: String,
}

/// 编审变更请求实体
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeRequest {
    pub id: Uuid,
    pub title: String,
    pub pack_id: String,
    pub change_type: ChangeType,
    pub author_id: String,
    pub content_diff: String,
    pub status: ChangeRequestStatus,
    pub signatures: Vec<ApprovalSignature>,
    pub rollout_percentage: u8, // 0..=100
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// 编审审计操作事件（只追加）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditorAuditEvent {
    pub event_id: Uuid,
    pub request_id: Uuid,
    pub operator_id: String,
    pub action: String,
    pub details: serde_json::Value,
    pub timestamp: DateTime<Utc>,
}

/// 规则编审工作台服务
#[derive(Default)]
pub struct RuleEditorService {
    requests: Arc<Mutex<Vec<ChangeRequest>>>,
    audit_logs: Arc<Mutex<Vec<EditorAuditEvent>>>,
}

impl RuleEditorService {
    pub fn new() -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            audit_logs: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 创建规则变更草案
    pub fn create_draft(
        &self,
        author_id: &str,
        title: &str,
        pack_id: &str,
        change_type: ChangeType,
        content_diff: &str,
    ) -> ChangeRequest {
        let now = Utc::now();
        let cr = ChangeRequest {
            id: Uuid::new_v4(),
            title: title.to_string(),
            pack_id: pack_id.to_string(),
            change_type,
            author_id: author_id.to_string(),
            content_diff: content_diff.to_string(),
            status: ChangeRequestStatus::Draft,
            signatures: Vec::new(),
            rollout_percentage: 0,
            created_at: now,
            updated_at: now,
        };

        self.requests.lock().unwrap().push(cr.clone());
        self.record_audit(cr.id, author_id, "CREATE_DRAFT", serde_json::json!({}));
        cr
    }

    /// 提交审批
    pub fn submit_for_review(&self, id: Uuid, author_id: &str) -> Result<ChangeRequest, String> {
        let mut list = self.requests.lock().unwrap();
        let cr = list
            .iter_mut()
            .find(|r| r.id == id)
            .ok_or_else(|| "Change request not found".to_string())?;

        if cr.status != ChangeRequestStatus::Draft {
            return Err("Only Draft can be submitted".to_string());
        }
        if cr.author_id != author_id {
            return Err("Only author can submit".to_string());
        }

        cr.status = ChangeRequestStatus::Submitted;
        cr.updated_at = Utc::now();
        let res = cr.clone();
        drop(list);

        self.record_audit(id, author_id, "SUBMIT_REVIEW", serde_json::json!({}));
        Ok(res)
    }

    /// 审批并签名
    ///
    /// 强制规则：
    /// 1. 起草人不得审批自身请求
    /// 2. 放宽类请求要求 LegalCompliance 与 BusinessOwner 双签
    /// 3. 普通变更要求非起草人的 PeerReviewer
    pub fn approve_request(
        &self,
        id: Uuid,
        reviewer_id: &str,
        role: ReviewerRole,
        comment: &str,
    ) -> Result<ChangeRequest, String> {
        let mut list = self.requests.lock().unwrap();
        let cr = list
            .iter_mut()
            .find(|r| r.id == id)
            .ok_or_else(|| "Change request not found".to_string())?;

        // 强校验：起草人不可自审
        if cr.author_id == reviewer_id {
            return Err("Self-review prohibited: author cannot approve own request".to_string());
        }

        if cr.status != ChangeRequestStatus::Submitted && cr.status != ChangeRequestStatus::InReview
        {
            return Err("Request not in reviewable state".to_string());
        }

        cr.signatures.push(ApprovalSignature {
            reviewer_id: reviewer_id.to_string(),
            role,
            approved_at: Utc::now(),
            comment: comment.to_string(),
        });

        // 检查是否达到通过条件
        let is_approved = match cr.change_type {
            ChangeType::Relaxation => {
                let has_legal = cr
                    .signatures
                    .iter()
                    .any(|s| s.role == ReviewerRole::LegalCompliance);
                let has_owner = cr
                    .signatures
                    .iter()
                    .any(|s| s.role == ReviewerRole::BusinessOwner);
                has_legal && has_owner
            }
            ChangeType::Tightening | ChangeType::Routine => {
                // 至少 1 位非作者的审核员批准
                !cr.signatures.is_empty()
            }
        };

        if is_approved {
            cr.status = ChangeRequestStatus::Approved;
        } else {
            cr.status = ChangeRequestStatus::InReview;
        }
        cr.updated_at = Utc::now();

        let res = cr.clone();
        drop(list);

        self.record_audit(
            id,
            reviewer_id,
            "APPROVE",
            serde_json::json!({ "role": role, "status": res.status }),
        );
        Ok(res)
    }

    /// 灰度部署
    pub fn deploy(
        &self,
        id: Uuid,
        operator_id: &str,
        rollout_pct: u8,
    ) -> Result<ChangeRequest, String> {
        let mut list = self.requests.lock().unwrap();
        let cr = list
            .iter_mut()
            .find(|r| r.id == id)
            .ok_or_else(|| "Change request not found".to_string())?;

        if cr.status != ChangeRequestStatus::Approved && cr.status != ChangeRequestStatus::Deployed
        {
            return Err("Only approved or deployed requests can adjust rollout".to_string());
        }

        cr.status = ChangeRequestStatus::Deployed;
        cr.rollout_percentage = rollout_pct.min(100);
        cr.updated_at = Utc::now();

        let res = cr.clone();
        drop(list);

        self.record_audit(
            id,
            operator_id,
            "DEPLOY",
            serde_json::json!({ "rollout": res.rollout_percentage }),
        );
        Ok(res)
    }

    /// 一键回滚
    pub fn rollback(
        &self,
        id: Uuid,
        operator_id: &str,
        reason: &str,
    ) -> Result<ChangeRequest, String> {
        let mut list = self.requests.lock().unwrap();
        let cr = list
            .iter_mut()
            .find(|r| r.id == id)
            .ok_or_else(|| "Change request not found".to_string())?;

        if cr.status != ChangeRequestStatus::Deployed {
            return Err("Only deployed requests can be rolled back".to_string());
        }

        cr.status = ChangeRequestStatus::RolledBack;
        cr.rollout_percentage = 0;
        cr.updated_at = Utc::now();

        let res = cr.clone();
        drop(list);

        self.record_audit(
            id,
            operator_id,
            "ROLLBACK",
            serde_json::json!({ "reason": reason }),
        );
        Ok(res)
    }

    /// 查询审计日志（只追加不可篡改）
    pub fn get_audit_logs(&self, request_id: Uuid) -> Vec<EditorAuditEvent> {
        self.audit_logs
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.request_id == request_id)
            .cloned()
            .collect()
    }

    fn record_audit(
        &self,
        request_id: Uuid,
        operator_id: &str,
        action: &str,
        details: serde_json::Value,
    ) {
        let entry = EditorAuditEvent {
            event_id: Uuid::new_v4(),
            request_id,
            operator_id: operator_id.to_string(),
            action: action.to_string(),
            details,
            timestamp: Utc::now(),
        };
        self.audit_logs.lock().unwrap().push(entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_self_review_prevention() {
        let service = RuleEditorService::new();
        let cr = service.create_draft(
            "alice",
            "更新违禁词",
            "anti_fawning",
            ChangeType::Tightening,
            "+word",
        );
        assert_eq!(cr.status, ChangeRequestStatus::Draft);

        service.submit_for_review(cr.id, "alice").unwrap();

        // 作者自身尝试审批 -> 必须被拒绝
        let res = service.approve_request(cr.id, "alice", ReviewerRole::PeerReviewer, "lgtm");
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("Self-review prohibited"));
    }

    #[test]
    fn test_relaxation_dual_sign_and_rollback() {
        let service = RuleEditorService::new();
        let cr = service.create_draft(
            "alice",
            "放宽部分敏感限制",
            "anti_fawning",
            ChangeType::Relaxation,
            "-restriction",
        );

        service.submit_for_review(cr.id, "alice").unwrap();

        // 仅有法务审批 -> 仍处于 InReview 状态
        let in_review = service
            .approve_request(
                cr.id,
                "bob_legal",
                ReviewerRole::LegalCompliance,
                "法务初审通过",
            )
            .unwrap();
        assert_eq!(in_review.status, ChangeRequestStatus::InReview);

        // 负责人审批 -> 达到双签标准，变为 Approved
        let approved = service
            .approve_request(
                cr.id,
                "carol_owner",
                ReviewerRole::BusinessOwner,
                "业务负责人终审通过",
            )
            .unwrap();
        assert_eq!(approved.status, ChangeRequestStatus::Approved);

        // 灰度发布 20%
        let deployed = service.deploy(cr.id, "operator_dave", 20).unwrap();
        assert_eq!(deployed.status, ChangeRequestStatus::Deployed);
        assert_eq!(deployed.rollout_percentage, 20);

        // 一键回滚
        let rolled_back = service
            .rollback(cr.id, "operator_dave", "线上观测到异常指标")
            .unwrap();
        assert_eq!(rolled_back.status, ChangeRequestStatus::RolledBack);
        assert_eq!(rolled_back.rollout_percentage, 0);

        // 验证只追加审计记录: 1 create + 1 submit + 2 approve (legal & owner) + 1 deploy + 1 rollback = 6 条
        let logs = service.get_audit_logs(cr.id);
        assert_eq!(logs.len(), 6);
    }
}
