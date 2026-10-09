//! 统一错误类型与错误码映射（基于设计规范 17.1 与 17.6 节）

use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, HubError>;

/// 记忆中枢统一错误定义
#[derive(thiserror::Error, Debug)]
pub enum HubError {
    #[error("参数不合法: {0}")]
    InvalidArgument(String),

    #[error("内容违反策略: {0}")]
    PolicyBlocked(String),

    #[error("对象不存在")]
    NotFound,

    #[error("版本冲突")]
    Conflict,

    #[error("未提供有效认证凭据")]
    AuthRequired,

    #[error("权限不足")]
    InsufficientScope,

    #[error("用户已撤回授权")]
    ConsentRevoked,

    #[error("触发限流")]
    RateLimited { retry_after_s: Option<u64> },

    #[error("需要用户确认")]
    ConfirmRequired { confirm_token: String },

    #[error("确认令牌过期或已失效")]
    ConfirmExpired,

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
    /// 转换为系统标准错误码（设计规范 17.6 节）
    pub fn error_code(&self) -> &'static str {
        match self {
            HubError::AuthRequired => "E_AUTH_REQUIRED",
            HubError::InsufficientScope => "E_INSUFFICIENT_SCOPE",
            HubError::ConsentRevoked => "E_CONSENT_REVOKED",
            HubError::InvalidArgument(_) => "E_INVALID_ARGUMENT",
            HubError::PolicyBlocked(_) => "E_POLICY_BLOCKED",
            HubError::NotFound => "E_NOT_FOUND",
            HubError::Conflict => "E_CONFLICT",
            HubError::ConfirmRequired { .. } => "E_CONFIRM_REQUIRED",
            HubError::ConfirmExpired => "E_CONFIRM_EXPIRED",
            HubError::RateLimited { .. } => "E_RATE_LIMITED",
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
            HubError::ConsentRevoked => ("用户已撤回授权", "Consent revoked by user"),
            HubError::InvalidArgument(s) => (s.as_str(), "Invalid argument"),
            HubError::PolicyBlocked(s) => (s.as_str(), "Content blocked by policy"),
            HubError::NotFound => ("对象不存在或不属于该用户", "Resource not found"),
            HubError::Conflict => ("版本冲突", "Version conflict"),
            HubError::ConfirmRequired { .. } => ("需要用户确认", "User confirmation required"),
            HubError::ConfirmExpired => {
                ("确认令牌过期或已用", "Confirmation token expired or used")
            }
            HubError::RateLimited { .. } => ("触发限流", "Rate limit exceeded"),
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
