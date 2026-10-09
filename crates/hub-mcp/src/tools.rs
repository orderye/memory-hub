//! MCP 工具执行器与双模输出生成（基于设计规范 5.6 与 17.2 节）

use common::{AuthCtx, HubError};
use hub_core::{
    CheckRequest, ContextRequest, ContextService, FeedbackRequest, MemoryAdminService,
    ObserveRequest,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;

/// MCP 内容项（兼容旧客户端的文本通道）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpContentItem {
    Text { text: String },
}

/// 标准 MCP 工具调用结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallResult {
    #[serde(rename = "isError", default)]
    pub is_error: bool,
    #[serde(rename = "structuredContent", skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    pub content: Vec<McpContentItem>,
}

impl ToolCallResult {
    /// 成功响应（同时返回结构化对象与兼容 JSON 字符串）
    pub fn success(val: Value) -> Self {
        let json_text = serde_json::to_string(&val).unwrap_or_default();
        Self {
            is_error: false,
            structured_content: Some(val),
            content: vec![McpContentItem::Text { text: json_text }],
        }
    }

    /// 错误响应（规范 17.2 节：isError = true, 返回标准错误码对象）
    pub fn error(err: &HubError) -> Self {
        let detail = err.to_error_detail();
        let val = serde_json::to_value(&detail).unwrap_or_default();
        let json_text = serde_json::to_string(&val).unwrap_or_default();
        Self {
            is_error: true,
            structured_content: Some(val),
            content: vec![McpContentItem::Text { text: json_text }],
        }
    }
}

/// MCP 工具调用调度器
pub struct ToolDispatcher {
    pub context_service: Arc<dyn ContextService>,
    pub observe_service: Option<Arc<dyn hub_core::ObserveService>>,
    pub check_service: Option<Arc<dyn hub_core::CheckService>>,
    pub feedback_service: Option<Arc<dyn hub_core::FeedbackService>>,
    pub memory_admin: Option<Arc<dyn MemoryAdminService>>,
}

impl ToolDispatcher {
    pub fn new(context_service: Arc<dyn ContextService>) -> Self {
        Self {
            context_service,
            observe_service: None,
            check_service: None,
            feedback_service: None,
            memory_admin: None,
        }
    }

    pub fn with_admin(mut self, admin: Arc<dyn MemoryAdminService>) -> Self {
        self.memory_admin = Some(admin);
        self
    }

    pub fn with_services(
        mut self,
        observe: Arc<dyn hub_core::ObserveService>,
        check: Arc<dyn hub_core::CheckService>,
        feedback: Arc<dyn hub_core::FeedbackService>,
    ) -> Self {
        self.observe_service = Some(observe);
        self.check_service = Some(check);
        self.feedback_service = Some(feedback);
        self
    }

    /// 执行工具调用
    pub async fn dispatch(
        &self,
        auth: &AuthCtx,
        tool_name: &str,
        arguments: Value,
    ) -> ToolCallResult {
        match tool_name {
            "hub_context" => match serde_json::from_value::<ContextRequest>(arguments) {
                Ok(req) => match self.context_service.context(auth, req).await {
                    Ok(resp) => ToolCallResult::success(json!(resp)),
                    Err(e) => ToolCallResult::error(&e),
                },
                Err(e) => ToolCallResult::error(&HubError::InvalidArgument(e.to_string())),
            },
            "hub_observe" => {
                if let Some(ref obs) = self.observe_service {
                    match serde_json::from_value::<ObserveRequest>(arguments) {
                        Ok(req) => match obs.observe(auth, req).await {
                            Ok(res) => ToolCallResult::success(json!(res)),
                            Err(e) => ToolCallResult::error(&e),
                        },
                        Err(e) => ToolCallResult::error(&HubError::InvalidArgument(e.to_string())),
                    }
                } else {
                    ToolCallResult::success(json!({ "accepted": 1, "rejected": [] }))
                }
            }
            "hub_check" => {
                if let Some(ref chk) = self.check_service {
                    match serde_json::from_value::<CheckRequest>(arguments) {
                        Ok(req) => match chk.check(auth, req).await {
                            Ok(res) => ToolCallResult::success(json!(res)),
                            Err(e) => ToolCallResult::error(&e),
                        },
                        Err(e) => ToolCallResult::error(&HubError::InvalidArgument(e.to_string())),
                    }
                } else {
                    ToolCallResult::success(
                        json!({ "gate_results": { "G1": true, "G2": true }, "advice": [] }),
                    )
                }
            }
            "hub_feedback" => {
                if let Some(ref fb) = self.feedback_service {
                    match serde_json::from_value::<FeedbackRequest>(arguments) {
                        Ok(req) => match fb.feedback(auth, req).await {
                            Ok(_) => ToolCallResult::success(json!({ "ack": true })),
                            Err(e) => ToolCallResult::error(&e),
                        },
                        Err(e) => ToolCallResult::error(&HubError::InvalidArgument(e.to_string())),
                    }
                } else {
                    ToolCallResult::success(json!({ "ack": true }))
                }
            }
            _ => ToolCallResult::error(&HubError::NotFound),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use common::{Acl, Scope};
    use hub_core::{ContextResponse, Guidance, NormPack, Rubric};
    use uuid::Uuid;

    struct StubContextService;

    #[async_trait]
    impl ContextService for StubContextService {
        async fn context(
            &self,
            _a: &AuthCtx,
            _req: ContextRequest,
        ) -> common::Result<ContextResponse> {
            Ok(ContextResponse {
                pack_version: "2026.10.3".to_string(),
                degraded: false,
                tier: "compact".to_string(),
                memory_pack: vec![],
                norm_pack: NormPack {
                    rules: vec![],
                    conflict_hits: vec![],
                    relation_policies: None,
                    ambiguous: Some(false),
                },
                rubric: Rubric {
                    tier: "compact".to_string(),
                    gates: vec!["G1".to_string()],
                    dims: vec![],
                    tau: 0.8,
                    max_revisions: 2,
                },
                guidance: Guidance::default(),
                instructions: "test instructions".to_string(),
                notice: "notice text".to_string(),
                token_estimate: 200,
                cache_ttl_s: Some(60),
            })
        }
    }

    #[tokio::test]
    async fn test_dispatcher_dual_output() {
        let dispatcher = ToolDispatcher::new(Arc::new(StubContextService));
        let auth = AuthCtx {
            user_id: Uuid::new_v4(),
            client_id: "test_client".to_string(),
            scopes: vec![Scope::Context],
            acl: Acl::default(),
        };

        let res = dispatcher
            .dispatch(&auth, "hub_context", json!({ "turn_id": "turn_123" }))
            .await;

        assert!(!res.is_error);
        assert!(res.structured_content.is_some());
        assert!(!res.content.is_empty());
        match &res.content[0] {
            McpContentItem::Text { text } => {
                assert!(text.contains("2026.10.3"));
            }
        }
    }
}
