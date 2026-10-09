//! 系统配置加载与校验（基于设计规范 17.5 节）

use std::collections::HashMap;
use std::env;

/// 记忆中枢服务配置项
#[derive(Debug, Clone)]
pub struct AppConfig {
    // 存储与模型
    pub db_url: String,
    pub embed_model_path: Option<String>,
    pub embed_dim: usize,

    // 预算与超时
    pub token_budget_default: usize,
    pub soft_timeout_ms: u64,
    pub retrieval_fts_k: usize,
    pub retrieval_vec_k: usize,
    pub score_weights: [f32; 5], // sim, kw, conf, rec, prov
    pub mmr_lambda: f32,

    // 升格与边际
    pub promote_min_obs: u32,
    pub promote_min_contexts: u32,
    pub promote_min_span_days: u32,
    pub promote_min_conf: f32,
    pub margin_delta: f32,
    pub decay_tau_days: HashMap<String, u32>,

    // 时效与限流
    pub idempotency_ttl_h: u32,
    pub confirm_token_ttl_s: u64,
    pub export_link_ttl_h: u32,
    pub rate_limit_per_client_rps: u32,
    pub rate_limit_per_user_rps: u32,
    pub max_request_bytes: usize,

    // 特性开关
    pub feature_national_memory: bool,
    pub feature_ledger_available: bool,
    pub feature_adapter_gateway: bool,
    pub log_redaction: String,
}

impl AppConfig {
    /// 从环境变量加载并执行严格校验
    pub fn from_env() -> anyhow::Result<Self> {
        let db_url = env::var("HUB_DB_URL").unwrap_or_else(|_| {
            "postgresql://hub_app:secret@localhost:5432/memory_hub".to_string()
        });

        let embed_model_path = env::var("HUB_EMBED_MODEL_PATH")
            .ok()
            .filter(|p| !p.trim().is_empty());

        let embed_dim = env::var("HUB_EMBED_DIM")
            .unwrap_or_else(|_| "1024".to_string())
            .parse::<usize>()?;

        let token_budget_default = env::var("HUB_TOKEN_BUDGET_DEFAULT")
            .unwrap_or_else(|_| "600".to_string())
            .parse::<usize>()?;

        let soft_timeout_ms = env::var("HUB_SOFT_TIMEOUT_MS")
            .unwrap_or_else(|_| "300".to_string())
            .parse::<u64>()?;

        let retrieval_fts_k = env::var("HUB_RETRIEVAL_FTS_K")
            .unwrap_or_else(|_| "20".to_string())
            .parse::<usize>()?;

        let retrieval_vec_k = env::var("HUB_RETRIEVAL_VEC_K")
            .unwrap_or_else(|_| "20".to_string())
            .parse::<usize>()?;

        let score_weights = [0.40, 0.20, 0.15, 0.10, 0.15];
        let mmr_lambda = env::var("HUB_MMR_LAMBDA")
            .unwrap_or_else(|_| "0.7".to_string())
            .parse::<f32>()?;

        let promote_min_obs = env::var("HUB_PROMOTE_MIN_OBS")
            .unwrap_or_else(|_| "3".to_string())
            .parse::<u32>()?;

        let promote_min_contexts = env::var("HUB_PROMOTE_MIN_CONTEXTS")
            .unwrap_or_else(|_| "2".to_string())
            .parse::<u32>()?;

        let promote_min_span_days = env::var("HUB_PROMOTE_MIN_SPAN_DAYS")
            .unwrap_or_else(|_| "7".to_string())
            .parse::<u32>()?;

        let promote_min_conf = env::var("HUB_PROMOTE_MIN_CONF")
            .unwrap_or_else(|_| "0.7".to_string())
            .parse::<f32>()?;

        let margin_delta = env::var("HUB_MARGIN_DELTA")
            .unwrap_or_else(|_| "0.15".to_string())
            .parse::<f32>()?;

        let mut decay_tau_days = HashMap::new();
        decay_tau_days.insert("state".to_string(), 7);
        decay_tau_days.insert("preference".to_string(), 90);
        decay_tau_days.insert("relation".to_string(), 365);
        decay_tau_days.insert("behavior".to_string(), 90);
        decay_tau_days.insert("goal_belief".to_string(), 180);

        let idempotency_ttl_h = env::var("HUB_IDEMPOTENCY_TTL_H")
            .unwrap_or_else(|_| "24".to_string())
            .parse::<u32>()?;

        let confirm_token_ttl_s = env::var("HUB_CONFIRM_TOKEN_TTL_S")
            .unwrap_or_else(|_| "120".to_string())
            .parse::<u64>()?;

        let export_link_ttl_h = env::var("HUB_EXPORT_LINK_TTL_H")
            .unwrap_or_else(|_| "24".to_string())
            .parse::<u32>()?;

        let rate_limit_per_client_rps = env::var("RATE_LIMIT_PER_CLIENT_RPS")
            .unwrap_or_else(|_| "50".to_string())
            .parse::<u32>()?;

        let rate_limit_per_user_rps = env::var("RATE_LIMIT_PER_USER_RPS")
            .unwrap_or_else(|_| "10".to_string())
            .parse::<u32>()?;

        let max_request_bytes = env::var("MAX_REQUEST_BYTES")
            .unwrap_or_else(|_| "65536".to_string())
            .parse::<usize>()?;

        let feature_national_memory = env::var("FEATURE_NATIONAL_MEMORY")
            .unwrap_or_else(|_| "false".to_string())
            .to_lowercase()
            == "true";

        let feature_ledger_available = env::var("FEATURE_LEDGER_AVAILABLE")
            .unwrap_or_else(|_| "true".to_string())
            .to_lowercase()
            == "true";

        let feature_adapter_gateway = env::var("FEATURE_ADAPTER_GATEWAY")
            .unwrap_or_else(|_| "false".to_string())
            .to_lowercase()
            == "true";

        let log_redaction = env::var("LOG_REDACTION").unwrap_or_else(|_| "always".to_string());

        Ok(Self {
            db_url,
            embed_model_path,
            embed_dim,
            token_budget_default,
            soft_timeout_ms,
            retrieval_fts_k,
            retrieval_vec_k,
            score_weights,
            mmr_lambda,
            promote_min_obs,
            promote_min_contexts,
            promote_min_span_days,
            promote_min_conf,
            margin_delta,
            decay_tau_days,
            idempotency_ttl_h,
            confirm_token_ttl_s,
            export_link_ttl_h,
            rate_limit_per_client_rps,
            rate_limit_per_user_rps,
            max_request_bytes,
            feature_national_memory,
            feature_ledger_available,
            feature_adapter_gateway,
            log_redaction,
        })
    }
}
