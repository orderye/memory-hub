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

/// 令牌签名密钥的最小长度（字节）。HS256 要拿到 256 位密钥强度，
/// 短于此值的口令可被离线爆破——而该密钥同时是三个进程共享的验签根。
pub const MIN_TOKEN_SECRET_BYTES: usize = 32;

/// 内置开发占位密钥。它绝不可以通过启动校验，否则等于用公开常量签发令牌。
pub const INSECURE_DEFAULT_TOKEN_SECRET: &str = "dev-only-insecure-secret";

/// 默认最小可信分（默认拒绝阈值）。
///
/// 低于该分的候选一律丢弃；全部被丢弃时对外返回"无可靠匹配"，
/// 而不是把低分结果硬塞给 Agent（长期记忆里，错误事实比没有事实更糟）。
///
/// 取值由标定曲线确定（`crates/retrieval/tests/eval_bench.rs`
/// 的 `test_min_score_calibration`，480 文档 / 600 查询）：
///
/// | 阈值 | Recall@1 | Recall@10 | 域外误召回率 | Top-3 硬负例 | 首位误取率 |
/// |------|----------|-----------|--------------|--------------|------------|
/// | 0.00 | 68.33%   | 97.59%    | 100.00%      | 15.19%       | 0.56%      |
/// | 0.55 | 68.33%   | 97.59%    | 100.00%      | 15.19%       | 0.56%      |
/// | 0.60 | 68.33%   | 97.59%    | 15.00%       | 15.74%       | 0.56%      |
/// | 0.65 | 68.15%   | 97.22%    | 0.00%        | 19.63%       | 0.56%      |
/// | 0.70 | 67.41%   | 94.44%    | 0.00%        | 14.63%       | 0.56%      |
/// | 0.80 | 62.78%   | 85.93%    | 0.00%        | 8.70%        | 0.19%      |
///
/// 选取规则：能把**域外误召回率压到 0** 的最小阈值 —— 即 0.65。
/// 更高的阈值继续压低 Top-3 硬负例占比，但 Recall@10 掉到 85% 以下，
/// 属于"用召回深度换表面精度"，不划算。
///
/// 关于硬负例：Top-3 里混入矛盾项与**首位**取到矛盾项是两件事。
/// 实测首位误取率仅 0.56%（矛盾项总是排在正确答案之后），
/// 这才是长期记忆里真正会造成错误事实的路径；
/// Top-3 混入矛盾项 Agent 尚可凭排序与判断回避。
/// 但在伪向量下，词法打分本来就无法区分"同主题但事实矛盾"的条目，
/// 因此该指标**不构成语义判别能力的结论**，必须在真实嵌入模型下重新判定。
///
/// **该值必须在真实嵌入模型下重新标定**：伪向量下的分数分布是词法分布，
/// 接入真实模型后 sim 维度参与打分，分数尺度会变（见标定用例）。
pub const DEFAULT_RETRIEVAL_MIN_SCORE: f32 = 0.65;

/// 多智能体长期记忆服务配置项
#[derive(Debug, Clone)]
pub struct AppConfig {
    // ===== 存储与模型 =====
    pub db_url: String,
    pub db_max_connections: u32,
    /// 单条 SQL 的服务器端超时（毫秒）；0 = 不限制。
    /// 没有它，一条被慢查询或锁等待拖住的语句会一直占着连接池里的坑位。
    pub db_statement_timeout_ms: u64,
    pub embed_dim: usize,
    /// 嵌入器提供方：`mock` | `http`
    pub embed_provider: String,
    /// 嵌入模型标识（`HUB_EMBED_MODEL`）；仅 http 提供方下有意义
    pub embed_model: String,
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

    // ===== 账户（沙箱）=====
    /// 引导账户用户名（存量数据归属；仅 auth-server 使用）
    pub bootstrap_username: String,
    /// 引导账户口令（未设置则不写入；仅 auth-server 使用）
    pub bootstrap_password: Option<String>,
    /// 账户注册邀请码（未设置 = 开放注册）
    pub invite_code: Option<String>,
    /// 账户登录会话有效期（小时）
    pub auth_session_ttl_h: i64,

    // ===== 检索 =====
    pub retrieval_fts_k: usize,
    pub retrieval_vec_k: usize,
    /// 近期性时间常数覆盖（天）：None 表示按记忆类型自动取值
    pub retrieval_recency_tau_days: Option<f32>,
    /// 场景过滤模式：`strict`（默认，硬过滤）/ `prefer`（软偏好，需评测确认后开启）
    pub retrieval_scene_mode: String,
    /// 分场景权重 profile（`chat` / `working` / `coding`），空表示统一使用 `score_weights`
    pub retrieval_scene_weight_profiles: std::collections::HashMap<String, [f32; SCORE_WEIGHT_LEN]>,
    pub score_weights: [f32; SCORE_WEIGHT_LEN],
    pub mmr_lambda: f32,
    /// 默认最小可信分（0 = 关闭默认拒绝阈值）
    pub retrieval_min_score: f32,
    /// 分场景最小可信分覆盖（缺省回落到 `retrieval_min_score`）
    pub retrieval_scene_min_scores: std::collections::HashMap<String, f32>,
    /// 单次检索软超时（毫秒），超时返回已召回结果
    pub soft_timeout_ms: u64,

    // ===== 备份 =====
    /// 每个命名空间保留的快照份数（超出后按时间淘汰最旧）
    pub backup_keep_count: usize,
    /// 快照保留天数（0 表示仅按份数淘汰）
    pub backup_retention_days: u32,
    /// 单次快照最大条目数
    pub backup_max_entries: usize,
    /// 证据保留期限（天）；0 = 永久保留
    ///
    /// 证据写入前已脱敏（见 `mem-core::redaction`），本项是第二道防线：
    /// 到期后由清理任务物理删除，不让不可变副本无限期留存。
    pub evidence_retention_days: u32,
    /// 定时备份 cron 表达式（5 段：分 时 日 月 周）
    pub backup_cron: String,

    // ===== 整合（去重 / 冲突归并）=====
    /// 判定重复的余弦相似度阈值（0~1）
    pub integrate_sim_threshold: f32,
    /// 单次扫描最多生成的整合提案数
    pub integrate_max_proposals: usize,
    /// 过期记忆清理间隔（小时，0 表示不在 MCP 进程内跑）
    pub expiry_sweep_interval_h: u32,
    /// 候选发现模式：`rules`（纯本地规则，默认）| `hybrid`（向量辅助）
    pub consolidation_mode: String,
    /// 向量模式不可用时的降级策略：`rules`（降级）| `error`（报错）
    pub consolidation_fallback: String,
    /// 是否允许 MCP 单次请求覆盖服务端模式
    pub consolidation_allow_mode_override: bool,
    /// 向量模式单次扫描最多新计算的向量条目数
    pub consolidation_embed_top_k: usize,
    /// 向量模式语义候选的相似度下限；`None` 表示**未经真实模型标定**
    pub consolidation_embed_min_similarity: Option<f32>,
    /// 纯规则模式的词法候选相似度下限（Jaccard）
    pub consolidation_lexical_min_similarity: f32,

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

/// 解析布尔配置：仅 `true` / `1` / `yes` 为真，其余非空值一律报错
///
/// 与 cjs 适配层"非 0/false 即为真"的宽松语义不同：布尔开关写错时
/// 静默取反会让"是否允许请求覆盖整合模式"这类安全开关失效，因此 fail-fast。
fn parse_bool(key: &str, default: bool) -> anyhow::Result<bool> {
    match var(key) {
        Some(v) => match v.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Ok(true),
            "false" | "0" | "no" => Ok(false),
            other => Err(anyhow::anyhow!(
                "配置项 {} 只能是 true/false/1/0/yes/no，当前为 {}",
                key,
                other
            )),
        },
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
            db_statement_timeout_ms: 30_000,
            embed_dim: 1024,
            embed_provider: "mock".to_string(),
            embed_model: "bge-m3".to_string(),
            embed_endpoint: None,
            embed_api_key: None,
            mcp_bind: "0.0.0.0:3000".parse().unwrap(),
            auth_bind: "0.0.0.0:8080".parse().unwrap(),
            token_secret: "dev-only-insecure-secret".to_string(),
            auth_issuer: "https://auth.memory-hub.local".to_string(),
            resource_uri: "https://hub.memory-hub.local".to_string(),
            access_token_ttl_s: 3600,
            confirm_token_ttl_s: 120,
            bootstrap_username: "bootstrap".to_string(),
            bootstrap_password: None,
            invite_code: None,
            auth_session_ttl_h: 720,
            // 候选池取 100：融合打分前按相关度取较大候选集，
            // 避免"匹配文档数超过候选池时更相关但更旧的文档被截断丢弃"
            retrieval_fts_k: 100,
            retrieval_vec_k: 100,
            retrieval_recency_tau_days: None,
            retrieval_scene_mode: "strict".to_string(),
            retrieval_scene_weight_profiles: std::collections::HashMap::new(),
            score_weights: [0.50, 0.20, 0.10, 0.10, 0.10],
            mmr_lambda: 0.7,
            retrieval_min_score: DEFAULT_RETRIEVAL_MIN_SCORE,
            retrieval_scene_min_scores: std::collections::HashMap::new(),
            soft_timeout_ms: 500,
            backup_keep_count: 10,
            backup_retention_days: 90,
            backup_max_entries: DEFAULT_BACKUP_MAX_ENTRIES,
            evidence_retention_days: 90,
            backup_cron: "0 3 * * *".to_string(),
            integrate_sim_threshold: 0.92,
            integrate_max_proposals: 100,
            expiry_sweep_interval_h: 6,
            consolidation_mode: "rules".to_string(),
            consolidation_fallback: "rules".to_string(),
            consolidation_allow_mode_override: false,
            consolidation_embed_top_k: 20,
            consolidation_embed_min_similarity: None,
            consolidation_lexical_min_similarity: 0.80,
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
        cfg.db_statement_timeout_ms =
            parse_u64("HUB_DB_STATEMENT_TIMEOUT_MS", cfg.db_statement_timeout_ms)?;
        cfg.embed_dim = parse_usize("HUB_EMBED_DIM", cfg.embed_dim)?;
        cfg.embed_provider = var_or("HUB_EMBED_PROVIDER", &cfg.embed_provider).to_lowercase();
        cfg.embed_model = var_or("HUB_EMBED_MODEL", &cfg.embed_model);
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

        cfg.bootstrap_username = var_or("HUB_BOOTSTRAP_USERNAME", &cfg.bootstrap_username);
        cfg.bootstrap_password = var("HUB_BOOTSTRAP_PASSWORD");
        cfg.invite_code = var("AUTH_INVITE_CODE");
        let session_ttl_h = parse_u64("AUTH_SESSION_TTL_H", cfg.auth_session_ttl_h as u64)?;
        cfg.auth_session_ttl_h = session_ttl_h.max(1) as i64;

        cfg.retrieval_fts_k = parse_usize("HUB_RETRIEVAL_FTS_K", cfg.retrieval_fts_k)?;
        cfg.retrieval_vec_k = parse_usize("HUB_RETRIEVAL_VEC_K", cfg.retrieval_vec_k)?;
        cfg.retrieval_recency_tau_days = match var("HUB_RETRIEVAL_RECENCY_TAU_DAYS") {
            Some(_) => Some(parse_f32(
                "HUB_RETRIEVAL_RECENCY_TAU_DAYS",
                cfg.retrieval_recency_tau_days.unwrap_or(30.0),
            )?),
            None => None,
        };
        cfg.retrieval_scene_mode =
            var_or("HUB_RETRIEVAL_SCENE_MODE", &cfg.retrieval_scene_mode).to_lowercase();
        cfg.retrieval_scene_weight_profiles = parse_scene_weight_profiles()?;
        cfg.score_weights = parse_score_weights()?;
        cfg.mmr_lambda = parse_f32("HUB_MMR_LAMBDA", cfg.mmr_lambda)?;
        cfg.retrieval_min_score = parse_f32("HUB_RETRIEVAL_MIN_SCORE", cfg.retrieval_min_score)?;
        cfg.retrieval_scene_min_scores = parse_scene_min_scores()?;
        cfg.soft_timeout_ms = parse_u64("HUB_SOFT_TIMEOUT_MS", cfg.soft_timeout_ms)?;

        cfg.backup_keep_count = parse_usize("BACKUP_KEEP_COUNT", cfg.backup_keep_count)?;
        cfg.backup_retention_days = parse_u32("BACKUP_RETENTION_DAYS", cfg.backup_retention_days)?;
        cfg.backup_max_entries = parse_usize("BACKUP_MAX_ENTRIES", cfg.backup_max_entries)?;
        cfg.evidence_retention_days =
            parse_u32("HUB_EVIDENCE_RETENTION_DAYS", cfg.evidence_retention_days)?;
        cfg.backup_cron = var_or("BACKUP_CRON", &cfg.backup_cron);

        cfg.integrate_sim_threshold =
            parse_f32("INTEGRATE_SIM_THRESHOLD", cfg.integrate_sim_threshold)?;
        cfg.integrate_max_proposals =
            parse_usize("INTEGRATE_MAX_PROPOSALS", cfg.integrate_max_proposals)?;
        cfg.expiry_sweep_interval_h =
            parse_u32("EXPIRY_SWEEP_INTERVAL_H", cfg.expiry_sweep_interval_h)?;

        cfg.consolidation_mode =
            var_or("HUB_CONSOLIDATION_MODE", &cfg.consolidation_mode).to_lowercase();
        cfg.consolidation_fallback =
            var_or("HUB_CONSOLIDATION_FALLBACK", &cfg.consolidation_fallback).to_lowercase();
        cfg.consolidation_allow_mode_override = parse_bool(
            "HUB_CONSOLIDATION_ALLOW_MODE_OVERRIDE",
            cfg.consolidation_allow_mode_override,
        )?;
        cfg.consolidation_embed_top_k = parse_usize(
            "HUB_CONSOLIDATION_EMBED_TOP_K",
            cfg.consolidation_embed_top_k,
        )?;
        cfg.consolidation_embed_min_similarity = match var("HUB_CONSOLIDATION_EMBED_MIN_SIMILARITY")
        {
            Some(_) => Some(parse_f32(
                "HUB_CONSOLIDATION_EMBED_MIN_SIMILARITY",
                cfg.consolidation_embed_min_similarity.unwrap_or(0.92),
            )?),
            None => None,
        };
        cfg.consolidation_lexical_min_similarity = parse_f32(
            "HUB_CONSOLIDATION_LEXICAL_MIN_SIMILARITY",
            cfg.consolidation_lexical_min_similarity,
        )?;

        cfg.idempotency_ttl_h = parse_u32("HUB_IDEMPOTENCY_TTL_H", cfg.idempotency_ttl_h)?;
        cfg.rate_limit_per_agent_rps =
            parse_u32("RATE_LIMIT_PER_AGENT_RPS", cfg.rate_limit_per_agent_rps)?;
        cfg.max_request_bytes = parse_usize("MAX_REQUEST_BYTES", cfg.max_request_bytes)?;
        cfg.allowed_origins = parse_list("ALLOWED_ORIGINS");

        cfg.log_redaction = var_or("LOG_REDACTION", &cfg.log_redaction);

        cfg.validate()?;
        Ok(cfg)
    }

    /// 后端是否为 PostgreSQL（三个二进制都用这一判定，避免各自复制字符串前缀检查）
    pub fn is_postgres(&self) -> bool {
        self.db_url.starts_with("postgres://") || self.db_url.starts_with("postgresql://")
    }

    /// 语义检索能力是否就绪
    ///
    /// 只有接入真实嵌入服务（`HUB_EMBED_PROVIDER=http`）才算就绪。
    /// mock 提供方生成的是伪向量：它的余弦相似度是噪声，
    /// 此时系统退化为**词法检索**，对外必须如实声明"语义能力未就绪"。
    pub fn semantic_ready(&self) -> bool {
        self.embed_provider == "http"
    }

    /// 语义能力未就绪时的原因说明（供健康检查与启动告警展示）
    pub fn semantic_status_reason(&self) -> Option<String> {
        if self.semantic_ready() {
            return None;
        }
        Some(format!(
            "嵌入提供方为 {}（伪向量），余弦相似度是噪声而非语义：当前为词法检索，语义召回不可用",
            self.embed_provider
        ))
    }

    /// 强校验：任何越界配置都在启动期失败
    pub fn validate(&self) -> anyhow::Result<()> {
        // 令牌签名根：auth-server 签发、mcp-server 与 backup-worker 离线验签，
        // 三方共用一个字面量。占位值或弱口令一旦带上生产，
        // 任何知道它的人都能伪造任意账户、任意 scope 的访问令牌，
        // 且已泄露的密钥无法在不重启的情况下作废（无轮换、无 jti 黑名单）。
        if self.token_secret.is_empty() {
            anyhow::bail!("HUB_TOKEN_SECRET 必须设置，且 auth-server 与 mcp-server 取值一致");
        }
        if self.token_secret == INSECURE_DEFAULT_TOKEN_SECRET {
            anyhow::bail!(
                "HUB_TOKEN_SECRET 仍是内置开发占位值，拒绝启动；请替换为至少 {} 字节的随机密钥",
                MIN_TOKEN_SECRET_BYTES
            );
        }
        if self.token_secret.len() < MIN_TOKEN_SECRET_BYTES {
            anyhow::bail!(
                "HUB_TOKEN_SECRET 至少需要 {} 字节，当前 {} 字节",
                MIN_TOKEN_SECRET_BYTES,
                self.token_secret.len()
            );
        }

        if self.embed_dim == 0 {
            anyhow::bail!("HUB_EMBED_DIM 必须大于 0");
        }
        if self.embed_dim > 4096 {
            anyhow::bail!("HUB_EMBED_DIM 超过 pgvector 实用上限 4096");
        }
        // schema 里的列是固定的 vector(1024)（migrations/…001_initial_schema.sql）。
        // 维度不符时启动期一切正常、首次写入才炸出 22000，因此在启动就拦住。
        const SCHEMA_VECTOR_DIM: usize = 1024;
        if self.is_postgres() && self.embed_dim != SCHEMA_VECTOR_DIM {
            anyhow::bail!(
                "PostgreSQL 后端的 memories.embedding 列固定为 vector({SCHEMA_VECTOR_DIM})，\
                 HUB_EMBED_DIM 当前为 {}；请改用 {SCHEMA_VECTOR_DIM} 维模型，\
                 或先迁移列定义",
                self.embed_dim
            );
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
        if !(0.0..=1.0).contains(&self.retrieval_min_score) {
            anyhow::bail!("HUB_RETRIEVAL_MIN_SCORE 必须落在 [0,1]");
        }
        for (name, v) in &self.retrieval_scene_min_scores {
            if !(0.0..=1.0).contains(v) {
                anyhow::bail!(
                    "HUB_SCENE_MIN_SCORE_PROFILES 的 {name} 阈值必须落在 [0,1]，当前 {v}"
                );
            }
        }
        if let Some(tau) = self.retrieval_recency_tau_days {
            if tau <= 0.0 {
                anyhow::bail!("HUB_RETRIEVAL_RECENCY_TAU_DAYS 必须大于 0");
            }
        }
        if self.retrieval_scene_mode != "strict" && self.retrieval_scene_mode != "prefer" {
            anyhow::bail!("HUB_RETRIEVAL_SCENE_MODE 只能是 strict 或 prefer");
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

        // 整合模式：未知值必须启动失败，绝不静默猜测（也不因配置了检索 embedding
        // 就自动切到 hybrid——那会把记忆内容发给外部服务）
        crate::consolidation::ConsolidationMode::parse(&self.consolidation_mode)
            .map_err(|e| anyhow::anyhow!("HUB_CONSOLIDATION_MODE 非法: {}", e))?;
        crate::consolidation::ConsolidationFallback::parse(&self.consolidation_fallback)
            .map_err(|e| anyhow::anyhow!("HUB_CONSOLIDATION_FALLBACK 非法: {}", e))?;
        if self.consolidation_embed_top_k == 0 || self.consolidation_embed_top_k > 200 {
            anyhow::bail!("HUB_CONSOLIDATION_EMBED_TOP_K 必须落在 [1,200]");
        }
        if let Some(v) = self.consolidation_embed_min_similarity {
            if !(0.0..=1.0).contains(&v) {
                anyhow::bail!("HUB_CONSOLIDATION_EMBED_MIN_SIMILARITY 必须落在 [0,1]");
            }
        }
        if !(0.0..=1.0).contains(&self.consolidation_lexical_min_similarity) {
            anyhow::bail!("HUB_CONSOLIDATION_LEXICAL_MIN_SIMILARITY 必须落在 [0,1]");
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
            "db_host={} embed_dim={} embed_provider={} embed_model={} semantic_ready={} \
             mcp_bind={} auth_bind={} \
             backup_cron={} integrate_threshold={} sweep_h={} origins={} \
             min_score={} invite={} bootstrap_pwd={} consolidation_mode={} \
             consolidation_fallback={} consolidation_override={} embed_top_k={}",
            db_host,
            self.embed_dim,
            self.embed_provider,
            self.embed_model,
            self.semantic_ready(),
            self.mcp_bind,
            self.auth_bind,
            self.backup_cron,
            self.integrate_sim_threshold,
            self.expiry_sweep_interval_h,
            self.allowed_origins.len(),
            self.retrieval_min_score,
            if self.invite_code.is_some() {
                "set"
            } else {
                "open"
            },
            if self.bootstrap_password.is_some() {
                "set"
            } else {
                "unset"
            },
            self.consolidation_mode,
            self.consolidation_fallback,
            self.consolidation_allow_mode_override,
            self.consolidation_embed_top_k,
        )
    }
}

/// 解析 `HUB_SCORE_WEIGHTS=a,b,c,d,e`
/// 解析分场景权重 profile：
/// `HUB_SCENE_WEIGHT_PROFILES="chat=0.45,0.20,0.10,0.15,0.10;coding=0.45,0.25,0.10,0.05,0.15"`
///
/// 每个 profile 必须是 5 个 [0,1] 权重且之和为 1；场景名限定 chat / working / coding。
/// 未配置时全部请求使用全局 `HUB_SCORE_WEIGHTS`（默认行为不变）。
fn parse_scene_weight_profiles(
) -> anyhow::Result<std::collections::HashMap<String, [f32; SCORE_WEIGHT_LEN]>> {
    let Some(raw) = var("HUB_SCENE_WEIGHT_PROFILES") else {
        return Ok(std::collections::HashMap::new());
    };
    let mut profiles = std::collections::HashMap::new();
    for entry in raw.split(';').map(str::trim).filter(|e| !e.is_empty()) {
        let (name, values) = entry
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("HUB_SCENE_WEIGHT_PROFILES 条目缺少 '='：{entry}"))?;
        let name = name.trim().to_lowercase();
        if !matches!(name.as_str(), "chat" | "working" | "coding") {
            anyhow::bail!(
                "HUB_SCENE_WEIGHT_PROFILES 不支持的场景：{name}（仅 chat/working/coding）"
            );
        }
        let mut weights = [0f32; SCORE_WEIGHT_LEN];
        let parts: Vec<&str> = values.split(',').map(str::trim).collect();
        if parts.len() != SCORE_WEIGHT_LEN {
            anyhow::bail!(
                "HUB_SCENE_WEIGHT_PROFILES 的 {name} 需要 {SCORE_WEIGHT_LEN} 个权重，当前 {}",
                parts.len()
            );
        }
        for (i, part) in parts.iter().enumerate() {
            let w: f32 = part.parse().map_err(|_| {
                anyhow::anyhow!("HUB_SCENE_WEIGHT_PROFILES 权重不是合法数字：{part}")
            })?;
            if !(0.0..=1.0).contains(&w) {
                anyhow::bail!(
                    "HUB_SCENE_WEIGHT_PROFILES 的 {name} 第 {} 项必须落在 [0,1]",
                    i + 1
                );
            }
            weights[i] = w;
        }
        let sum: f32 = weights.iter().sum();
        if (sum - 1.0).abs() > 1e-3 {
            anyhow::bail!("HUB_SCENE_WEIGHT_PROFILES 的 {name} 权重之和必须为 1.0，当前为 {sum}");
        }
        profiles.insert(name, weights);
    }
    Ok(profiles)
}

/// 解析分场景最小可信分：`HUB_SCENE_MIN_SCORE_PROFILES="chat=0.55;working=0.65"`
///
/// 场景名限定 chat / working / coding；未配置的场景回落到全局 `HUB_RETRIEVAL_MIN_SCORE`。
/// 分场景阈值存在的意义：不同场景对"宁可无结果"的容忍度不同——
/// Chat 事实错一次的代价远高于 Working 经验少命中一次。
fn parse_scene_min_scores() -> anyhow::Result<std::collections::HashMap<String, f32>> {
    let Some(raw) = var("HUB_SCENE_MIN_SCORE_PROFILES") else {
        return Ok(std::collections::HashMap::new());
    };
    let mut out = std::collections::HashMap::new();
    for entry in raw.split(';').map(str::trim).filter(|e| !e.is_empty()) {
        let (name, value) = entry
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("HUB_SCENE_MIN_SCORE_PROFILES 条目缺少 '='：{entry}"))?;
        let name = name.trim().to_lowercase();
        if !matches!(name.as_str(), "chat" | "working" | "coding") {
            anyhow::bail!(
                "HUB_SCENE_MIN_SCORE_PROFILES 不支持的场景：{name}（仅 chat/working/coding）"
            );
        }
        let v: f32 = value.trim().parse().map_err(|_| {
            anyhow::anyhow!("HUB_SCENE_MIN_SCORE_PROFILES 的 {name} 阈值不是合法数字")
        })?;
        if !(0.0..=1.0).contains(&v) {
            anyhow::bail!("HUB_SCENE_MIN_SCORE_PROFILES 的 {name} 阈值必须落在 [0,1]");
        }
        out.insert(name, v);
    }
    Ok(out)
}

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

    /// 够长的测试密钥：validate() 拒收占位值与短密钥，用例需自带合规值，
    /// 否则"密钥不合规"会让 from_env 以意料之外的原因失败。
    const TEST_SECRET: &str = "unit-test-token-secret-0123456789abcdef";

    fn with_env<T>(vars: &[(&str, Option<&str>)], f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap();

        // 未显式指定 HUB_TOKEN_SECRET 的用例统一注入合规测试密钥。
        let mut effective: Vec<(&str, Option<&str>)> = vars.iter().map(|(k, v)| (*k, *v)).collect();
        let declares_secret = vars.iter().any(|(k, _)| *k == "HUB_TOKEN_SECRET");
        if !declares_secret {
            effective.push(("HUB_TOKEN_SECRET", Some(TEST_SECRET)));
        }

        let saved: Vec<(String, Option<String>)> = effective
            .iter()
            .map(|(k, _)| ((*k).to_string(), env::var(k).ok()))
            .collect();

        for (k, v) in &effective {
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
        // 默认值整体自洽，但内置占位密钥必须被拒——否则带着默认密钥也能启动。
        assert!(
            cfg.validate().is_err(),
            "默认占位 token_secret 必须被 validate() 拒绝"
        );
        assert_eq!(cfg.score_weights.iter().sum::<f32>(), 1.0);

        let secured = AppConfig {
            token_secret: TEST_SECRET.to_string(),
            ..Default::default()
        };
        assert!(secured.validate().is_ok());
    }

    #[test]
    fn test_token_secret_strength_enforced() {
        let short = AppConfig {
            token_secret: "too-short".to_string(),
            ..Default::default()
        };
        assert!(short.validate().is_err(), "短密钥可被离线爆破，必须拒绝");

        let empty = AppConfig {
            token_secret: String::new(),
            ..Default::default()
        };
        assert!(empty.validate().is_err());

        // from_env 未设置 HUB_TOKEN_SECRET 时不得静默使用占位值
        with_env(
            &[("HUB_TOKEN_SECRET", Some(INSECURE_DEFAULT_TOKEN_SECRET))],
            || {
                assert!(
                    AppConfig::from_env().is_err(),
                    "占位密钥必须 fail-fast，而非告警后继续启动"
                );
            },
        );
    }

    #[test]
    fn test_from_env_reads_bind_addresses() {
        with_env(
            &[
                ("HUB_MCP_BIND", Some("127.0.0.1:3999")),
                ("AUTH_BIND", Some("127.0.0.1:3998")),
                ("HUB_SOFT_TIMEOUT_MS", Some("250")),
                // 768 维只在非 PostgreSQL 后端下合法（PG 列固定 vector(1024)）
                ("HUB_DB_URL", Some("memory://local")),
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
    fn test_scene_mode_and_weight_profiles_from_env() {
        with_env(&[("HUB_RETRIEVAL_SCENE_MODE", Some("prefer"))], || {
            let cfg = AppConfig::from_env().unwrap();
            assert_eq!(cfg.retrieval_scene_mode, "prefer");
        });
        with_env(&[("HUB_RETRIEVAL_SCENE_MODE", Some("loose"))], || {
            assert!(AppConfig::from_env().is_err(), "未支持的场景模式必须被拒绝");
        });

        with_env(
            &[(
                "HUB_SCENE_WEIGHT_PROFILES",
                Some("chat=0.45,0.20,0.10,0.15,0.10;coding=1.0,0,0,0,0"),
            )],
            || {
                let cfg = AppConfig::from_env().unwrap();
                assert_eq!(cfg.retrieval_scene_weight_profiles.len(), 2);
                assert!((cfg.retrieval_scene_weight_profiles["chat"][0] - 0.45).abs() < 1e-6);
            },
        );
        with_env(
            &[(
                "HUB_SCENE_WEIGHT_PROFILES",
                Some("chat=0.5,0.5,0.5,0.5,0.5"),
            )],
            || {
                assert!(
                    AppConfig::from_env().is_err(),
                    "profile 权重之和不为 1 必须被拒绝"
                );
            },
        );
        with_env(
            &[("HUB_SCENE_WEIGHT_PROFILES", Some("unknown=1.0,0,0,0,0"))],
            || {
                assert!(AppConfig::from_env().is_err(), "未知场景名必须被拒绝");
            },
        );
    }

    #[test]
    fn test_recency_tau_override_must_be_positive() {
        // 基准配置需先满足其它启动校验（此处仅替换占位 token 密钥）
        let base = AppConfig {
            token_secret: "k".repeat(64),
            ..Default::default()
        };
        assert!(base.validate().is_ok(), "{:?}", base.validate());

        let invalid = AppConfig {
            retrieval_recency_tau_days: Some(0.0),
            ..base.clone()
        };
        let err = invalid.validate().expect_err("τ 为 0 必须被拒绝");
        assert!(
            err.to_string().contains("HUB_RETRIEVAL_RECENCY_TAU_DAYS"),
            "错误信息应指明配置项：{err}"
        );

        let valid = AppConfig {
            retrieval_recency_tau_days: Some(45.0),
            ..base
        };
        assert!(valid.validate().is_ok());
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

    #[test]
    fn test_semantic_ready_only_with_real_embedder() {
        // mock 提供方 = 伪向量 = 语义能力未就绪
        with_env(&[("HUB_EMBED_PROVIDER", Some("mock"))], || {
            let cfg = AppConfig::from_env().unwrap();
            assert!(!cfg.semantic_ready());
            assert!(cfg.semantic_status_reason().is_some());
        });

        with_env(
            &[
                ("HUB_EMBED_PROVIDER", Some("http")),
                (
                    "HUB_EMBED_ENDPOINT",
                    Some("http://127.0.0.1:9999/v1/embeddings"),
                ),
            ],
            || {
                let cfg = AppConfig::from_env().unwrap();
                assert!(cfg.semantic_ready());
                assert!(cfg.semantic_status_reason().is_none());
            },
        );
    }

    #[test]
    fn test_min_score_from_env_and_validation() {
        with_env(&[("HUB_RETRIEVAL_MIN_SCORE", Some("0.72"))], || {
            let cfg = AppConfig::from_env().unwrap();
            assert!((cfg.retrieval_min_score - 0.72).abs() < 1e-6);
        });

        // 越界必须被拒绝：阈值失真会让"无可靠匹配"永远不触发
        with_env(&[("HUB_RETRIEVAL_MIN_SCORE", Some("1.5"))], || {
            assert!(AppConfig::from_env().is_err());
        });
        with_env(&[("HUB_RETRIEVAL_MIN_SCORE", Some("-0.1"))], || {
            assert!(AppConfig::from_env().is_err());
        });

        // 0 = 显式关闭默认拒绝阈值
        with_env(&[("HUB_RETRIEVAL_MIN_SCORE", Some("0"))], || {
            let cfg = AppConfig::from_env().unwrap();
            assert_eq!(cfg.retrieval_min_score, 0.0);
        });
    }

    #[test]
    fn test_scene_min_score_profiles() {
        with_env(
            &[(
                "HUB_SCENE_MIN_SCORE_PROFILES",
                Some("chat=0.55;working=0.68"),
            )],
            || {
                let cfg = AppConfig::from_env().unwrap();
                assert_eq!(cfg.retrieval_scene_min_scores.len(), 2);
                assert!((cfg.retrieval_scene_min_scores["chat"] - 0.55).abs() < 1e-6);
            },
        );
        with_env(
            &[("HUB_SCENE_MIN_SCORE_PROFILES", Some("unknown=0.5"))],
            || {
                assert!(AppConfig::from_env().is_err(), "未知场景名必须被拒绝");
            },
        );
        with_env(
            &[("HUB_SCENE_MIN_SCORE_PROFILES", Some("chat=2.0"))],
            || {
                assert!(AppConfig::from_env().is_err(), "越界阈值必须被拒绝");
            },
        );
    }
}
