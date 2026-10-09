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

    /// 渲染编审工作台可视化管理控制台 HTML
    pub fn render_dashboard_html(&self) -> String {
        let requests = self.requests.lock().unwrap();
        let rows_html: String = if requests.is_empty() {
            "<tr><td colspan='7' style='text-align:center; color:#94a3b8; padding: 24px;'>当前无变更提案</td></tr>".to_string()
        } else {
            requests
                .iter()
                .map(|r| {
                    let status_badge = match r.status {
                        ChangeRequestStatus::Draft => "<span style='background:#f1f5f9; color:#475569; padding:2px 8px; border-radius:4px;'>草案</span>",
                        ChangeRequestStatus::Submitted | ChangeRequestStatus::InReview => "<span style='background:#fef3c7; color:#92400e; padding:2px 8px; border-radius:4px;'>审核中</span>",
                        ChangeRequestStatus::Approved => "<span style='background:#dcfce7; color:#166534; padding:2px 8px; border-radius:4px;'>已批准</span>",
                        ChangeRequestStatus::Rejected => "<span style='background:#fee2e2; color:#991b1b; padding:2px 8px; border-radius:4px;'>已拒绝</span>",
                        ChangeRequestStatus::Deployed => "<span style='background:#dbeafe; color:#1e40af; padding:2px 8px; border-radius:4px;'>灰度发布中</span>",
                        ChangeRequestStatus::RolledBack => "<span style='background:#f3e8ff; color:#6b21a8; padding:2px 8px; border-radius:4px;'>已回滚</span>",
                    };

                    let type_label = match r.change_type {
                        ChangeType::Tightening => "收紧防护",
                        ChangeType::Relaxation => "<strong style='color:#dc2626'>放宽底线 (需双签)</strong>",
                        ChangeType::Routine => "日常微调",
                    };

                    format!(
                        r##"<tr>
                            <td><code>{}</code></td>
                            <td><strong>{}</strong></td>
                            <td><code>{}</code></td>
                            <td>{}</td>
                            <td>{}</td>
                            <td>{}%</td>
                            <td>
                                <form method='POST' action='/editor/approve' style='display:inline;'>
                                    <input type='hidden' name='id' value='{}' />
                                    <button type='submit' style='padding:4px 8px; font-size:12px; cursor:pointer;'>审批</button>
                                </form>
                                <form method='POST' action='/editor/rollback' style='display:inline; margin-left:4px;'>
                                    <input type='hidden' name='id' value='{}' />
                                    <button type='submit' style='padding:4px 8px; font-size:12px; background:#ef4444; color:white; border:none; border-radius:3px; cursor:pointer;'>回滚</button>
                                </form>
                            </td>
                        </tr>"##,
                        &r.id.to_string()[0..8],
                        r.title,
                        r.pack_id,
                        type_label,
                        status_badge,
                        r.rollout_percentage,
                        r.id,
                        r.id
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        format!(
            r##"<!DOCTYPE html>
<html lang="zh-CN">
<head>
    <meta charset="UTF-8">
    <title>记忆中枢 - 规范与规则编审工作台</title>
    <style>
        body {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif; background: #0f172a; color: #f8fafc; padding: 32px; }}
        .header {{ display: flex; justify-content: space-between; align-items: center; border-bottom: 1px solid #334155; padding-bottom: 20px; margin-bottom: 24px; }}
        h1 {{ margin: 0; font-size: 22px; color: #38bdf8; }}
        .card {{ background: #1e293b; border: 1px solid #334155; border-radius: 8px; padding: 20px; margin-bottom: 24px; }}
        table {{ width: 100%; border-collapse: collapse; text-align: left; }}
        th, td {{ padding: 12px; border-bottom: 1px solid #334155; font-size: 13px; }}
        th {{ background: #0f172a; color: #94a3b8; font-weight: 600; }}
        .btn-create {{ background: #0284c7; color: white; border: none; padding: 8px 16px; border-radius: 6px; cursor: pointer; text-decoration: none; }}
    </style>
</head>
<body>
    <div class="header">
        <div>
            <h1>记忆中枢规则编审控制台 (Rule Editor Workbench)</h1>
            <p style="margin:4px 0 0; color:#94a3b8; font-size:13px;">双人审批制衡体系 · 放宽类法务与负责人双签机制 · 核心锁防篡改防线</p>
        </div>
        <div>
            <button class="btn-create" onclick="alert('起草新规则变更')">+ 起草新规则提案</button>
        </div>
    </div>

    <div class="card">
        <h3 style="margin-top:0;">变更提案与审批流</h3>
        <table>
            <thead>
                <tr>
                    <th>编号</th>
                    <th>提案标题</th>
                    <th>目标规范包</th>
                    <th>类型</th>
                    <th>状态</th>
                    <th>灰度比例</th>
                    <th>管理操作</th>
                </tr>
            </thead>
            <tbody>
                {}
            </tbody>
        </table>
    </div>
</body>
</html>"##,
            rows_html
        )
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
