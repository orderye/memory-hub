//! 多智能体记忆服务配置加载与校验
//!
//! 所有配置项都从环境变量读取并在 `validate` 中做强校验；
//! 非法配置必须在启动期失败（fail-fast），绝不带病进入运行态。

use std::env;
use std::net::SocketAddr;

/// 检索打分的五维权重顺序（sim, kw, conf, rec, fresh）
pub const SCORE_WEIGHT_LEN: usize = 5;

/// 单次快照的默认条目上限（防止备份无限膨胀）
pub const DEFAULT_BACKUP_MAX_ENTRIES: usize = 50_000;

/// 多智能体长期记忆服务配置项
#[derive(Debug, Clone)]
pub struct AppConfig {
    // ===== 存储与模型 =====
    pub db_url: String,
    pub db_max_connections: u32,
    pub embed_dim: usize,
    /// 嵌入器提供方：`mock` | `http`
    pub embed_provider: String,
    pub embed_endpoint: Option<String>,
    pub embed_api_key: Option<String>,

    // ===== 监听地址 =====
    pub mcp_bind: SocketAddr,
    pub auth_bind: SocketAddr,

    // ===== 鉴权与令牌 =====
    /// 访问令牌签名密钥（HS256）
    pub token_secret: String,
    /// 签发方标识（iss）
    pub auth_issuer: String,
    /// 服务资源 URI（aud，RFC 8707 资源指示符）
    pub resource_uri: String,
    pub access_token_ttl_s: i64,
    pub confirm_token_ttl_s: u64,

    // ===== 检索 =====
    pub retrieval_fts_k: usize,
    pub retrieval_vec_k: usize,
    pub score_weights: [f32; SCORE_WEIGHT_LEN],
    pub mmr_lambda: f32,
    /// 单次检索软超时（毫秒），超时返回已召回结果
    pub soft_timeout_ms: u64,

    // ===== 备份 =====
    /// 每个命名空间保留的快照份数（超出后按时间淘汰最旧）
    pub backup_keep_count: usize,
    /// 快照保留天数（0 表示仅按份数淘汰）
    pub backup_retention_days: u32,
    /// 单次快照最大条目数
    pub backup_max_entries: usize,
    /// 定时备份 cron 表达式（5 段：分 时 日 月 周）
    pub backup_cron: String,

    // ===== 整合（去重 / 冲突归并）=====
    /// 判定重复的余弦相似度阈值（0~1）
    pub integrate_sim_threshold: f32,
    /// 单次扫描最多生成的整合提案数
    pub integrate_max_proposals: usize,
    /// 过期记忆清理间隔（小时，0 表示不在 MCP 进程内跑）
    pub expiry_sweep_interval_h: u32,

    // ===== 时效与限流 =====
    pub idempotency_ttl_h: u32,
    pub rate_limit_per_agent_rps: u32,
    pub max_request_bytes: usize,
    /// 允许的 Origin 白名单（空表示不校验，仅限本地桥接）
    pub allowed_origins: Vec<String>,

    // ===== 其他 =====
    pub log_redaction: String,
}

/// 环境变量读取辅助：返回原始字符串
fn var(key: &str) -> Option<String> {
    env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// 环境变量读取辅助：带默认值
fn var_or(key: &str, default: &str) -> String {
    var(key).unwrap_or_else(|| default.to_string())
}

/// 解析无符号整数，失败即报错（fail-fast）
fn parse_u64(key: &str, default: u64) -> anyhow::Result<u64> {
    match var(key) {
        Some(v) => v
            .trim()
            .parse::<u64>()
            .map_err(|_| anyhow::anyhow!("配置项 {} 不是合法无符号整数: {}", key, v)),
        None => Ok(default),
    }
}

fn parse_usize(key: &str, default: usize) -> anyhow::Result<usize> {
    Ok(parse_u64(key, default as u64)? as usize)
}

fn parse_u32(key: &str, default: u32) -> anyhow::Result<u32> {
    let v = parse_u64(key, default as u64)?;
    u32::try_from(v).map_err(|_| anyhow::anyhow!("配置项 {} 超出 u32 范围: {}", key, v))
}

fn parse_f32(key: &str, default: f32) -> anyhow::Result<f32> {
    match var(key) {
        Some(v) => v
            .trim()
            .parse::<f32>()
            .map_err(|_| anyhow::anyhow!("配置项 {} 不是合法浮点数: {}", key, v)),
        None => Ok(default),
    }
}

fn parse_addr(key: &str, default: &str) -> anyhow::Result<SocketAddr> {
    let raw = var_or(key, default);
    raw.trim()
        .parse::<SocketAddr>()
        .map_err(|_| anyhow::anyhow!("配置项 {} 不是合法监听地址（host:port）: {}", key, raw))
}

fn parse_list(key: &str) -> Vec<String> {
    var(key)
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            db_url: "postgresql://mem_app:secret@localhost:5432/memory_hub".to_string(),
            db_max_connections: 10,
            embed_dim: 1024,
            embed_provider: "mock".to_string(),
            embed_endpoint: None,
            embed_api_key: None,
            mcp_bind: "0.0.0.0:3000".parse().unwrap(),
            auth_bind: "0.0.0.0:8080".parse().unwrap(),
            token_secret: "dev-only-insecure-secret".to_string(),
            auth_issuer: "https://auth.memory-hub.local".to_string(),
            resource_uri: "https://hub.memory-hub.local".to_string(),
            access_token_ttl_s: 3600,
            confirm_token_ttl_s: 120,
            retrieval_fts_k: 20,
            retrieval_vec_k: 20,
            score_weights: [0.50, 0.20, 0.10, 0.10, 0.10],
            mmr_lambda: 0.7,
            soft_timeout_ms: 500,
            backup_keep_count: 10,
            backup_retention_days: 90,
            backup_max_entries: DEFAULT_BACKUP_MAX_ENTRIES,
            backup_cron: "0 3 * * *".to_string(),
            integrate_sim_threshold: 0.92,
            integrate_max_proposals: 100,
            expiry_sweep_interval_h: 6,
            idempotency_ttl_h: 24,
            rate_limit_per_agent_rps: 50,
            max_request_bytes: 262_144,
            allowed_origins: Vec::new(),
            log_redaction: "always".to_string(),
        }
    }
}

impl AppConfig {
    /// 从环境变量加载并执行严格校验
    pub fn from_env() -> anyhow::Result<Self> {
        let mut cfg = Self::default();

        cfg.db_url = var_or("HUB_DB_URL", &cfg.db_url);
        cfg.db_max_connections = parse_u32("HUB_DB_MAX_CONNECTIONS", cfg.db_max_connections)?;
        cfg.embed_dim = parse_usize("HUB_EMBED_DIM", cfg.embed_dim)?;
        cfg.embed_provider = var_or("HUB_EMBED_PROVIDER", &cfg.embed_provider).to_lowercase();
        cfg.embed_endpoint = var("HUB_EMBED_ENDPOINT");
        cfg.embed_api_key = var("HUB_EMBED_API_KEY");

        cfg.mcp_bind = parse_addr("HUB_MCP_BIND", "0.0.0.0:3000")?;
        cfg.auth_bind = parse_addr("AUTH_BIND", "0.0.0.0:8080")?;

        cfg.token_secret = var_or("HUB_TOKEN_SECRET", &cfg.token_secret);
        cfg.auth_issuer = var_or("AUTH_ISSUER", &cfg.auth_issuer);
        cfg.resource_uri = var_or("HUB_RESOURCE_URI", &cfg.resource_uri);
        cfg.access_token_ttl_s =
            parse_u64("AS_ACCESS_TOKEN_TTL_S", cfg.access_token_ttl_s as u64)? as i64;
        cfg.confirm_token_ttl_s = parse_u64("HUB_CONFIRM_TOKEN_TTL_S", cfg.confirm_token_ttl_s)?;

        cfg.retrieval_fts_k = parse_usize("HUB_RETRIEVAL_FTS_K", cfg.retrieval_fts_k)?;
        cfg.retrieval_vec_k = parse_usize("HUB_RETRIEVAL_VEC_K", cfg.retrieval_vec_k)?;
        cfg.score_weights = parse_score_weights()?;
        cfg.mmr_lambda = parse_f32("HUB_MMR_LAMBDA", cfg.mmr_lambda)?;
        cfg.soft_timeout_ms = parse_u64("HUB_SOFT_TIMEOUT_MS", cfg.soft_timeout_ms)?;

        cfg.backup_keep_count = parse_usize("BACKUP_KEEP_COUNT", cfg.backup_keep_count)?;
        cfg.backup_retention_days = parse_u32("BACKUP_RETENTION_DAYS", cfg.backup_retention_days)?;
        cfg.backup_max_entries = parse_usize("BACKUP_MAX_ENTRIES", cfg.backup_max_entries)?;
        cfg.backup_cron = var_or("BACKUP_CRON", &cfg.backup_cron);

        cfg.integrate_sim_threshold =
            parse_f32("INTEGRATE_SIM_THRESHOLD", cfg.integrate_sim_threshold)?;
        cfg.integrate_max_proposals =
            parse_usize("INTEGRATE_MAX_PROPOSALS", cfg.integrate_max_proposals)?;
        cfg.expiry_sweep_interval_h =
            parse_u32("EXPIRY_SWEEP_INTERVAL_H", cfg.expiry_sweep_interval_h)?;

        cfg.idempotency_ttl_h = parse_u32("HUB_IDEMPOTENCY_TTL_H", cfg.idempotency_ttl_h)?;
        cfg.rate_limit_per_agent_rps =
            parse_u32("RATE_LIMIT_PER_AGENT_RPS", cfg.rate_limit_per_agent_rps)?;
        cfg.max_request_bytes = parse_usize("MAX_REQUEST_BYTES", cfg.max_request_bytes)?;
        cfg.allowed_origins = parse_list("ALLOWED_ORIGINS");

        cfg.log_redaction = var_or("LOG_REDACTION", &cfg.log_redaction);

        cfg.validate()?;
        Ok(cfg)
    }

    /// 强校验：任何越界配置都在启动期失败
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.embed_dim == 0 {
            anyhow::bail!("HUB_EMBED_DIM 必须大于 0");
        }
        if self.embed_dim > 4096 {
            anyhow::bail!("HUB_EMBED_DIM 超过 pgvector 实用上限 4096");
        }
        if self.db_max_connections == 0 {
            anyhow::bail!("HUB_DB_MAX_CONNECTIONS 必须大于 0");
        }
        if self.soft_timeout_ms == 0 {
            anyhow::bail!("HUB_SOFT_TIMEOUT_MS 必须大于 0");
        }
        if self.max_request_bytes < 1024 {
            anyhow::bail!("MAX_REQUEST_BYTES 不得小于 1024 字节");
        }
        if self.retrieval_fts_k == 0 || self.retrieval_vec_k == 0 {
            anyhow::bail!("HUB_RETRIEVAL_FTS_K / HUB_RETRIEVAL_VEC_K 必须大于 0");
        }
        if self.mmr_lambda < 0.0 || self.mmr_lambda > 1.0 {
            anyhow::bail!("HUB_MMR_LAMBDA 必须落在 [0,1]");
        }

        let wsum: f32 = self.score_weights.iter().sum();
        if (wsum - 1.0).abs() > 1e-3 {
            anyhow::bail!("HUB_SCORE_WEIGHTS 五维权重之和必须为 1.0，当前为 {}", wsum);
        }
        for (i, w) in self.score_weights.iter().enumerate() {
            if *w < 0.0 || *w > 1.0 {
                anyhow::bail!("HUB_SCORE_WEIGHTS 第 {} 项必须落在 [0,1]", i);
            }
        }

        if !(0.0..=1.0).contains(&self.integrate_sim_threshold) {
            anyhow::bail!("INTEGRATE_SIM_THRESHOLD 必须落在 [0,1]");
        }
        if self.integrate_max_proposals == 0 {
            anyhow::bail!("INTEGRATE_MAX_PROPOSALS 必须大于 0");
        }
        if self.backup_keep_count == 0 {
            anyhow::bail!("BACKUP_KEEP_COUNT 必须大于 0（至少保留 1 份）");
        }
        if self.backup_max_entries == 0 {
            anyhow::bail!("BACKUP_MAX_ENTRIES 必须大于 0");
        }

        if self.access_token_ttl_s <= 0 {
            anyhow::bail!("AS_ACCESS_TOKEN_TTL_S 必须大于 0");
        }
        if self.confirm_token_ttl_s == 0 {
            anyhow::bail!("HUB_CONFIRM_TOKEN_TTL_S 必须大于 0");
        }

        match self.embed_provider.as_str() {
            "mock" | "http" => {}
            other => anyhow::bail!("HUB_EMBED_PROVIDER 只支持 mock | http，当前为 {}", other),
        }

        if self.embed_provider == "http"
            && self
                .embed_endpoint
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
        {
            anyhow::bail!("HUB_EMBED_PROVIDER=http 时必须提供 HUB_EMBED_ENDPOINT");
        }

        // 备份 cron 表达式启动期即解析，非法值 fail-fast
        crate::cron::CronSchedule::parse(&self.backup_cron)
            .map_err(|e| anyhow::anyhow!("BACKUP_CRON 不合法: {}", e))?;

        if self.log_redaction != "always" {
            anyhow::bail!("LOG_REDACTION 固定为 always（数据红线，不可关闭）");
        }

        Ok(())
    }

    /// 脱敏摘要（用于启动日志；绝不打印密钥与连接串口令）
    pub fn redacted_summary(&self) -> String {
        let db_host = self
            .db_url
            .split('@')
            .next_back()
            .unwrap_or("<unknown>")
            .to_string();
        format!(
            "db_host={} embed_dim={} embed_provider={} mcp_bind={} auth_bind={} \
             backup_cron={} integrate_threshold={} sweep_h={} origins={}",
            db_host,
            self.embed_dim,
            self.embed_provider,
            self.mcp_bind,
            self.auth_bind,
            self.backup_cron,
            self.integrate_sim_threshold,
            self.expiry_sweep_interval_h,
            self.allowed_origins.len(),
        )
    }
}

/// 解析 `HUB_SCORE_WEIGHTS=a,b,c,d,e`
fn parse_score_weights() -> anyhow::Result<[f32; SCORE_WEIGHT_LEN]> {
    let raw = match var("HUB_SCORE_WEIGHTS") {
        Some(v) => v,
        None => return Ok([0.50, 0.20, 0.10, 0.10, 0.10]),
    };

    let parts: Vec<&str> = raw.split(',').map(str::trim).collect();
    if parts.len() != SCORE_WEIGHT_LEN {
        anyhow::bail!(
            "HUB_SCORE_WEIGHTS 必须提供 {} 个逗号分隔数值，当前 {} 个",
            SCORE_WEIGHT_LEN,
            parts.len()
        );
    }

    let mut out = [0.0f32; SCORE_WEIGHT_LEN];
    for (i, p) in parts.iter().enumerate() {
        out[i] = p
            .parse::<f32>()
            .map_err(|_| anyhow::anyhow!("HUB_SCORE_WEIGHTS 第 {} 项不是合法浮点数: {}", i, p))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 串行化环境变量读写，避免测试间相互干扰
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_env<T>(vars: &[(&str, Option<&str>)], f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved: Vec<(String, Option<String>)> = vars
            .iter()
            .map(|(k, _)| ((*k).to_string(), env::var(k).ok()))
            .collect();

        for (k, v) in vars {
            match v {
                Some(val) => env::set_var(k, val),
                None => env::remove_var(k),
            }
        }

        let out = f();

        for (k, v) in saved {
            match v {
                Some(val) => env::set_var(&k, val),
                None => env::remove_var(k),
            }
        }
        out
    }

    #[test]
    fn test_defaults_are_valid() {
        let cfg = AppConfig::default();
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.score_weights.iter().sum::<f32>(), 1.0);
    }

    #[test]
    fn test_from_env_reads_bind_addresses() {
        with_env(
            &[
                ("HUB_MCP_BIND", Some("127.0.0.1:3999")),
                ("AUTH_BIND", Some("127.0.0.1:3998")),
                ("HUB_SOFT_TIMEOUT_MS", Some("250")),
                ("HUB_EMBED_DIM", Some("768")),
                ("HUB_EMBED_PROVIDER", Some("mock")),
                ("BACKUP_CRON", Some("*/30 * * * *")),
            ],
            || {
                let cfg = AppConfig::from_env().unwrap();
                assert_eq!(cfg.mcp_bind.port(), 3999);
                assert_eq!(cfg.auth_bind.port(), 3998);
                assert_eq!(cfg.soft_timeout_ms, 250);
                assert_eq!(cfg.embed_dim, 768);
                assert_eq!(cfg.backup_cron, "*/30 * * * *");
            },
        );
    }

    #[test]
    fn test_from_env_rejects_bad_timeout() {
        with_env(&[("HUB_SOFT_TIMEOUT_MS", Some("abc"))], || {
            assert!(AppConfig::from_env().is_err());
        });
    }

    #[test]
    fn test_from_env_rejects_bad_bind() {
        with_env(&[("HUB_MCP_BIND", Some("not-an-addr"))], || {
            assert!(AppConfig::from_env().is_err());
        });
    }

    #[test]
    fn test_score_weights_sum_must_be_one() {
        let cfg = AppConfig {
            score_weights: [0.5, 0.5, 0.5, 0.5, 0.5],
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_score_weights_from_env() {
        with_env(
            &[("HUB_SCORE_WEIGHTS", Some("0.3,0.2,0.2,0.2,0.1"))],
            || {
                let cfg = AppConfig::from_env().unwrap();
                assert_eq!(cfg.score_weights[0], 0.3);
                assert!((cfg.score_weights.iter().sum::<f32>() - 1.0).abs() < 1e-6);
            },
        );
    }

    #[test]
    fn test_mmr_lambda_range_enforced() {
        let cfg = AppConfig {
            mmr_lambda: 1.5,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_embed_provider_http_requires_endpoint() {
        with_env(
            &[
                ("HUB_EMBED_PROVIDER", Some("http")),
                ("HUB_EMBED_ENDPOINT", None),
            ],
            || {
                assert!(AppConfig::from_env().is_err());
            },
        );
    }

    #[test]
    fn test_invalid_backup_cron_fails_fast() {
        with_env(&[("BACKUP_CRON", Some("99 * * * *"))], || {
            assert!(AppConfig::from_env().is_err());
        });
    }

    #[test]
    fn test_backup_keep_count_must_be_positive() {
        let cfg = AppConfig {
            backup_keep_count: 0,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_log_redaction_cannot_be_disabled() {
        with_env(&[("LOG_REDACTION", Some("off"))], || {
            assert!(AppConfig::from_env().is_err());
        });
    }

    #[test]
    fn test_redacted_summary_hides_credentials() {
        let cfg = AppConfig {
            db_url: "postgresql://mem_app:supersecret@db.internal:5432/memory_hub".to_string(),
            ..Default::default()
        };
        let s = cfg.redacted_summary();
        assert!(!s.contains("supersecret"), "摘要不得泄露口令: {}", s);
        assert!(!s.contains("mem_app"), "摘要不得泄露用户名: {}", s);
        assert!(s.contains("db.internal:5432"));
    }

    #[test]
    fn test_origins_parsing() {
        with_env(
            &[("ALLOWED_ORIGINS", Some("https://a.com, https://b.com ,,"))],
            || {
                let cfg = AppConfig::from_env().unwrap();
                assert_eq!(cfg.allowed_origins, vec!["https://a.com", "https://b.com"]);
            },
        );
    }
}
