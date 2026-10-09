#![forbid(unsafe_code)]

//! 记忆中枢共享基础库（错误、鉴权、配置与脱敏日志）

pub mod auth;
pub mod config;
pub mod error;
pub mod log;

pub use auth::{Acl, AuthCtx, ClientId, Scope, UserId};
pub use config::AppConfig;
pub use error::{ErrorDetail, HubError, Result};
pub use log::init_tracing;
