//! 统一错误类型与错误码映射
//!
//! 工具错误通过 `ToolCallResult.isError` 在 MCP 200 响应内表达；
//! 传输层错误（认证 / 限流等）通过 JSON-RPC 错误码 + HTTP 状态码表达。

use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, HubError>;

/// 多智能体长期记忆服务统一错误定义
#[derive(thiserror::Error, Debug)]
pub enum HubError {
    #[error("参数不合法: {0}")]
    InvalidArgument(String),

    #[error("对象不存在")]
    NotFound,

    #[error("版本冲突")]
    Conflict,

    #[error("未提供有效认证凭据")]
    AuthRequired,

    #[error("权限不足")]
    InsufficientScope,

    #[error("触发限流")]
    RateLimited { retry_after_s: Option<u64> },

    #[error("需要二次确认")]
    ConfirmRequired { confirm_token: String },

    #[error("确认令牌过期或已失效")]
    ConfirmExpired,

    #[error("方法不存在: {0}")]
    MethodNotFound(String),

    #[error("服务暂不可用")]
    Unavailable,

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// 对外标准错误描述对象（用于 structuredContent 响应）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorDetail {
    pub code: String,
    pub message_zh: String,
    pub message_en: String,
    pub retryable: bool,
}

impl HubError {
    /// 转换为系统标准错误码
    pub fn error_code(&self) -> &'static str {
        match self {
            HubError::AuthRequired => "E_AUTH_REQUIRED",
            HubError::InsufficientScope => "E_INSUFFICIENT_SCOPE",
            HubError::InvalidArgument(_) => "E_INVALID_ARGUMENT",
            HubError::NotFound => "E_NOT_FOUND",
            HubError::Conflict => "E_CONFLICT",
            HubError::ConfirmRequired { .. } => "E_CONFIRM_REQUIRED",
            HubError::ConfirmExpired => "E_CONFIRM_EXPIRED",
            HubError::RateLimited { .. } => "E_RATE_LIMITED",
            HubError::MethodNotFound(_) => "E_METHOD_NOT_FOUND",
            HubError::Unavailable => "E_UNAVAILABLE",
            HubError::Internal(_) => "E_INTERNAL",
        }
    }

    /// 是否可重试
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            HubError::Conflict
                | HubError::RateLimited { .. }
                | HubError::Unavailable
                | HubError::Internal(_)
        )
    }

    /// 转换为结构化错误描述
    pub fn to_error_detail(&self) -> ErrorDetail {
        let code = self.error_code().to_string();
        let retryable = self.is_retryable();
        let (message_zh, message_en) = match self {
            HubError::AuthRequired => ("缺少或无效令牌", "Missing or invalid token"),
            HubError::InsufficientScope => ("权限不足", "Insufficient scope"),
            HubError::InvalidArgument(s) => (s.as_str(), "Invalid argument"),
            HubError::NotFound => ("对象不存在或无权访问", "Resource not found"),
            HubError::Conflict => ("版本冲突", "Version conflict"),
            HubError::ConfirmRequired { .. } => ("需要二次确认", "Confirmation required"),
            HubError::ConfirmExpired => {
                ("确认令牌过期或已用", "Confirmation token expired or used")
            }
            HubError::RateLimited { .. } => ("触发限流", "Rate limit exceeded"),
            HubError::MethodNotFound(s) => (s.as_str(), "Method not found"),
            HubError::Unavailable => ("服务暂不可用", "Service temporarily unavailable"),
            HubError::Internal(_) => ("服务内部错误", "Internal server error"),
        };
        ErrorDetail {
            code,
            message_zh: message_zh.to_string(),
            message_en: message_en.to_string(),
            retryable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_codes_are_distinct() {
        let codes = [
            HubError::AuthRequired.error_code(),
            HubError::InsufficientScope.error_code(),
            HubError::InvalidArgument("x".to_string()).error_code(),
            HubError::NotFound.error_code(),
            HubError::Conflict.error_code(),
            HubError::MethodNotFound("m".to_string()).error_code(),
        ];
        let set: std::collections::HashSet<_> = codes.iter().collect();
        assert_eq!(set.len(), codes.len(), "错误码必须互不相同");
    }

    #[test]
    fn test_internal_error_is_retryable() {
        assert!(HubError::Internal(anyhow::anyhow!("boom")).is_retryable());
        assert!(!HubError::NotFound.is_retryable());
        assert!(!HubError::InvalidArgument("x".to_string()).is_retryable());
    }

    #[test]
    fn test_error_detail_serialization() {
        let detail = HubError::NotFound.to_error_detail();
        assert_eq!(detail.code, "E_NOT_FOUND");
        assert!(!detail.retryable);
        let v = serde_json::to_value(&detail).unwrap();
        assert_eq!(v["code"], "E_NOT_FOUND");
        assert!(v["message_zh"].is_string());
    }

    #[test]
    fn test_invalid_argument_carries_message() {
        let e = HubError::InvalidArgument("namespace 不能为空".to_string());
        let detail = e.to_error_detail();
        assert_eq!(detail.message_zh, "namespace 不能为空");
    }
}
