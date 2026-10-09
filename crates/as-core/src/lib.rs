#![forbid(unsafe_code)]

//! 授权服务器核心（as-core）
//!
//! 提供：
//! - `verify_pkce_s256`, `compute_code_challenge_s256`：强制 S256 PKCE 验证
//! - `TokenFamilyManager`：OAuth 2.1 刷新令牌强制轮换与重放防御
//! - `ConfirmTokenManager`：120 秒有效期操作确认令牌

pub mod confirm;
pub mod pkce;
pub mod token_family;

pub use confirm::ConfirmTokenManager;
pub use pkce::{compute_code_challenge_s256, verify_pkce_s256};
pub use token_family::{RefreshTokenRecord, TokenFamilyManager};
