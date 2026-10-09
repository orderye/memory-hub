#![forbid(unsafe_code)]

//! 资源服务器鉴权中间件（hub-authz）
//!
//! 提供：
//! - `ProtectedResourceMetadata`：RFC 9728 元数据公布
//! - `TokenValidator`：OAuth 2.1 令牌与 RFC 8707 资源指示符校验

pub mod metadata;
pub mod validator;

pub use metadata::ProtectedResourceMetadata;
pub use validator::{TokenClaims, TokenValidator};
