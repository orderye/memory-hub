#![forbid(unsafe_code)]

//! 多智能体长期记忆服务授权服务器 Web 层（auth-web）
//!
//! 提供：
//! - `AgentOAuthServer`：OAuth 2.1 编排（DCR / client_credentials / 授权码+PKCE / 刷新轮换）
//! - `AccountService`：账户注册/登录/会话与注册令牌（沙箱边界的身份能力）
//! - `oauth_router`：axum 路由（元数据、账户页面、注册、授权、令牌、内省、撤销、账户控制台）

pub mod account;
pub mod oauth_flow;
pub mod routes;

pub use account::{ensure_bootstrap_account, AccountService};
pub use oauth_flow::{
    parse_scopes, AgentOAuthServer, AuthorizeParams, IntrospectResponse, RegisterAgentRequest,
    TokenRequest, TokenResponse,
};
pub use routes::{oauth_router, AsWebState, ConsentForm};
