//! 检索评测基线 Bench v2（数据集 / 指标 / 硬负例 / 消融 / 相对门禁）
//!
//! 与旧 `eval_suite.rs`（管道连通性冒烟）不同，本套件的目标是**可回归的质量基线**：
//!
//! - **数据集**：程序化确定性生成（主题 × 变体），覆盖精确命中、语义改写、时间有效性、
//!   事实演化、场景区分、Working 环境边界与噪声负例；每个主题都带硬负例
//!   （同主题旧版本、语义相似但矛盾条目、同措辞跨场景干扰）。
//!   数据集指纹（SHA-256）写入报告，保证"基线是哪一个数据集"可追溯。
//! - **指标**：Recall@1/5/10、MRR@10、nDCG@10、硬负例违规率（Top-3），并按场景分层；
//!   懒加载延迟 p50/p95（进程内）。
//! - **门禁**：与冻结基线逐指标比较，超过容差即失败（相对基线不退化），
//!   而不是旧套件的"绝对 100%"。
//! - **嵌入器**：默认 `TestEmbedder`（伪向量）——只能检测管道与排序逻辑回归，
//!   语义改写类样本不作为门禁；配置 `HUB_EVAL_EMBED_ENDPOINT` 时改用真实 HTTP 嵌入模型，
//!   向量缓存到 `HUB_EVAL_EMBED_CACHE`（默认临时目录），使重复运行离线且可复现。
//! - **消融**：`HUB_EVAL_ABLATIONS=1` 时逐信号 / 场景加成 / MMR λ / 近期性 τ 做消融并打表。
//! - **PostgreSQL**：`HUB_EVAL_PG=1` + `DATABASE_URL` 时在真实 PG 上跑同一数据集
//!   （用于 p50/p95 与 PG 侧词法/索引行为），需要 `--ignored` 显式触发。
//!
//! 运行方式：
//! ```bash
//! cargo test --test eval_bench -- --nocapture                 # 门禁（默认伪向量）
//! HUB_EVAL_WRITE_BASELINE=1 cargo test --test eval_bench      # 冻结基线
//! HUB_EVAL_ABLATIONS=1 cargo test --test eval_bench -- --nocapture --ignored
//! ```

use async_trait::async_trait;
use chrono::{DateTime, Duration, TimeZone, Utc};
use common::{AccountId, AuthCtx, Result, Scope};
use mem_core::{
    domain::{CognitiveLayer, MemoryEntry, MemoryKind, MemoryScene, MemoryStatus},
    testing::{FixedClock, TestEmbedder},
    AccountScopedStore, Embedder, GrantStore, MemoryHubStore, MemoryStore, Retriever, SearchQuery,
};
use retrieval::HybridRetriever;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use store::InMemoryHubStore;
use uuid::Uuid;

/// 评测固定时钟：任何日期运行结果一致（消除时间炸弹）
fn fixed_now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 19, 0, 0, 0).unwrap()
}

const TOP_K: usize = 10;
/// 硬负例检查深度
const FORBIDDEN_K: usize = 3;

// ============================================================================
// 数据集
// ============================================================================

#[derive(Debug, Clone)]
struct Doc {
    key: String,
    entry: MemoryEntry,
    /// 是否具备嵌入向量（false = 模拟写入时嵌入失败，条目只有词法信号）
    has_embedding: bool,
}

#[derive(Debug, Clone)]
struct EvalQuery {
    id: String,
    /// 样本类别：exact / paraphrase / temporal / evolution / scene / env / negative
    category: &'static str,
    namespace: String,
    text: String,
    scene: Option<MemoryScene>,
    layer: Option<CognitiveLayer>,
    tags: Option<Vec<String>>,
    valid_at: Option<DateTime<Utc>>,
    include_superseded: bool,
    relevant: Vec<String>,
    forbidden: Vec<String>,
}

#[derive(Debug, Default, Clone)]
struct Dataset {
    docs: Vec<Doc>,
    queries: Vec<EvalQuery>,
}

/// 主题定义：一个主题 = 同一事实的多个变体（当前 / 历史 / 改写 / 矛盾 / 跨场景 / 环境）
struct Topic {
    id: &'static str,
    scene: MemoryScene,
    namespace: &'static str,
    layer: CognitiveLayer,
    kind: MemoryKind,
    /// 现行事实
    current: &'static str,
    /// 已失效的旧版本
    outdated: &'static str,
    /// 同义改写（语义相同、用词不同）
    paraphrase: &'static str,
    /// 主题相似但事实矛盾（不得作为正确结果）
    contradiction: &'static str,
    /// 精确查询
    q_exact: &'static str,
    /// 语义改写查询（真实嵌入模型下才有意义）
    q_paraphrase: &'static str,
    /// Working 主题的环境边界（第一个为当前环境）
    envs: &'static [&'static str],
}

fn topics() -> Vec<Topic> {
    use CognitiveLayer as L;
    use MemoryKind as K;
    use MemoryScene as S;
    vec![
        // ===== Chat：用户偏好与基础设施事实 =====
        Topic {
            id: "chat-db-conn",
            scene: S::Chat, namespace: "bench-chat", layer: L::Fact, kind: K::Fact,
            current: "生产 PostgreSQL 数据库最大连接数设置为 200，超出会触发连接拒绝",
            outdated: "生产 PostgreSQL 数据库最大连接数曾设置为 100，已于上一季度调整",
            paraphrase: "线上主库的连接池上限是两百",
            contradiction: "生产 PostgreSQL 数据库最大连接数设置为 50，且不限制连接复用",
            q_exact: "PostgreSQL 最大连接数",
            q_paraphrase: "主库连接池上限是多少",
            envs: &[],
        },
        Topic {
            id: "chat-gateway-timeout",
            scene: S::Chat, namespace: "bench-chat", layer: L::Fact, kind: K::Fact,
            current: "API 网关超时时间设定为 30 秒 (gateway timeout 30s)",
            outdated: "API 网关超时时间曾设定为 10 秒 (gateway timeout 10s)，已放宽",
            paraphrase: "链路入口的等待上限提升到了半分钟",
            contradiction: "API 网关超时时间设定为 5 秒，且失败不重试",
            q_exact: "gateway timeout 30s",
            q_paraphrase: "入口等待超时是多久",
            envs: &[],
        },
        Topic {
            id: "chat-jwt-rotation",
            scene: S::Chat, namespace: "bench-chat", layer: L::Fact, kind: K::Fact,
            current: "Auth 认证服务的 JWT 密钥轮转周期为 7 天 (JWT secret rotation cycle 7 days)",
            outdated: "Auth 认证服务的 JWT 密钥曾每 30 天轮转一次",
            paraphrase: "签名密钥每周更换一次",
            contradiction: "Auth 认证服务不再轮转 JWT 密钥，改为永久有效",
            q_exact: "JWT secret rotation cycle",
            q_paraphrase: "签名密钥多久换一次",
            envs: &[],
        },
        Topic {
            id: "chat-lang-preference",
            scene: S::Chat, namespace: "bench-chat", layer: L::Identity, kind: K::Preference,
            current: "用户偏好简短直接的中文回复，避免客套与重复确认",
            outdated: "用户曾偏好详细解释与逐步说明",
            paraphrase: "答话要短、要直给结论",
            contradiction: "用户偏好冗长的分点长文说明",
            q_exact: "回复风格偏好",
            q_paraphrase: "用户希望我怎么回答问题",
            envs: &[],
        },
        Topic {
            id: "chat-budget",
            scene: S::Chat, namespace: "bench-chat", layer: L::Fact, kind: K::Fact,
            current: "本季度云资源预算上限为每月 3000 元，超出需审批",
            outdated: "上季度云资源预算为每月 5000 元",
            paraphrase: "花销红线是三千块一个月",
            contradiction: "云资源不设预算上限，可自由扩容",
            q_exact: "云资源预算上限",
            q_paraphrase: "每月能花多少钱",
            envs: &[],
        },
        Topic {
            id: "chat-oncall",
            scene: S::Chat, namespace: "bench-chat", layer: L::Knowledge, kind: K::Procedure,
            current: "线上故障响应流程：先止血（回滚或降级），再定位，最后补回归测试",
            outdated: "早期流程要求先定位根因再动手",
            paraphrase: "出事先恢复服务再查原因",
            contradiction: "故障必须先写完整复盘报告再恢复服务",
            q_exact: "线上故障响应流程",
            q_paraphrase: "线上出问题先做什么",
            envs: &[],
        },
        Topic {
            id: "chat-retention",
            scene: S::Chat, namespace: "bench-chat", layer: L::Fact, kind: K::Fact,
            current: "备份快照保留 10 份、最长 90 天，超期自动淘汰",
            outdated: "备份快照曾保留 30 份",
            paraphrase: "快照只留十份三个月的量",
            contradiction: "备份快照永久保留、从不淘汰",
            q_exact: "备份快照保留策略",
            q_paraphrase: "快照会留多久",
            envs: &[],
        },
        Topic {
            id: "chat-region",
            scene: S::Chat, namespace: "bench-chat", layer: L::Fact, kind: K::Fact,
            current: "生产数据库主库部署在上海可用区 A，只读副本在可用区 B",
            outdated: "生产数据库主库曾部署在杭州可用区 B",
            paraphrase: "主库在上海，副本在另一个区",
            contradiction: "生产数据库主库部署在深圳，且没有只读副本",
            q_exact: "数据库主库部署位置",
            q_paraphrase: "主库在哪个地域",
            envs: &[],
        },
        Topic {
            id: "chat-cache",
            scene: S::Chat, namespace: "bench-chat", layer: L::Fact, kind: K::Fact,
            current: "Redis 缓存集群主节点为 redis-master-2.prod",
            outdated: "Redis 缓存集群主节点曾为 redis-master-1.prod",
            paraphrase: "缓存主节点已经切到了二号实例",
            contradiction: "Redis 缓存集群没有主节点，走客户端分片",
            q_exact: "Redis 缓存集群主节点",
            q_paraphrase: "缓存的主节点是哪台",
            envs: &[],
        },
        Topic {
            id: "chat-api-version",
            scene: S::Chat, namespace: "bench-chat", layer: L::Fact, kind: K::Fact,
            current: "对外 REST API 版本策略：破坏性变更必须新开 /v2 前缀并保留 /v1 一年",
            outdated: "早期允许在 /v1 内直接做破坏性变更",
            paraphrase: "对外接口不能原路破坏，要开新版本",
            contradiction: "对外接口允许随时原地修改字段，无需新版本",
            q_exact: "REST API 版本策略",
            q_paraphrase: "接口破坏性变更怎么处理",
            envs: &[],
        },
        // ===== 新增 Chat 主题（补齐到 15 个）=====
        Topic {
            id: "chat-session-ttl",
            scene: S::Chat, namespace: "bench-chat", layer: L::Fact, kind: K::Fact,
            current: "账户登录会话有效期 720 小时（30 天），超期需重新认证",
            outdated: "早期账户登录会话有效期仅 24 小时",
            paraphrase: "登录状态能维持一个月",
            contradiction: "登录会话永久有效、不再过期",
            q_exact: "登录会话有效期",
            q_paraphrase: "登录多久需要重新认证",
            envs: &[],
        },
        Topic {
            id: "chat-invite",
            scene: S::Chat, namespace: "bench-chat", layer: L::Knowledge, kind: K::Procedure,
            current: "新账户注册需要邀请码，邀请码为 8 位随机十六进制字符串",
            outdated: "早期开放自助注册，无需邀请码",
            paraphrase: "注册要凭邀请码才能开户",
            contradiction: "注册完全开放，任何人可自助开户",
            q_exact: "注册邀请码",
            q_paraphrase: "开户需要什么凭证",
            envs: &[],
        },
        Topic {
            id: "chat-rate-limit",
            scene: S::Chat, namespace: "bench-chat", layer: L::Fact, kind: K::Fact,
            current: "MCP 接口对每个 Agent 限速每秒 50 次请求，超出返回限流错误",
            outdated: "早期接口不限速，仅靠反向代理粗粒度限制",
            paraphrase: "每个 agent 每秒最多五十次请求",
            contradiction: "接口已完全取消限流",
            q_exact: "接口限速策略 每秒 50",
            q_paraphrase: "请求太快会被限流吗",
            envs: &[],
        },
        Topic {
            id: "chat-backup-cron",
            scene: S::Chat, namespace: "bench-chat", layer: L::Fact, kind: K::Fact,
            current: "备份任务每天凌晨 3 点执行，保留 10 份快照",
            outdated: "备份任务曾每小时执行一次",
            paraphrase: "快照在每天凌晨三点跑",
            contradiction: "备份任务已停用，改为手工导出",
            q_exact: "备份执行时间 凌晨 3 点",
            q_paraphrase: "备份什么时候跑",
            envs: &[],
        },
        Topic {
            id: "chat-cjk",
            scene: S::Chat, namespace: "bench-chat", layer: L::Knowledge, kind: K::Fact,
            current: "中文检索使用自研 CJK bigram 分词，不依赖数据库分词扩展",
            outdated: "早期中文检索依赖 jieba 分词扩展",
            paraphrase: "中文切词是自己实现的，不装扩展",
            contradiction: "中文检索直接整句匹配，不做分词",
            q_exact: "中文分词方案 bigram",
            q_paraphrase: "中文检索怎么切词",
            envs: &[],
        },
        // ===== 新增 Coding 主题（补齐到 15 个）=====
        Topic {
            id: "code-idempotency",
            scene: S::Coding, namespace: "bench-code", layer: L::Fact, kind: K::Fact,
            current: "写入接口按请求 ID 幂等，24 小时内重复请求直接返回首次结果",
            outdated: "早期重复请求会真实写入两次",
            paraphrase: "同一个请求重复提交只生效一次",
            contradiction: "幂等缓存已移除，请求一律重复执行",
            q_exact: "写入幂等 请求 ID",
            q_paraphrase: "重复提交会不会写两遍",
            envs: &[],
        },
        Topic {
            id: "code-scoped-ns",
            scene: S::Coding, namespace: "bench-code", layer: L::Fact, kind: K::Fact,
            current: "命名空间按账户前缀物理隔离，越权访问在存储层直接拒绝",
            outdated: "早期隔离仅靠应用层校验",
            paraphrase: "隔离用账户前缀落到物理层",
            contradiction: "命名空间改为全局共享，取消账户前缀",
            q_exact: "命名空间物理隔离 账户前缀",
            q_paraphrase: "不同账户之间怎么隔离",
            envs: &[],
        },
        Topic {
            id: "code-jemalloc",
            scene: S::Coding, namespace: "bench-code", layer: L::Fact, kind: K::Fact,
            current: "服务采用 jemalloc 分配器降低内存碎片率",
            outdated: "早期使用系统默认分配器",
            paraphrase: "内存分配器换成了 jemalloc",
            contradiction: "分配器已切回系统默认实现",
            q_exact: "jemalloc 分配器",
            q_paraphrase: "内存分配器用的哪个",
            envs: &[],
        },
        Topic {
            id: "code-stmt-timeout",
            scene: S::Coding, namespace: "bench-code", layer: L::Fact, kind: K::Fact,
            current: "数据库语句超时设置为 30 秒，超时后中断并把错误返回调用方",
            outdated: "早期没有语句超时，慢查询可能长期占用连接",
            paraphrase: "SQL 最多跑三十秒就被中断",
            contradiction: "语句超时已取消，慢查询允许无限等待",
            q_exact: "数据库语句超时 30 秒",
            q_paraphrase: "SQL 跑太久会怎样",
            envs: &[],
        },
        Topic {
            id: "code-redaction",
            scene: S::Coding, namespace: "bench-code", layer: L::Fact, kind: K::Fact,
            current: "日志与代码变更记录对 Token、密钥等凭证自动脱敏",
            outdated: "早期凭证原样落盘，存在泄漏风险",
            paraphrase: "密钥在日志里会被自动打码",
            contradiction: "脱敏只对控制台输出生效，落盘保留明文",
            q_exact: "凭证脱敏策略 Token",
            q_paraphrase: "日志里会不会泄露密钥",
            envs: &[],
        },
        Topic {
            id: "code-cursor",
            scene: S::Coding, namespace: "bench-code", layer: L::Fact, kind: K::Fact,
            current: "列表接口使用游标分页（updated_at + id 复合游标），禁止 offset 分页",
            outdated: "早期列表接口使用 offset 分页",
            paraphrase: "分页改成游标式，不用 offset",
            contradiction: "列表接口已改回 offset 分页",
            q_exact: "游标分页 复合游标",
            q_paraphrase: "列表接口怎么翻页",
            envs: &[],
        },
        Topic {
            id: "code-sql-validate",
            scene: S::Coding, namespace: "bench-code", layer: L::Fact, kind: K::Fact,
            current: "迁移脚本执行前先做 SQL 静态校验，拒绝危险语句",
            outdated: "早期迁移脚本直接执行",
            paraphrase: "迁移 SQL 先过静态校验器",
            contradiction: "静态校验器已移除以提升执行速度",
            q_exact: "迁移脚本静态校验",
            q_paraphrase: "迁移 SQL 会被检查吗",
            envs: &[],
        },
        // ===== 新增 Working 主题（补齐到 30 个，全部带环境边界）=====
        Topic {
            id: "work-kafka-lag",
            scene: S::Working, namespace: "bench-work", layer: L::Knowledge, kind: K::Procedure,
            current: "Kafka 消费积压排查：先看 consumer group lag，再评估扩分区或加消费者",
            outdated: "早期遇到积压直接重启消费者",
            paraphrase: "积压先查 lag 再决定扩分区",
            contradiction: "消费积压无需处理，集群会自行追平",
            q_exact: "Kafka 消费积压排查 lag",
            q_paraphrase: "消息积压怎么查",
            envs: &["kafka:3", "kafka:2"],
        },
        Topic {
            id: "work-es-shard",
            scene: S::Working, namespace: "bench-work", layer: L::Knowledge, kind: K::Procedure,
            current: "ES 分片不均时用 cluster reroute 迁移热点分片，避免重建索引",
            outdated: "早期通过重建索引解决分片倾斜",
            paraphrase: "分片倾斜要手工 reroute",
            contradiction: "分片倾斜属于正常现象，无需处理",
            q_exact: "ES 分片不均 reroute",
            q_paraphrase: "es 分片倾斜怎么办",
            envs: &["es:8", "es:7"],
        },
        Topic {
            id: "work-nginx-502",
            scene: S::Working, namespace: "bench-work", layer: L::Knowledge, kind: K::Procedure,
            current: "网关 502 排查：先看上游健康检查与连接队列，再定位具体上游实例",
            outdated: "早期只会重启网关进程",
            paraphrase: "502 先查上游健康与连接队列",
            contradiction: "502 是客户端问题，与网关无关",
            q_exact: "Nginx 502 排查 连接队列",
            q_paraphrase: "网关报 502 怎么查",
            envs: &["nginx:1.25", "nginx:1.24"],
        },
        Topic {
            id: "work-mysql-lock",
            scene: S::Working, namespace: "bench-work", layer: L::Knowledge, kind: K::Procedure,
            current: "行锁等待用 information_schema.innodb_trx 定位阻塞源再决定 kill 会话",
            outdated: "早期遇到锁等待只能盲目 kill 会话",
            paraphrase: "锁等待先查 innodb_trx 找阻塞源",
            contradiction: "锁等待无需排查，等待即可自动解开",
            q_exact: "MySQL 行锁等待排查 innodb_trx",
            q_paraphrase: "数据库锁住了怎么查",
            envs: &["mysql:8", "mysql:5.7"],
        },
        Topic {
            id: "work-tls-handshake",
            scene: S::Working, namespace: "bench-work", layer: L::Knowledge, kind: K::Procedure,
            current: "TLS 握手失败用 openssl s_client 查看证书链与握手时序",
            outdated: "早期只替换证书不验证链路",
            paraphrase: "握手失败要看证书链与时序",
            contradiction: "握手失败重启服务即可，无需排查",
            q_exact: "TLS 握手失败 openssl s_client",
            q_paraphrase: "证书握手失败怎么办",
            envs: &["os:linux", "os:windows"],
        },
        Topic {
            id: "work-jvm-oom",
            scene: S::Working, namespace: "bench-work", layer: L::Knowledge, kind: K::Procedure,
            current: "JVM OOM 排查：先取 heap dump 分析对象占用，再评估 Xmx 与泄漏",
            outdated: "早期遇到 OOM 直接加大 Xmx",
            paraphrase: "OOM 先看堆转储再调堆大小",
            contradiction: "OOM 只在测试环境出现，生产无需处理",
            q_exact: "JVM OOM 排查 heap dump",
            q_paraphrase: "java 内存溢出怎么查",
            envs: &["jvm:17", "jvm:8"],
        },
        Topic {
            id: "work-inode",
            scene: S::Working, namespace: "bench-work", layer: L::Knowledge, kind: K::Procedure,
            current: "磁盘显示有空间但写入失败时，用 df -i 检查 inode 占用并清理小文件",
            outdated: "早期只看 df -h 可用空间",
            paraphrase: "写不进去要看 inode 而不是剩余空间",
            contradiction: "inode 资源不会耗尽，可忽略",
            q_exact: "inode 耗尽排查 df -i",
            q_paraphrase: "磁盘有空间却写不进去",
            envs: &["os:linux", "os:macos"],
        },
        Topic {
            id: "work-dns-dig",
            scene: S::Working, namespace: "bench-work", layer: L::Knowledge, kind: K::Procedure,
            current: "解析超时先在本机 dig 对比公共 DNS，判断是本地解析器还是权威侧问题",
            outdated: "早期直接改 hosts 绕过解析",
            paraphrase: "解析超时先本地 dig 与公共 DNS 对比",
            contradiction: "DNS 超时无需排查，等待恢复即可",
            q_exact: "DNS 解析超时排查 dig",
            q_paraphrase: "域名解析很慢怎么查",
            envs: &["os:linux", "os:windows"],
        },
        Topic {
            id: "work-pg-bloat",
            scene: S::Working, namespace: "bench-work", layer: L::Knowledge, kind: K::Procedure,
            current: "表膨胀用 pg_stat_user_tables 判断死元组比例，再安排常规 vacuum",
            outdated: "早期靠人工估算表体积",
            paraphrase: "表膨胀看统计视图再排 vacuum",
            contradiction: "vacuum 会锁表，因此全局禁用",
            q_exact: "PostgreSQL 表膨胀 vacuum",
            q_paraphrase: "pg 表越来越大怎么办",
            envs: &["postgres:15", "postgres:14"],
        },
        Topic {
            id: "work-redis-maxmemory",
            scene: S::Working, namespace: "bench-work", layer: L::Knowledge, kind: K::Procedure,
            current: "Redis 内存告警先看 maxmemory-policy 与 bigkey 分布",
            outdated: "早期内存告警直接扩容实例",
            paraphrase: "内存告警先查淘汰策略与 bigkey",
            contradiction: "Redis 内存不会超限，告警可忽略",
            q_exact: "Redis 内存告警 bigkey",
            q_paraphrase: "redis 内存满了怎么办",
            envs: &["redis:7", "redis:6"],
        },
        Topic {
            id: "work-cron-silent",
            scene: S::Working, namespace: "bench-work", layer: L::Knowledge, kind: K::Procedure,
            current: "定时任务静默失败排查：先看退出码与执行用户，再检查日志轮转是否吞掉输出",
            outdated: "早期只确认任务是否注册",
            paraphrase: "定时任务失败先看退出码与日志轮转",
            contradiction: "cron 不存在静默失败，任务必然执行",
            q_exact: "定时任务静默失败排查 cron",
            q_paraphrase: "cron 没执行怎么查",
            envs: &["os:linux", "os:macos"],
        },
        Topic {
            id: "work-crashloop",
            scene: S::Working, namespace: "bench-work", layer: L::Knowledge, kind: K::Procedure,
            current: "CrashLoopBackOff 排查：先看上一次容器日志与退出码，再确认探针配置",
            outdated: "早期只看当前容器日志",
            paraphrase: "崩溃循环要看上一次日志与退出码",
            contradiction: "崩溃循环重建 Pod 即可，无需分析",
            q_exact: "CrashLoopBackOff 排查 退出码",
            q_paraphrase: "pod 一直重启怎么查",
            envs: &["k8s:prod", "k8s:staging"],
        },
        Topic {
            id: "work-conn-pool",
            scene: S::Working, namespace: "bench-work", layer: L::Knowledge, kind: K::Procedure,
            current: "连接池耗尽排查：先看活跃连接与慢查询占用，再评估池上限",
            outdated: "早期遇到耗尽直接加大池上限",
            paraphrase: "池子耗尽先看慢查询占用",
            contradiction: "连接池无需监控，耗尽会自动恢复",
            q_exact: "连接池耗尽排查 慢查询",
            q_paraphrase: "连接池满了怎么办",
            envs: &["os:linux", "os:windows"],
        },
        Topic {
            id: "work-log-rotate",
            scene: S::Working, namespace: "bench-work", layer: L::Knowledge, kind: K::Procedure,
            current: "日志暴涨先用 du 定位目录，再调整轮转份数与压缩策略",
            outdated: "早期直接删除历史日志",
            paraphrase: "日志暴涨先定位目录再调轮转",
            contradiction: "日志不需要轮转，磁盘足够大",
            q_exact: "日志暴涨定位 轮转策略",
            q_paraphrase: "日志占满磁盘怎么办",
            envs: &["os:linux", "os:macos"],
        },
        Topic {
            id: "work-acme-renew",
            scene: S::Working, namespace: "bench-work", layer: L::Knowledge, kind: K::Procedure,
            current: "证书自动续期失败：先检查 ACME 校验路径与 DNS 记录是否可达",
            outdated: "早期续期失败靠手工替换证书",
            paraphrase: "续期失败先查 ACME 校验路径",
            contradiction: "续期失败可以忽略，证书不会过期",
            q_exact: "证书续期失败 ACME 校验",
            q_paraphrase: "自动续期失败怎么办",
            envs: &["os:linux", "os:windows"],
        },
        Topic {
            id: "work-backup-drill",
            scene: S::Working, namespace: "bench-work", layer: L::Knowledge, kind: K::Procedure,
            current: "备份可用性验证必须实际恢复一次并核对校验和，不能只看文件存在",
            outdated: "早期仅确认备份文件已生成",
            paraphrase: "备份要真正恢复一次才算验证通过",
            contradiction: "备份文件存在即视为可用，无需演练",
            q_exact: "备份可用性验证 校验和",
            q_paraphrase: "怎么确认备份能用",
            envs: &["env:prod", "env:dev"],
        },
        Topic {
            id: "work-index-bloat",
            scene: S::Working, namespace: "bench-work", layer: L::Knowledge, kind: K::Procedure,
            current: "索引膨胀用 pgstatindex 判断叶页利用率，再决定是否重建",
            outdated: "早期定期全量重建所有索引",
            paraphrase: "索引膨胀看 pgstatindex 再重建",
            contradiction: "索引不会膨胀，无需维护",
            q_exact: "索引膨胀 重建 pgstatindex",
            q_paraphrase: "索引越来越大怎么办",
            envs: &["postgres:15", "postgres:14"],
        },
        Topic {
            id: "work-retry-dlq",
            scene: S::Working, namespace: "bench-work", layer: L::Knowledge, kind: K::Procedure,
            current: "消息消费重试采用指数退避，超过阈值进入死信队列人工处理",
            outdated: "早期失败消息无限重试",
            paraphrase: "重试走指数退避，超限进死信",
            contradiction: "失败消息直接丢弃，不保留死信",
            q_exact: "消息重试 死信队列 退避",
            q_paraphrase: "消费失败会一直重试吗",
            envs: &["kafka:3", "rabbitmq:3"],
        },
        Topic {
            id: "work-clock-skew",
            scene: S::Working, namespace: "bench-work", layer: L::Knowledge, kind: K::Procedure,
            current: "时钟漂移会导致证书与签名校验失败，用 chrony 校准并监控偏移量",
            outdated: "早期忽略服务器时间同步",
            paraphrase: "时间漂移会让校验失败，用 chrony 校准",
            contradiction: "服务器时间不需要同步，误差无关紧要",
            q_exact: "时钟漂移 chrony 校准",
            q_paraphrase: "服务器时间不准怎么办",
            envs: &["os:linux", "os:windows"],
        },
        Topic {
            id: "work-replica-lag",
            scene: S::Working, namespace: "bench-work", layer: L::Knowledge, kind: K::Procedure,
            current: "主从延迟用 pg_stat_replication 观察，超过阈值时按业务降级读主库",
            outdated: "早期不监控复制延迟",
            paraphrase: "主从延迟看复制视图，超标就降级读主",
            contradiction: "主从延迟不影响业务，无需处理",
            q_exact: "主从延迟 pg_stat_replication",
            q_paraphrase: "从库数据落后怎么办",
            envs: &["postgres:15", "mysql:8"],
        },
        Topic {
            id: "work-trace-sampling",
            scene: S::Working, namespace: "bench-work", layer: L::Knowledge, kind: K::Procedure,
            current: "链路追踪按 1% 采样上报，错误链路全量保留以便定位",
            outdated: "早期全量上报导致成本过高",
            paraphrase: "追踪采样百分之一，错误全留",
            contradiction: "链路追踪已完全关闭",
            q_exact: "链路追踪采样策略",
            q_paraphrase: "追踪成本太高怎么降",
            envs: &["os:linux", "k8s:prod"],
        },
        Topic {
            id: "work-flaky-quarantine",
            scene: S::Working, namespace: "bench-work", layer: L::Knowledge, kind: K::Procedure,
            current: "用例偶发失败先隔离并重跑三次确认，再决定修用例还是修代码",
            outdated: "早期把偶发失败直接标记忽略",
            paraphrase: "偶发失败先隔离重跑再定位",
            contradiction: "偶发失败可长期忽略，不影响发布",
            q_exact: "用例偶发失败 隔离重跑",
            q_paraphrase: "CI 不稳定怎么办",
            envs: &["os:linux", "os:macos"],
        },

        // ===== Working：任务经验与环境边界 =====
        Topic {
            id: "work-jemalloc",
            scene: S::Working, namespace: "bench-work", layer: L::Knowledge, kind: K::Skill,
            current: "在 Linux ARM64 环境下交叉编译 jemalloc 需注入 CC_aarch64_unknown_linux_gnu 环境变量",
            outdated: "早期在 x86 环境下编译 jemalloc 无需交叉编译参数",
            paraphrase: "arm64 上编 jemalloc 得先给交叉工具链变量",
            contradiction: "jemalloc 在任何架构下都不需要交叉编译参数",
            q_exact: "Linux ARM64 jemalloc 交叉编译",
            q_paraphrase: "arm64 编 jemalloc 报错怎么办",
            envs: &["os:linux-arm64", "os:macos-arm64"],
        },
        Topic {
            id: "work-redis-pool",
            scene: S::Working, namespace: "bench-work", layer: L::Knowledge, kind: K::Procedure,
            current: "排查生产环境 Redis 连接池泄漏需通过 netstat 结合 redis-cli client list 统计空闲连接",
            outdated: "早期用 info clients 单指标判断连接泄漏",
            paraphrase: "用 netstat 配合 client list 找没释放的 redis 连接",
            contradiction: "Redis 连接数不需要统计，重启实例即可视为修复",
            q_exact: "Redis 连接池泄漏排查 netstat",
            q_paraphrase: "redis 连接不释放怎么查",
            envs: &["os:linux", "os:macos"],
        },
        Topic {
            id: "work-pg-slow",
            scene: S::Working, namespace: "bench-work", layer: L::Knowledge, kind: K::Procedure,
            current: "PostgreSQL 慢查询定位使用 pg_stat_statements 扩展按总耗时排序",
            outdated: "早期依赖 log_min_duration_statement 逐条翻日志",
            paraphrase: "用 pg_stat_statements 找最耗时的 SQL",
            contradiction: "PostgreSQL 无法统计慢查询，只能靠业务侧埋点",
            q_exact: "PostgreSQL 慢查询定位 pg_stat_statements",
            q_paraphrase: "pg 慢 SQL 怎么定位",
            envs: &["os:linux", "os:windows"],
        },
        Topic {
            id: "work-disk-full",
            scene: S::Working, namespace: "bench-work", layer: L::Knowledge, kind: K::Procedure,
            current: "磁盘写满应急：先用 du -x --max-depth=1 定位大目录，再清理日志与临时卷",
            outdated: "早期直接删除 /var/log 下全部文件导致审计断档",
            paraphrase: "盘满了先找大目录再清日志",
            contradiction: "磁盘写满直接格式化数据盘即可恢复",
            q_exact: "磁盘写满应急处理 du",
            q_paraphrase: "服务器磁盘满了怎么办",
            envs: &["os:linux", "os:windows"],
        },
        Topic {
            id: "work-dns",
            scene: S::Working, namespace: "bench-work", layer: L::Knowledge, kind: K::Skill,
            current: "容器内 DNS 解析异常时用 nslookup 与 tcpdump 抓 53 端口确认 CoreDNS 丢包",
            outdated: "早期只重启 kube-dns 不抓包验证",
            paraphrase: "容器域名解析失败要抓 53 端口看丢包",
            contradiction: "容器 DNS 永远不需要排查，重建 Pod 即可",
            q_exact: "容器 DNS 解析异常排查 tcpdump",
            q_paraphrase: "pod 解析不了域名怎么查",
            envs: &["os:linux", "rancher:k8s"],
        },
        Topic {
            id: "work-oom",
            scene: S::Working, namespace: "bench-work", layer: L::Knowledge, kind: K::Procedure,
            current: "服务 OOM 排查：先看 cgroup memory.max 与 oom_score_adj，再用 heaptrack 验证增长点",
            outdated: "早期仅凭 free -m 判断内存泄漏",
            paraphrase: "被 OOM kill 要结合 cgroup 限制和 heaptrack 分析",
            contradiction: "OOM 只需调大 JVM 堆参数即可根治",
            q_exact: "服务 OOM 排查 cgroup heaptrack",
            q_paraphrase: "服务总被杀是内存问题吗",
            envs: &["os:linux", "os:macos"],
        },
        Topic {
            id: "work-cert",
            scene: S::Working, namespace: "bench-work", layer: L::Knowledge, kind: K::Procedure,
            current: "证书过期应急：用 openssl s_client -connect 校验链，再按 90 天周期接入自动续期",
            outdated: "早期手工替换证书且不设提醒",
            paraphrase: "证书过期先用 openssl 验链再上自动续期",
            contradiction: "证书过期不影响服务，可忽略告警",
            q_exact: "证书过期应急 openssl s_client",
            q_paraphrase: "https 证书到期怎么办",
            envs: &["os:linux", "os:windows"],
        },
        Topic {
            id: "work-migration",
            scene: S::Working, namespace: "bench-work", layer: L::Knowledge, kind: K::Procedure,
            current: "在线数据迁移使用双写 + 影子表校验，回滚窗口保留 24 小时",
            outdated: "早期停机迁移，回滚窗口 0 小时",
            paraphrase: "迁移要双写并留一天回滚窗口",
            contradiction: "数据迁移不需要回滚方案，直接切换即可",
            q_exact: "在线数据迁移双写 影子表",
            q_paraphrase: "数据迁移怎么做才安全",
            envs: &["env:prod", "env:staging"],
        },
        // ===== Coding：代码演进与修订链 =====
        Topic {
            id: "code-hybrid",
            scene: S::Coding, namespace: "bench-code", layer: L::Fact, kind: K::Fact,
            current: "重构 HybridRetriever：FTS 与向量双路召回后做五维加权融合再 MMR 去冗余",
            outdated: "早期 HybridRetriever 只做向量召回并按更新时间排序",
            paraphrase: "把两路召回结果加权合并后再去重排序",
            contradiction: "HybridRetriever 只保留向量路，删除 FTS 召回",
            q_exact: "HybridRetriever MMR 重排",
            q_paraphrase: "召回结果怎么合并排序",
            envs: &[],
        },
        Topic {
            id: "code-fk",
            scene: S::Coding, namespace: "bench-code", layer: L::Fact, kind: K::Fact,
            current: "实现 Composite Foreign Key 迁移以强化跨租户命名空间外键约束",
            outdated: "早期仅靠应用层校验命名空间归属",
            paraphrase: "用复合外键把租户隔离落到数据库层",
            contradiction: "取消数据库外键约束，全部改由应用层校验",
            q_exact: "Composite Foreign Key 跨租户命名空间",
            q_paraphrase: "租户隔离怎么做在库层",
            envs: &[],
        },
        Topic {
            id: "code-tools",
            scene: S::Coding, namespace: "bench-code", layer: L::Fact, kind: K::Fact,
            current: "扩展 MCP ToolDispatcher 支持 9 个专用认知增强工具分发",
            outdated: "早期 ToolDispatcher 只分发 18 个基础记忆工具",
            paraphrase: "给工具箱加了认知类工具的分发分支",
            contradiction: "MCP ToolDispatcher 已废弃，改为单一 RPC 入口",
            q_exact: "MCP ToolDispatcher 认知增强工具",
            q_paraphrase: "认知工具是怎么分发出去的",
            envs: &[],
        },
        Topic {
            id: "code-tokenizer",
            scene: S::Coding, namespace: "bench-code", layer: L::Fact, kind: K::Fact,
            current: "实现零依赖 CJK bigram 分词器，中文检索不再依赖数据库全文扩展",
            outdated: "早期中文检索依赖 pg_jieba 扩展",
            paraphrase: "中文切词自己实现，不装外部扩展",
            contradiction: "中文检索改用全文搜索引擎，放弃数据库内实现",
            q_exact: "零依赖 CJK bigram 分词器",
            q_paraphrase: "中文分词是怎么做的",
            envs: &[],
        },
        Topic {
            id: "code-backup",
            scene: S::Coding, namespace: "bench-code", layer: L::Fact, kind: K::Fact,
            current: "备份恢复实现 Replace 模式：恢复前自动保存当前快照并校验校验和",
            outdated: "早期恢复直接覆盖，无前置备份",
            paraphrase: "恢复前先自保一份再覆盖",
            contradiction: "恢复操作删除前置校验与备份以提升速度",
            q_exact: "备份恢复 Replace 模式 校验和",
            q_paraphrase: "恢复会不会先备份",
            envs: &[],
        },
        Topic {
            id: "code-ratelimit",
            scene: S::Coding, namespace: "bench-code", layer: L::Fact, kind: K::Fact,
            current: "为 MCP 传输层加入每 Agent 每秒 50 次的令牌桶限流与 256KB 请求体上限",
            outdated: "早期不做限流，仅靠反向代理粗粒度限制",
            paraphrase: "给每个 agent 加了限速和体积上限",
            contradiction: "限流与体积上限全部移除，交由客户端自律",
            q_exact: "MCP 传输层令牌桶限流",
            q_paraphrase: "请求限速是怎么做的",
            envs: &[],
        },
        Topic {
            id: "code-migration-split",
            scene: S::Coding, namespace: "bench-code", layer: L::Fact, kind: K::Fact,
            current: "迁移语句切分感知 dollar-quoted 块，修复全新库部署失败",
            outdated: "早期按分号切分迁移脚本，遇到函数体即失败",
            paraphrase: "修了迁移脚本按分号切分导致建库失败的问题",
            contradiction: "迁移不再拆分语句，改为整文件执行",
            q_exact: "迁移语句切分 dollar-quoted",
            q_paraphrase: "全新库部署失败是怎么修的",
            envs: &[],
        },
        Topic {
            id: "code-backoff",
            scene: S::Coding, namespace: "bench-code", layer: L::Fact, kind: K::Fact,
            current: "经验反馈闭环：负反馈只下调反馈分，累计低于 0.3 自动标记 Deprecated",
            outdated: "早期负反馈同时下调置信度与反馈分",
            paraphrase: "差评只扣一个分数，扣到跌线就弃用",
            contradiction: "负反馈立即物理删除经验条目",
            q_exact: "经验反馈闭环 Deprecated 阈值",
            q_paraphrase: "经验被差评后会怎样",
            envs: &[],
        },
    ]
}

fn build_dataset() -> Dataset {
    let now = fixed_now();
    let mut ds = Dataset::default();
    let mut doc_seq = 0usize;

    let push_doc = |ds: &mut Dataset,
                    seq: &mut usize,
                    key: String,
                    ns: &str,
                    content: &str,
                    scene: MemoryScene,
                    layer: CognitiveLayer,
                    kind: MemoryKind,
                    valid_from: Option<DateTime<Utc>>,
                    valid_until: Option<DateTime<Utc>>,
                    occurred_at: Option<DateTime<Utc>>,
                    tags: Vec<String>,
                    has_embedding: bool| {
        *seq += 1;
        ds.docs.push(Doc {
            key,
            has_embedding,
            entry: MemoryEntry {
                id: Uuid::from_u128(*seq as u128 + 1),
                namespace: ns.to_string(),
                key: None,
                kind,
                content: content.to_string(),
                tags,
                metadata: json!({}),
                embedding: None,
                embedding_model: None,
                source_agent: "bench-agent".into(),
                confidence: 1.0,
                status: MemoryStatus::Active,
                version: 1,
                expires_at: None,
                merged_into: None,
                created_at: now - Duration::days(120),
                updated_at: now - Duration::days(5),
                scene: Some(scene),
                layer: Some(layer),
                valid_from,
                valid_until,
                occurred_at,
            },
        });
    };

    for t in topics() {
        // 现行事实：90 天前生效，无失效时间
        push_doc(
            &mut ds,
            &mut doc_seq,
            format!("{}-current", t.id),
            t.namespace,
            t.current,
            t.scene,
            t.layer,
            t.kind,
            Some(now - Duration::days(90)),
            None,
            Some(now - Duration::days(3)),
            vec![format!("topic:{}", t.id)],
            true,
        );
        // 历史版本：400 天前生效、90 天前失效
        push_doc(
            &mut ds,
            &mut doc_seq,
            format!("{}-outdated", t.id),
            t.namespace,
            t.outdated,
            t.scene,
            t.layer,
            t.kind,
            Some(now - Duration::days(400)),
            Some(now - Duration::days(90)),
            Some(now - Duration::days(300)),
            vec![format!("topic:{}", t.id), "superseded".to_string()],
            true,
        );
        // 同义改写：与现行事实同效
        push_doc(
            &mut ds,
            &mut doc_seq,
            format!("{}-paraphrase", t.id),
            t.namespace,
            t.paraphrase,
            t.scene,
            t.layer,
            t.kind,
            Some(now - Duration::days(90)),
            None,
            Some(now - Duration::days(10)),
            vec![format!("topic:{}", t.id)],
            true,
        );
        // 硬负例：同主题但事实矛盾
        push_doc(
            &mut ds,
            &mut doc_seq,
            format!("{}-contradiction", t.id),
            t.namespace,
            t.contradiction,
            t.scene,
            t.layer,
            t.kind,
            Some(now - Duration::days(90)),
            None,
            Some(now - Duration::days(4)),
            vec!["unverified".to_string()],
            true,
        );
        // 硬负例：同措辞跨场景干扰
        let cross_scene = match t.scene {
            MemoryScene::Chat => MemoryScene::Coding,
            MemoryScene::Working => MemoryScene::Chat,
            MemoryScene::Coding => MemoryScene::Working,
        };
        push_doc(
            &mut ds,
            &mut doc_seq,
            format!("{}-cross-scene", t.id),
            t.namespace,
            t.current,
            cross_scene,
            t.layer,
            t.kind,
            Some(now - Duration::days(90)),
            None,
            Some(now - Duration::days(2)),
            vec![format!("topic:{}", t.id)],
            true,
        );
        // Working 主题：环境边界不同的同任务版本
        if !t.envs.is_empty() {
            let env_note = format!("【适用环境 {}】{}", t.envs[0], t.current);
            push_doc(
                &mut ds,
                &mut doc_seq,
                format!("{}-env-primary", t.id),
                t.namespace,
                &env_note,
                t.scene,
                t.layer,
                t.kind,
                Some(now - Duration::days(90)),
                None,
                Some(now - Duration::days(3)),
                vec![format!("topic:{}", t.id), format!("env:{}", t.envs[0])],
                true,
            );
            if t.envs.len() > 1 {
                let other = format!(
                    "【适用环境 {}】{}（不同环境的等价做法，需按环境替换工具链）",
                    t.envs[1], t.paraphrase
                );
                push_doc(
                    &mut ds,
                    &mut doc_seq,
                    format!("{}-env-other", t.id),
                    t.namespace,
                    &other,
                    t.scene,
                    t.layer,
                    t.kind,
                    Some(now - Duration::days(90)),
                    None,
                    Some(now - Duration::days(3)),
                    vec![format!("topic:{}", t.id), format!("env:{}", t.envs[1])],
                    true,
                );
            }
        }

        // ===== 查询 =====
        let ns = t.namespace.to_string();
        ds.queries.push(EvalQuery {
            id: format!("{}-exact", t.id),
            category: "exact",
            namespace: ns.clone(),
            text: t.q_exact.to_string(),
            scene: Some(t.scene),
            layer: None,
            tags: None,
            valid_at: None,
            include_superseded: false,
            relevant: vec![format!("{}-current", t.id), format!("{}-paraphrase", t.id)],
            forbidden: vec![
                format!("{}-contradiction", t.id),
                format!("{}-cross-scene", t.id),
            ],
        });
        ds.queries.push(EvalQuery {
            id: format!("{}-paraphrase", t.id),
            category: "paraphrase",
            namespace: ns.clone(),
            text: t.q_paraphrase.to_string(),
            scene: Some(t.scene),
            layer: None,
            tags: None,
            valid_at: None,
            include_superseded: false,
            relevant: vec![format!("{}-current", t.id), format!("{}-paraphrase", t.id)],
            forbidden: vec![format!("{}-contradiction", t.id)],
        });
        ds.queries.push(EvalQuery {
            id: format!("{}-temporal", t.id),
            category: "temporal",
            namespace: ns.clone(),
            text: t.q_exact.to_string(),
            scene: Some(t.scene),
            layer: None,
            tags: None,
            valid_at: Some(now - Duration::days(200)),
            include_superseded: false,
            relevant: vec![format!("{}-outdated", t.id)],
            forbidden: vec![
                format!("{}-current", t.id),
                format!("{}-contradiction", t.id),
            ],
        });
        ds.queries.push(EvalQuery {
            id: format!("{}-evolution", t.id),
            category: "evolution",
            namespace: ns.clone(),
            text: t.q_exact.to_string(),
            scene: Some(t.scene),
            layer: None,
            tags: None,
            valid_at: None,
            include_superseded: true,
            relevant: vec![format!("{}-current", t.id), format!("{}-outdated", t.id)],
            forbidden: vec![format!("{}-contradiction", t.id)],
        });
        ds.queries.push(EvalQuery {
            id: format!("{}-scene", t.id),
            category: "scene",
            namespace: ns.clone(),
            text: t.q_exact.to_string(),
            scene: Some(t.scene),
            layer: None,
            tags: None,
            valid_at: None,
            include_superseded: false,
            relevant: vec![format!("{}-current", t.id)],
            forbidden: vec![format!("{}-cross-scene", t.id)],
        });
        ds.queries.push(EvalQuery {
            id: format!("{}-tags", t.id),
            category: "tags",
            namespace: ns.clone(),
            text: t.q_exact.to_string(),
            scene: Some(t.scene),
            layer: None,
            tags: Some(vec![format!("topic:{}", t.id)]),
            valid_at: None,
            include_superseded: false,
            relevant: vec![format!("{}-current", t.id), format!("{}-paraphrase", t.id)],
            // 矛盾条目没有该主题标签：tags 过滤必须把它挡在结果之外
            forbidden: vec![format!("{}-contradiction", t.id)],
        });
        ds.queries.push(EvalQuery {
            id: format!("{}-layer", t.id),
            category: "layer",
            namespace: ns.clone(),
            text: t.q_exact.to_string(),
            scene: Some(t.scene),
            layer: Some(t.layer),
            tags: None,
            valid_at: None,
            include_superseded: false,
            relevant: vec![format!("{}-current", t.id), format!("{}-paraphrase", t.id)],
            forbidden: vec![format!("{}-contradiction", t.id)],
        });
        if !t.envs.is_empty() {
            // 匹配当前环境：目标是对应环境边界的主条目
            ds.queries.push(EvalQuery {
                id: format!("{}-env", t.id),
                category: "env",
                namespace: ns.clone(),
                text: format!("{} 【环境 {}】", t.q_exact, t.envs[0]),
                scene: Some(t.scene),
                layer: None,
                tags: None,
                valid_at: None,
                include_superseded: false,
                relevant: vec![format!("{}-env-primary", t.id)],
                forbidden: vec![format!("{}-contradiction", t.id)],
            });
            if t.envs.len() > 1 {
                // 另一个环境：memory_search 目前没有结构化 env 参数，
                // 环境区分只能靠词法（环境标记出现在正文中）
                ds.queries.push(EvalQuery {
                    id: format!("{}-env-alt", t.id),
                    category: "env",
                    namespace: ns.clone(),
                    text: format!("{} 【环境 {}】", t.q_exact, t.envs[1]),
                    scene: Some(t.scene),
                    layer: None,
                    tags: None,
                    valid_at: None,
                    include_superseded: false,
                    relevant: vec![format!("{}-env-other", t.id)],
                    forbidden: vec![],
                });
            }
        }
    }

    // ===== 噪声文档：与所有查询主题无关，但随机分布在三个命名空间 =====
    for i in 0..60 {
        let ns = ["bench-chat", "bench-work", "bench-code"][i % 3];
        let scene = match ns {
            "bench-chat" => MemoryScene::Chat,
            "bench-work" => MemoryScene::Working,
            _ => MemoryScene::Coding,
        };
        let content = format!(
            "环境标识 noise-{} 的例行巡检记录：CPU 温度 {} 度、机房湿度 {}%，无异常事件",
            i,
            40 + (i % 15),
            30 + (i % 40)
        );
        push_doc(
            &mut ds,
            &mut doc_seq,
            format!("noise-{}", i),
            ns,
            &content,
            scene,
            CognitiveLayer::Raw,
            MemoryKind::Note,
            Some(now - Duration::days(120)),
            None,
            Some(now - Duration::days(60 + (i as i64 % 30))),
            vec!["noise".to_string()],
            true,
        );
    }

    // ===== 缺嵌入条目（写入时嵌入失败）：必须仍能靠词法信号召回 =====
    // 20 个离线补录主题 × 3 类事项 = 60 条无可检索向量的条目，各自配一条查询
    const OFFLINE_TOPICS: [&str; 20] = [
        "机房巡检发现的网线松动",
        "客户邮件确认的对账口径",
        "值班交接留下的口头约定",
        "供应商合同里的赔付条款",
        "安全审计发现的弱口令",
        "监控告警的抑制规则",
        "灰度名单的评估标准",
        "客户投诉的升级路径",
        "灾备演练的切换顺序",
        "成本分摊的归属规则",
        "资产盘点的标签规范",
        "发布窗口的冻结时段",
        "数据保留的删除策略",
        "接口废弃的下线流程",
        "权限申请的审批链路",
        "变更评审的回滚条件",
        "故障复盘的归档要求",
        "容量规划的评估周期",
        "证书清单的续期责任人",
        "供应商准入的资质清单",
    ];
    const OFFLINE_ITEMS: [&str; 3] = ["处置记录", "核对结论", "交接备注"];
    for (t_idx, topic) in OFFLINE_TOPICS.iter().enumerate() {
        for (i_idx, item) in OFFLINE_ITEMS.iter().enumerate() {
            let key = format!("no-embed-{}-{}", t_idx, i_idx);
            let content = format!("{}：{item}（离线补录，未经向量化）", topic);
            push_doc(
                &mut ds,
                &mut doc_seq,
                key.clone(),
                "bench-chat",
                &content,
                MemoryScene::Chat,
                CognitiveLayer::Fact,
                MemoryKind::Fact,
                Some(now - Duration::days(90)),
                None,
                Some(now - Duration::days(7)),
                vec!["topic:no-embed".to_string()],
                false,
            );
            ds.queries.push(EvalQuery {
                id: format!("{}-lexical", key),
                category: "no_embedding",
                namespace: "bench-chat".to_string(),
                text: format!("{topic} {item}"),
                scene: Some(MemoryScene::Chat),
                layer: None,
                tags: None,
                valid_at: None,
                include_superseded: false,
                relevant: vec![key.clone()],
                forbidden: vec![],
            });
        }
    }

    // ===== 负控制查询：期望零命中（与语料完全无关的领域，20 × 3 = 60 条）=====
    const UNRELATED_SUBJECTS: [&str; 20] = [
        "火星殖民基地的氧气循环",
        "大熊猫繁育基地的饲养排班",
        "深海潜水器的耐压壳焊接",
        "热带雨林监测站的供电",
        "极地科考站的供暖",
        "沙漠光伏电站的清洗",
        "远洋渔船的冷链",
        "高原天文台的镜面镀膜",
        "古建筑彩绘的修复",
        "博物馆恒温展柜的湿度",
        "赛马场的跑道排水",
        "咖啡庄园的日晒发酵",
        "蜂场的越冬保温",
        "盐湖提锂的蒸发池",
        "跨海大桥的伸缩缝",
        "山地索道的钢缆探伤",
        "花房的补光周期",
        "冰川钻孔的取芯",
        "舞台机械的液压系统",
        "海龟保护区的沙滩温度",
    ];
    const UNRELATED_ITEMS: [&str; 3] = ["巡检记录", "工艺参数", "排班表"];
    for (s_idx, subject) in UNRELATED_SUBJECTS.iter().enumerate() {
        for (i_idx, item) in UNRELATED_ITEMS.iter().enumerate() {
            let ns = ["bench-chat", "bench-work", "bench-code"][(s_idx + i_idx) % 3];
            ds.queries.push(EvalQuery {
                id: format!("neg-{s_idx}-{i_idx}"),
                category: "negative",
                namespace: ns.to_string(),
                text: format!("{subject} {item}"),
                scene: None,
                layer: None,
                tags: None,
                valid_at: None,
                include_superseded: false,
                relevant: vec![],
                forbidden: vec![],
            });
        }
    }

    ds
}

// ============================================================================
// 嵌入器：真实模型（可选）+ 磁盘缓存
// ============================================================================

/// 带磁盘缓存的嵌入器包装：首次运行调用真实模型，之后离线可复现
struct CachedEmbedder {
    inner: Arc<dyn Embedder>,
    cache_path: std::path::PathBuf,
    cache: Mutex<HashMap<String, Vec<f32>>>,
}

impl CachedEmbedder {
    fn new(inner: Arc<dyn Embedder>, cache_path: std::path::PathBuf) -> Self {
        let cache = std::fs::read_to_string(&cache_path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        Self {
            inner,
            cache_path,
            cache: Mutex::new(cache),
        }
    }

    fn flush(&self) {
        let cache = self.cache.lock().unwrap();
        if let Ok(raw) = serde_json::to_string(&*cache) {
            let _ = std::fs::write(&self.cache_path, raw);
        }
    }
}

#[async_trait]
impl Embedder for CachedEmbedder {
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    fn dim(&self) -> usize {
        self.inner.dim()
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let missing: Vec<String> = {
            let cache = self.cache.lock().unwrap();
            texts
                .iter()
                .filter(|t| !cache.contains_key(t.as_str()))
                .cloned()
                .collect()
        };
        if !missing.is_empty() {
            let fetched = self.inner.embed(&missing).await?;
            let mut cache = self.cache.lock().unwrap();
            for (text, vec) in missing.into_iter().zip(fetched) {
                cache.insert(text, vec);
            }
        }
        let cache = self.cache.lock().unwrap();
        Ok(texts
            .iter()
            .map(|t| cache.get(t.as_str()).cloned().unwrap_or_default())
            .collect())
    }
}

/// 回归：显式配置了真实嵌入端点、且要求语义时，端点不可达必须让评测
/// **显式失败**，而不是回落到伪向量后继续产出"看起来跑过了"的结果。
///
/// 注意 `HttpEmbedder::new` 只构建 HTTP 客户端、不发起连接，所以"配置了端点"
/// 不等于"端点可用"——真正的失败发生在首次 embed 调用。
///
/// 这里用**子进程**隔离环境变量：`std::env::set_var` 是进程级全局状态，
/// 在同进程内设置会污染并行运行的其他评测用例（实测会让 bench 门禁连到
/// 不可达端点而失败）。子进程让该场景自成一体，不与其他测试共享环境。
#[test]
fn unreachable_configured_endpoint_fails_when_semantics_required() {
    let exe = std::env::current_exe().expect("当前测试二进制路径");
    let output = std::process::Command::new(&exe)
        .args([
            "--exact",
            "unreachable_endpoint_probe_inner",
            "--ignored",
            "--nocapture",
        ])
        .env(
            "HUB_EVAL_EMBED_ENDPOINT",
            "http://127.0.0.1:1/v1/embeddings",
        )
        .env("HUB_EVAL_REQUIRE_SEMANTIC", "1")
        .env(
            "HUB_EVAL_EMBED_CACHE",
            "/tmp/mh_eval_unreachable_probe.json",
        )
        .output()
        .expect("无法启动子进程");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{stdout}{stderr}");
    let _ = std::fs::remove_file("/tmp/mh_eval_unreachable_probe.json");

    assert!(
        combined.contains("PROBE_RESULT: explicit_failure"),
        "不可达端点必须让 embed 显式失败；子进程输出：\n{combined}"
    );
}

/// 子进程内部探针：验证"配置了真实端点 + 要求语义"时不会静默回落到伪向量。
#[tokio::test]
#[ignore = "由 unreachable_configured_endpoint_fails_when_semantics_required 在子进程中触发"]
async fn unreachable_endpoint_probe_inner() {
    let (embedder, _cached, label) = eval_embedder();
    let semantic_ready = embedder.provides_semantic_signal();
    let embed_result = embedder.embed(&["探测文本".to_string()]).await;
    if semantic_ready && embed_result.is_err() {
        println!("PROBE_RESULT: explicit_failure");
    } else {
        println!("PROBE_RESULT: silent_fallback({label})");
    }
}

fn eval_embedder() -> (Arc<dyn Embedder>, Option<Arc<CachedEmbedder>>, String) {
    if let Ok(endpoint) = std::env::var("HUB_EVAL_EMBED_ENDPOINT") {
        if !endpoint.trim().is_empty() {
            let model = std::env::var("HUB_EVAL_EMBED_MODEL").unwrap_or_else(|_| "bge-m3".into());
            let dim: usize = std::env::var("HUB_EVAL_EMBED_DIM")
                .ok()
                .and_then(|d| d.parse().ok())
                .unwrap_or(1024);
            let api_key = std::env::var("HUB_EVAL_EMBED_API_KEY").ok();
            match embed::HttpEmbedder::new(endpoint, model.clone(), dim, api_key) {
                Ok(http) => {
                    let cache_path = std::env::var("HUB_EVAL_EMBED_CACHE")
                        .map(std::path::PathBuf::from)
                        .unwrap_or_else(|_| {
                            std::env::temp_dir()
                                .join(format!("mh_eval_embed_cache_{model}_{dim}.json"))
                        });
                    let cached = Arc::new(CachedEmbedder::new(
                        Arc::new(http) as Arc<dyn Embedder>,
                        cache_path.clone(),
                    ));
                    let model_label = format!("{model}（HTTP，缓存 {}）", cache_path.display());
                    return (cached.clone(), Some(cached), model_label);
                }
                Err(e) => {
                    eprintln!("[bench] 真实嵌入器初始化失败（{e}），回落到 TestEmbedder");
                }
            }
        }
    }
    let test = Arc::new(TestEmbedder::new(1024));
    let label = "TestEmbedder（伪向量：仅用于管道与排序逻辑回归）".to_string();
    (test, None, label)
}

// ============================================================================
// 指标
// ============================================================================

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct MetricSet {
    queries: usize,
    recall_at_1: f64,
    recall_at_5: f64,
    recall_at_10: f64,
    mrr_at_10: f64,
    ndcg_at_10: f64,
    /// 硬负例违规率（Top-3 内出现 forbidden 的查询占比）
    forbidden_rate: f64,
    /// 首位误取率（**第 1 条**结果就是硬负例的查询占比）
    ///
    /// 与 `forbidden_rate` 的区别：Top-3 里混入矛盾项，Agent 尚可凭排序
    /// 与判断回避；首位就是矛盾项，则 Agent 会直接把它当成答案依据。
    /// 这才是长期记忆里真正会造成错误事实的路径。
    first_hit_forbidden_rate: f64,
    /// 负控制查询被命中的比例（越低越好）
    negative_hit_rate: f64,
    p50_ms: f64,
    p95_ms: f64,
}

/// 逐查询结果（配对门禁用：同一条查询在基线/本次之间逐条比较，
/// 消除"这批查询本身多难"的方差，灵敏度远高于两个独立比例相减）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerQueryRecord {
    id: String,
    category: String,
    scene: Option<String>,
    rank_first_relevant: Option<usize>,
    relevant_total: usize,
    violated_forbidden: bool,
    /// 第 1 条结果就是硬负例
    #[serde(default)]
    first_hit_forbidden: bool,
    result_count: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct BenchReport {
    dataset_fingerprint: String,
    dataset_docs: usize,
    dataset_queries: usize,
    embedder: String,
    /// 嵌入器是否提供真实语义信号（false = 伪向量，指标不代表语义能力）
    #[serde(default)]
    semantic_ready: bool,
    config: String,
    overall: MetricSet,
    by_scene: BTreeMap<String, MetricSet>,
    by_category: BTreeMap<String, MetricSet>,
    #[serde(default)]
    per_query: Vec<PerQueryRecord>,
}

fn dataset_fingerprint(ds: &Dataset) -> String {
    let mut hasher = Sha256::new();
    for doc in &ds.docs {
        hasher.update(doc.key.as_bytes());
        hasher.update(doc.entry.content.as_bytes());
    }
    for q in &ds.queries {
        hasher.update(q.id.as_bytes());
        hasher.update(q.text.as_bytes());
    }
    format!("{:x}", hasher.finalize())[..16].to_string()
}

struct QueryOutcome {
    id: String,
    category: &'static str,
    scene: Option<MemoryScene>,
    rank_first_relevant: Option<usize>,
    retrieved_relevant: usize,
    relevant_total: usize,
    violated_forbidden: bool,
    /// 第 1 条结果就是硬负例
    first_hit_forbidden: bool,
    /// 本次检索实际返回的条目数（负控制查询用它判断"是否召回了任何东西"）
    result_count: usize,
    elapsed_ms: f64,
}

fn metrics_for(outcomes: &[QueryOutcome]) -> MetricSet {
    let n = outcomes.len();
    if n == 0 {
        return MetricSet::default();
    }
    let mut recall1 = 0usize;
    let mut recall5 = 0usize;
    let mut recall10 = 0usize;
    let mut mrr = 0.0f64;
    let mut ndcg = 0.0f64;
    let mut forbidden = 0usize;
    let mut first_hit_forbidden = 0usize;
    let mut negatives_hit = 0usize;
    let mut negative_total = 0usize;
    let mut latencies: Vec<f64> = Vec::with_capacity(n);

    for o in outcomes {
        latencies.push(o.elapsed_ms);
        if o.category == "negative" {
            negative_total += 1;
            if o.result_count > 0 {
                negatives_hit += 1;
            }
            continue;
        }
        let rank = o.rank_first_relevant;
        if let Some(r) = rank {
            if r <= 1 {
                recall1 += 1;
            }
            if r <= 5 {
                recall5 += 1;
            }
            if r <= 10 {
                recall10 += 1;
            }
            mrr += 1.0 / r as f64;
        }
        if o.relevant_total > 0 {
            // 二值相关性 nDCG@10：理想排序 = 全部相关文档排在前面
            let dcg: f64 = match o.rank_first_relevant {
                Some(r) if r <= 10 => 1.0 / ((r as f64 + 1.0).log2()),
                _ => 0.0,
            };
            let ideal_count = o.relevant_total.min(10);
            let idcg: f64 = (0..ideal_count)
                .map(|i| 1.0 / ((i as f64 + 2.0).log2()))
                .sum();
            if idcg > 0.0 {
                ndcg += dcg / idcg;
            }
        }
        if o.violated_forbidden {
            forbidden += 1;
        }
        if o.first_hit_forbidden {
            first_hit_forbidden += 1;
        }
    }

    let effective = n - negative_total;
    latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| -> f64 {
        if latencies.is_empty() {
            0.0
        } else {
            let idx = ((latencies.len() - 1) as f64 * p).round() as usize;
            latencies[idx.min(latencies.len() - 1)]
        }
    };

    MetricSet {
        queries: n,
        recall_at_1: recall1 as f64 / effective.max(1) as f64,
        recall_at_5: recall5 as f64 / effective.max(1) as f64,
        recall_at_10: recall10 as f64 / effective.max(1) as f64,
        mrr_at_10: mrr / effective.max(1) as f64,
        ndcg_at_10: ndcg / effective.max(1) as f64,
        forbidden_rate: forbidden as f64 / effective.max(1) as f64,
        first_hit_forbidden_rate: first_hit_forbidden as f64 / effective.max(1) as f64,
        negative_hit_rate: if negative_total == 0 {
            0.0
        } else {
            negatives_hit as f64 / negative_total as f64
        },
        p50_ms: pct(0.50),
        p95_ms: pct(0.95),
    }
}

// ============================================================================
// 运行评测
// ============================================================================

async fn seed_dataset(
    ds: &Dataset,
    embedder: &Arc<dyn Embedder>,
) -> Result<AccountScopedStore<dyn MemoryHubStore>> {
    let account: AccountId = Uuid::from_u128(0xBEEF);
    let hub = Arc::new(InMemoryHubStore::new());
    let store = AccountScopedStore::new(hub as Arc<dyn MemoryHubStore>, account);

    // 批量嵌入（真实模型下显著减少请求数；缺嵌入样本保持 None）
    let texts: Vec<String> = ds
        .docs
        .iter()
        .filter(|d| d.has_embedding)
        .map(|d| d.entry.content.clone())
        .collect();
    let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
    for chunk in texts.chunks(32) {
        vectors.extend(embedder.embed(chunk).await?);
    }

    let mut vectors = vectors.into_iter();
    for doc in &ds.docs {
        let mut entry = doc.entry.clone();
        if doc.has_embedding {
            let mut vector = vectors.next().expect("嵌入数量与文档数不匹配");
            mem_core::l2_normalize(&mut vector);
            entry.embedding = Some(vector);
            entry.embedding_model = Some(embedder.model_id().to_string());
        }
        store.insert(&entry).await?;
    }
    Ok(store)
}

async fn run_bench(
    retriever_factory: impl Fn(
        AccountScopedStore<dyn MemoryHubStore>,
        Arc<dyn Embedder>,
    ) -> HybridRetriever,
    embedder: Arc<dyn Embedder>,
    label_config: String,
    ds: &Dataset,
) -> Result<BenchReport> {
    let store = seed_dataset(ds, &embedder).await?;
    let retriever = retriever_factory(store, embedder.clone());

    let auth = AuthCtx {
        agent_id: "bench-agent".into(),
        account_id: Uuid::from_u128(0xBEEF),
        scopes: vec![Scope::Read, Scope::Write],
        acl: Default::default(),
    };

    let mut outcomes = Vec::with_capacity(ds.queries.len());
    for q in &ds.queries {
        let query = SearchQuery {
            namespace: q.namespace.clone(),
            query_text: q.text.clone(),
            kinds: None,
            tags: q.tags.clone(),
            top_k: TOP_K,
            include_archived: false,
            min_score: None,
            scene: q.scene,
            layer: q.layer,
            valid_at: q.valid_at,
            include_superseded: q.include_superseded,
        };
        let start = Instant::now();
        let results = retriever.search(&auth, &query).await?;
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        outcomes.push(score_outcome(ds, q, results, elapsed_ms));
    }

    let by_scene = |scene: MemoryScene| -> MetricSet {
        let subset: Vec<QueryOutcome> = outcomes
            .iter()
            .filter(|o| o.scene == Some(scene))
            .map(clone_outcome)
            .collect();
        metrics_for(&subset)
    };
    let mut scene_metrics = BTreeMap::new();
    for (name, scene) in [
        ("chat", MemoryScene::Chat),
        ("working", MemoryScene::Working),
        ("coding", MemoryScene::Coding),
    ] {
        scene_metrics.insert(name.to_string(), by_scene(scene));
    }

    let mut category_metrics = BTreeMap::new();
    let categories: HashSet<&str> = outcomes.iter().map(|o| o.category).collect();
    for category in categories {
        let subset: Vec<QueryOutcome> = outcomes
            .iter()
            .filter(|o| o.category == category)
            .map(clone_outcome)
            .collect();
        category_metrics.insert(category.to_string(), metrics_for(&subset));
    }

    let per_query: Vec<PerQueryRecord> = outcomes.iter().map(per_query_record).collect();
    let overall = metrics_for(&outcomes);
    Ok(BenchReport {
        dataset_fingerprint: dataset_fingerprint(ds),
        dataset_docs: ds.docs.len(),
        dataset_queries: ds.queries.len(),
        embedder: embedder.model_id().to_string(),
        semantic_ready: embedder.provides_semantic_signal(),
        config: label_config,
        overall,
        by_scene: scene_metrics,
        by_category: category_metrics,
        per_query,
    })
}

fn per_query_record(o: &QueryOutcome) -> PerQueryRecord {
    PerQueryRecord {
        id: o.id.clone(),
        category: o.category.to_string(),
        scene: o.scene.map(|s| format!("{:?}", s).to_lowercase()),
        rank_first_relevant: o.rank_first_relevant,
        relevant_total: o.relevant_total,
        violated_forbidden: o.violated_forbidden,
        first_hit_forbidden: o.first_hit_forbidden,
        result_count: o.result_count,
    }
}

fn clone_outcome(o: &QueryOutcome) -> QueryOutcome {
    QueryOutcome {
        id: o.id.clone(),
        category: o.category,
        scene: o.scene,
        rank_first_relevant: o.rank_first_relevant,
        retrieved_relevant: o.retrieved_relevant,
        relevant_total: o.relevant_total,
        violated_forbidden: o.violated_forbidden,
        first_hit_forbidden: o.first_hit_forbidden,
        result_count: o.result_count,
        elapsed_ms: o.elapsed_ms,
    }
}

fn score_outcome(
    ds: &Dataset,
    q: &EvalQuery,
    results: Vec<mem_core::Scored<MemoryEntry>>,
    elapsed_ms: f64,
) -> QueryOutcome {
    let relevant: HashSet<&str> = q.relevant.iter().map(String::as_str).collect();
    let forbidden: HashSet<&str> = q.forbidden.iter().map(String::as_str).collect();
    let key_by_id: HashMap<Uuid, &str> = ds
        .docs
        .iter()
        .map(|d| (d.entry.id, d.key.as_str()))
        .collect();

    let result_count = results.len();
    let mut rank_first_relevant = None;
    let mut retrieved_relevant = 0usize;
    let mut violated_forbidden = false;
    let mut first_hit_forbidden = false;
    for (idx, scored) in results.iter().enumerate() {
        if let Some(key) = key_by_id.get(&scored.item.id) {
            if relevant.contains(key) {
                retrieved_relevant += 1;
                if rank_first_relevant.is_none() {
                    rank_first_relevant = Some(idx + 1);
                }
            }
            if idx < FORBIDDEN_K && forbidden.contains(key) {
                violated_forbidden = true;
            }
            if idx == 0 && forbidden.contains(key) {
                first_hit_forbidden = true;
            }
        }
    }
    QueryOutcome {
        id: q.id.clone(),
        category: q.category,
        scene: q.scene,
        rank_first_relevant,
        retrieved_relevant,
        relevant_total: q.relevant.len(),
        violated_forbidden,
        first_hit_forbidden,
        result_count,
        elapsed_ms,
    }
}

/// 生产默认配置的检索器
///
/// 包含**默认拒绝阈值**：评测必须反映生产行为——
/// 生产上调用方不传 min_score 时就是这个阈值在把关，
/// 评测若不带阈值，量到的就不是线上会发生的事。
fn default_retriever(
    store: AccountScopedStore<dyn MemoryHubStore>,
    embedder: Arc<dyn Embedder>,
) -> HybridRetriever {
    HybridRetriever::new(
        Arc::new(store) as Arc<dyn MemoryStore>,
        embedder,
        100,
        100,
        [0.50, 0.20, 0.10, 0.10, 0.10],
        0.7,
        1000,
    )
    .with_clock(Arc::new(FixedClock::new(fixed_now())))
    .with_default_min_score(Some(common::DEFAULT_RETRIEVAL_MIN_SCORE))
}

// ============================================================================
// 基线读写与门禁
// ============================================================================

fn baseline_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("crates/retrieval/tests/data/eval_baseline.json")
}

/// 基线可比性判定：指纹 / 配置 / 嵌入器任一不同即视为不可比，必须显式失败。
///
/// 抽成独立函数是为了让"不可比必须报错"这一契约可被单测覆盖——
/// 门禁里写成 `println + return` 时，测试无法区分"没退化"与"没比较"。
fn assert_baseline_comparable(base: &BenchReport, report: &BenchReport) {
    if base.dataset_fingerprint != report.dataset_fingerprint {
        panic!(
            "数据集指纹与基线不一致（{} → {}）：两次评测不可比，\
             不得据此判定无退化。确认数据变更符合预期后，用 \
             HUB_EVAL_WRITE_BASELINE=1 重新冻结基线",
            base.dataset_fingerprint, report.dataset_fingerprint
        );
    }
    if base.config != report.config {
        panic!(
            "评测配置与基线不一致（基线 {} / 本次 {}）：权重、阈值或场景模式\
             不同意味着指标口径不同，不可做配对检验。确认无回归后用 \
             HUB_EVAL_WRITE_BASELINE=1 重新冻结基线",
            base.config, report.config
        );
    }
    assert_eq!(
        base.embedder, report.embedder,
        "嵌入器与基线不一致（{} vs {}）：伪向量与真实模型不可比，请用相同嵌入器重跑或重新冻结基线",
        base.embedder, report.embedder
    );
}

fn load_baseline() -> Option<BenchReport> {
    std::fs::read_to_string(baseline_path())
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
}

fn save_baseline(report: &BenchReport) {
    let path = baseline_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let raw = serde_json::to_string_pretty(report).unwrap();
    std::fs::write(&path, raw).expect("写入基线失败");
}

fn print_report(report: &BenchReport) {
    println!("\n==================== Retrieval Bench v2 ====================");
    println!(
        "数据集: {} 文档 / {} 查询 (指纹 {})",
        report.dataset_docs, report.dataset_queries, report.dataset_fingerprint
    );
    println!("嵌入器: {}", report.embedder);
    println!(
        "语义能力: {}",
        if report.semantic_ready {
            "已就绪（真实嵌入模型，指标代表语义检索质量）"
        } else {
            "未就绪（伪向量：指标只反映管道与词法排序，不代表语义能力）"
        }
    );
    println!("配置  : {}", report.config);
    let m = &report.overall;
    println!(
        "总体  : Recall@1 {:.2}% | Recall@5 {:.2}% | Recall@10 {:.2}% | MRR@10 {:.4} | nDCG@10 {:.4}",
        m.recall_at_1 * 100.0,
        m.recall_at_5 * 100.0,
        m.recall_at_10 * 100.0,
        m.mrr_at_10,
        m.ndcg_at_10
    );
    println!(
        "       : 硬负例违规率 {:.2}%（Top-3）| 首位误取率 {:.2}% | 负控制命中率 {:.2}% | p50 {:.2}ms | p95 {:.2}ms",
        m.forbidden_rate * 100.0,
        m.first_hit_forbidden_rate * 100.0,
        m.negative_hit_rate * 100.0,
        m.p50_ms,
        m.p95_ms
    );
    for (name, s) in &report.by_scene {
        println!(
            "  [{name:>7}] n={:<4} Recall@1 {:.2}% Recall@5 {:.2}% MRR {:.4} nDCG {:.4} 违规 {:.2}%",
            s.queries,
            s.recall_at_1 * 100.0,
            s.recall_at_5 * 100.0,
            s.mrr_at_10,
            s.ndcg_at_10,
            s.forbidden_rate * 100.0
        );
    }
    for (name, s) in &report.by_category {
        println!(
            "  [{name:>10}] n={:<4} Recall@1 {:.2}% Recall@5 {:.2}% nDCG {:.4} 违规 {:.2}%",
            s.queries,
            s.recall_at_1 * 100.0,
            s.recall_at_5 * 100.0,
            s.ndcg_at_10,
            s.forbidden_rate * 100.0
        );
    }
    println!("===========================================================\n");
}

/// 每条查询的 6 个可配对指标：Recall@1/5/10、RR@10、nDCG@10、硬负例违规
///
/// 负控制查询不参与前 5 个质量指标（与 `metrics_for` 口径一致），
/// 它们单独用"是否返回任何结果"衡量。
fn per_query_values(r: &PerQueryRecord) -> Option<[f64; 7]> {
    if r.category == "negative" {
        return None;
    }
    let hit = |k: usize| {
        r.rank_first_relevant
            .map_or(0.0, |rank| if rank <= k { 1.0 } else { 0.0 })
    };
    let rr = r
        .rank_first_relevant
        .map_or(0.0, |rank| if rank <= 10 { 1.0 / rank as f64 } else { 0.0 });
    let ndcg = match r.rank_first_relevant {
        Some(rank) if rank <= 10 && r.relevant_total > 0 => {
            let dcg = 1.0 / ((rank as f64 + 1.0).log2());
            let ideal: f64 = (0..r.relevant_total.min(10))
                .map(|i| 1.0 / ((i as f64 + 2.0).log2()))
                .sum();
            dcg / ideal
        }
        _ => 0.0,
    };
    let violation = if r.violated_forbidden { 1.0 } else { 0.0 };
    let first_violation = if r.first_hit_forbidden { 1.0 } else { 0.0 };
    Some([
        hit(1),
        hit(5),
        hit(10),
        rr,
        ndcg,
        violation,
        first_violation,
    ])
}

const PAIRED_METRICS: [(&str, bool); 7] = [
    ("Recall@1", true),
    ("Recall@5", true),
    ("Recall@10", true),
    ("MRR@10", true),
    ("nDCG@10", true),
    ("硬负例违规率", false),
    ("首位误取率", false),
];

/// 配对比较结果（用于打印与门禁判定）
struct PairedDelta {
    metric: &'static str,
    base_mean: f64,
    cur_mean: f64,
    delta: f64,
    std_err: f64,
    z: f64,
    n: usize,
}

/// 配对检验：同一批查询 id 上逐条比较，返回每个指标的配对差值、标准误与 z 值。
///
/// 相比"两个独立比例相减"，配对设计消掉了查询难度差异带来的方差：
/// 600 条查询下总体指标的配对标准误通常只有 1~2 个百分点，比独立比较小一个量级。
fn paired_deltas(base: &[PerQueryRecord], cur: &[PerQueryRecord]) -> (Vec<PairedDelta>, usize) {
    let base_map: HashMap<&str, &PerQueryRecord> =
        base.iter().map(|r| (r.id.as_str(), r)).collect();
    let mut diffs: Vec<[f64; 7]> = Vec::new();
    let mut base_sum = [0.0f64; 7];
    let mut cur_sum = [0.0f64; 7];
    let mut matched = 0usize;

    for record in cur {
        let Some(b) = base_map.get(record.id.as_str()) else {
            continue;
        };
        matched += 1;
        // 负控制查询不参与质量指标（单独用"是否返回结果"衡量）
        let (Some(base_vals), Some(cur_vals)) = (per_query_values(b), per_query_values(record))
        else {
            continue;
        };
        let mut d = [0.0f64; 7];
        for i in 0..7 {
            base_sum[i] += base_vals[i];
            cur_sum[i] += cur_vals[i];
            d[i] = cur_vals[i] - base_vals[i];
        }
        diffs.push(d);
    }

    let n = diffs.len();
    let mut out = Vec::new();
    if n > 0 {
        for (i, (name, _)) in PAIRED_METRICS.iter().enumerate() {
            let delta = diffs.iter().map(|d| d[i]).sum::<f64>() / n as f64;
            let variance = if n > 1 {
                diffs
                    .iter()
                    .map(|d| {
                        let x = d[i] - delta;
                        x * x
                    })
                    .sum::<f64>()
                    / (n - 1) as f64
            } else {
                0.0
            };
            let std_err = (variance / n as f64).sqrt();
            let z = if std_err > f64::EPSILON {
                delta / std_err
            } else if delta.abs() > f64::EPSILON {
                f64::INFINITY.copysign(delta)
            } else {
                0.0
            };
            out.push(PairedDelta {
                metric: name,
                base_mean: base_sum[i] / n as f64,
                cur_mean: cur_sum[i] / n as f64,
                delta,
                std_err,
                z,
                n,
            });
        }
    }
    (out, matched)
}

/// 门禁参数：绝对下限 1pt + 配对 z 检验（z >= 2.0）。
/// 只抓"既超过 1 个百分点、又在统计上可分"的退化，避免噪声触发告警。
const PAIRED_ABS_FLOOR: f64 = 0.01;
const PAIRED_Z_CRITICAL: f64 = 2.0;
/// 类别级门禁的最小配对样本量（不足则只打印不判定）
const PAIRED_MIN_CATEGORY_N: usize = 30;

fn is_regression(d: &PairedDelta, higher_is_better: bool) -> bool {
    if higher_is_better {
        d.delta < -PAIRED_ABS_FLOOR && d.z <= -PAIRED_Z_CRITICAL
    } else {
        d.delta > PAIRED_ABS_FLOOR && d.z >= PAIRED_Z_CRITICAL
    }
}

fn print_paired(title: &str, deltas: &[PairedDelta]) {
    println!(
        "[paired] {title}（n={}，逐查询配对）",
        deltas.first().map_or(0, |d| d.n)
    );
    for d in deltas {
        println!(
            "  {:<14} 基线 {:.4} → 本次 {:.4} | Δ {:+.4} | SE {:.4} | z {:+.2}",
            d.metric, d.base_mean, d.cur_mean, d.delta, d.std_err, d.z
        );
    }
}

/// 门禁：逐指标配对比较 + 分类别配对比较
fn paired_gate(base: &BenchReport, cur: &BenchReport) -> Vec<String> {
    let mut failures = Vec::new();

    let expected = base.per_query.len();
    let (overall, matched) = paired_deltas(&base.per_query, &cur.per_query);
    if expected > 0 && (matched as f64) < 0.95 * expected as f64 {
        failures.push(format!(
            "配对样本不足：基线 {expected} 条，仅匹配到 {matched} 条（<95%），无法做可靠比较"
        ));
        return failures;
    }
    if overall.is_empty() {
        failures.push(
            "基线缺少逐查询记录（旧格式基线），请用 HUB_EVAL_WRITE_BASELINE=1 重新冻结".into(),
        );
        return failures;
    }
    print_paired("总体", &overall);
    for (d, (name, higher)) in overall.iter().zip(PAIRED_METRICS) {
        if is_regression(d, higher) {
            failures.push(format!(
                "{name} 退化：{:.4} → {:.4}（Δ {:+.4}，SE {:.4}，z {:+.2}）",
                d.base_mean, d.cur_mean, d.delta, d.std_err, d.z
            ));
        }
    }

    // 分类别：样本量足够时同样做配对门禁
    let mut categories: Vec<&str> = base.by_category.keys().map(String::as_str).collect();
    categories.sort_unstable();
    for category in categories {
        let base_sub: Vec<PerQueryRecord> = base
            .per_query
            .iter()
            .filter(|r| r.category == category)
            .cloned()
            .collect();
        let cur_sub: Vec<PerQueryRecord> = cur
            .per_query
            .iter()
            .filter(|r| r.category == category)
            .cloned()
            .collect();
        let (deltas, matched) = paired_deltas(&base_sub, &cur_sub);
        if deltas.is_empty() || matched < PAIRED_MIN_CATEGORY_N {
            continue;
        }
        print_paired(&format!("类别 {category}"), &deltas);
        for (d, (name, higher)) in deltas.iter().zip(PAIRED_METRICS) {
            // 违规率只对"应当有约束"的类别有意义；其余指标按同一规则
            if is_regression(d, higher) {
                failures.push(format!(
                    "[{category}] {name} 退化：{:.4} → {:.4}（Δ {:+.4}，SE {:.4}，z {:+.2}）",
                    d.base_mean, d.cur_mean, d.delta, d.std_err, d.z
                ));
            }
        }
    }

    failures
}

/// 回归：基线不可比时门禁必须**显式失败**，不能静默跳过。
///
/// 旧行为在数据集指纹或配置不一致时只打印一行提示就 return，测试照样绿，
/// 于是"数据集/权重被改动"与"质量达标"在 CI 里长得一模一样。
#[test]
fn incomparable_baseline_must_fail_loudly() {
    let mut base = BenchReport {
        dataset_fingerprint: "fingerprint-a".to_string(),
        config: "cfg-a".to_string(),
        embedder: "embedder-a".to_string(),
        ..Default::default()
    };
    let report = BenchReport {
        dataset_fingerprint: "fingerprint-b".to_string(),
        config: "cfg-b".to_string(),
        embedder: "embedder-b".to_string(),
        ..Default::default()
    };
    // 指纹不同 → 必须 panic（用 catch_unwind 观察，而不是接受"正常返回"）
    let outcome = std::panic::catch_unwind(|| {
        assert_baseline_comparable(&base, &report);
    });
    assert!(
        outcome.is_err(),
        "数据集指纹不一致时必须显式失败，而不是静默跳过门禁"
    );

    // 指纹相同但配置不同 → 同样必须失败
    base.dataset_fingerprint = report.dataset_fingerprint.clone();
    let outcome = std::panic::catch_unwind(|| {
        assert_baseline_comparable(&base, &report);
    });
    assert!(
        outcome.is_err(),
        "配置口径不一致时必须显式失败，而不是静默跳过门禁"
    );

    // 嵌入器不同（伪向量 vs 真实模型）→ 同样必须失败
    base.config = report.config.clone();
    let outcome = std::panic::catch_unwind(|| {
        assert_baseline_comparable(&base, &report);
    });
    assert!(
        outcome.is_err(),
        "嵌入器不一致时必须显式失败：伪向量与真实模型不可比"
    );

    // 三项一致时应当通过
    base.embedder = report.embedder.clone();
    assert_baseline_comparable(&base, &report);
}

#[tokio::test]
async fn test_retrieval_bench_gate() {
    let ds = build_dataset();
    let (embedder, cached, label) = eval_embedder();
    let report = run_bench(
        default_retriever,
        embedder,
        format!(
            "default(strict, w=0.5/0.2/0.1/0.1/0.1, λ=0.7, min_score={})",
            common::DEFAULT_RETRIEVAL_MIN_SCORE
        ),
        &ds,
    )
    .await
    .expect("评测运行失败");
    if let Some(cached) = &cached {
        cached.flush();
    }
    print_report(&report);

    if std::env::var("HUB_EVAL_WRITE_BASELINE").is_ok() {
        save_baseline(&report);
        println!("[bench] 基线已写入 {}", baseline_path().display());
        return;
    }

    let Some(base) = load_baseline() else {
        println!(
            "[bench] 未找到基线（{}），本次仅打印指标；冻结基线请用 HUB_EVAL_WRITE_BASELINE=1",
            baseline_path().display()
        );
        return;
    };

    assert_baseline_comparable(&base, &report);

    let regressions = paired_gate(&base, &report);
    assert!(
        regressions.is_empty(),
        "检索质量相对基线退化（配对检验）：\n{}",
        regressions.join("\n")
    );

    // 语义验收开关：接入真实嵌入模型后，CI 应设 HUB_EVAL_REQUIRE_SEMANTIC=1，
    // 让"用伪向量冒充语义质量结论"直接失败。
    if std::env::var("HUB_EVAL_REQUIRE_SEMANTIC").is_ok() && !report.semantic_ready {
        panic!(
            "评测要求真实语义模型，但当前嵌入器 {} 不提供语义信号（伪向量）：\
             伪向量下的指标不代表语义能力，不得作为语义验收结论",
            report.embedder
        );
    }

    println!(
        "[bench] 门禁通过：{}（嵌入器 {}，数据集指纹 {}，语义能力 {}）",
        label,
        report.embedder,
        report.dataset_fingerprint,
        if report.semantic_ready {
            "已就绪"
        } else {
            "未就绪"
        }
    );
}

// ============================================================================
// 消融（默认 ignored；HUB_EVAL_ABLATIONS=1 + --ignored 触发）
// ============================================================================

#[tokio::test]
#[ignore]
async fn test_retrieval_bench_ablations() {
    if std::env::var("HUB_EVAL_ABLATIONS").is_err() {
        println!("[bench] 未设置 HUB_EVAL_ABLATIONS=1，跳过消融");
        return;
    }
    let ds = build_dataset();
    let (embedder, cached, _label) = eval_embedder();

    struct Variant {
        name: &'static str,
        weights: [f32; 5],
        lambda: f32,
        tau_override: Option<f32>,
    }
    let base_weights = [0.50f32, 0.20, 0.10, 0.10, 0.10];
    let mut variants: Vec<Variant> = vec![Variant {
        name: "baseline",
        weights: base_weights,
        lambda: 0.7,
        tau_override: None,
    }];
    for (i, name) in ["sim", "kw", "conf", "rec", "prior"].iter().enumerate() {
        let mut w = base_weights;
        w[i] = 0.0;
        let sum: f32 = w.iter().sum();
        for x in w.iter_mut() {
            *x /= sum;
        }
        variants.push(Variant {
            name: match i {
                0 => "ablate-sim",
                1 => "ablate-kw",
                2 => "ablate-conf",
                3 => "ablate-rec",
                _ => "ablate-prior",
            },
            weights: w,
            lambda: 0.7,
            tau_override: None,
        });
        let _ = name;
    }
    for (name, lambda) in [("lambda-0.5", 0.5f32), ("lambda-0.9", 0.9f32)] {
        variants.push(Variant {
            name,
            weights: base_weights,
            lambda,
            tau_override: None,
        });
    }
    for (name, tau) in [("tau-7d", 7.0f32), ("tau-180d", 180.0f32)] {
        variants.push(Variant {
            name,
            weights: base_weights,
            lambda: 0.7,
            tau_override: Some(tau),
        });
    }
    variants.push(Variant {
        name: "scene-prefer",
        weights: base_weights,
        lambda: 0.7,
        tau_override: None,
    });

    println!("\n==================== 消融实验 ====================");
    println!(
        "{:<16} {:>9} {:>9} {:>9} {:>9} {:>9}",
        "variant", "R@1", "R@5", "R@10", "MRR@10", "nDCG@10"
    );
    for v in variants {
        let weights = v.weights;
        let lambda = v.lambda;
        let tau = v.tau_override;
        let scene_mode = if v.name == "scene-prefer" {
            mem_core::SceneMode::Prefer
        } else {
            mem_core::SceneMode::Strict
        };
        let report = run_bench(
            move |store, embedder| {
                let mut r = HybridRetriever::new(
                    Arc::new(store) as Arc<dyn MemoryStore>,
                    embedder,
                    100,
                    100,
                    weights,
                    lambda,
                    1000,
                )
                .with_clock(Arc::new(FixedClock::new(fixed_now())))
                .with_scene_mode(scene_mode);
                if let Some(tau) = tau {
                    r = r.with_recency_tau(tau);
                }
                r
            },
            embedder.clone(),
            format!("ablation:{}", v.name),
            &ds,
        )
        .await
        .expect("消融运行失败");
        let m = &report.overall;
        println!(
            "{:<16} {:>8.2}% {:>8.2}% {:>8.2}% {:>9.4} {:>9.4}",
            v.name,
            m.recall_at_1 * 100.0,
            m.recall_at_5 * 100.0,
            m.recall_at_10 * 100.0,
            m.mrr_at_10,
            m.ndcg_at_10
        );
    }
    println!("=================================================\n");
    if let Some(cached) = &cached {
        cached.flush();
    }
}

// ============================================================================
// 绝对门禁：域外拒绝率 + 业务正确性（不依赖基线，独立判定）
// ============================================================================
//
// 相对门禁（"相比上次没有退化"）不足以守住检索质量：
// 一个本身就很弱的基线不发生退化时，测试照样通过。
// 下面两类门禁给出**绝对**判定，与基线无关：
//
// 1. **域外拒绝门禁**：域外查询被命中的比例必须有绝对上限。
//    长期记忆里，域外查询硬凑一条结果比"没有结果"更危险——
//    Agent 会把它当成事实依据。
// 2. **业务正确性门禁**：时间有效性、场景约束、标签/层级约束、
//    Working 环境边界都属于业务语义要求，必须**零违规**。
//    这些性质与排序质量无关：即使 Recall@1 很低，
//    "返回了在参考时点已失效的事实"也是硬错误。

/// 域外误召回率绝对上限（相对"所有域外查询"）
///
/// 取值依据：默认拒绝阈值标定结果（见 `test_min_score_calibration`）。
/// 该上限与嵌入器无关——域外查询无论用什么模型都不该被当成答案。
const OOD_MAX_HIT_RATE: f64 = 0.10;

/// 首位误取率绝对上限（第 1 条结果即硬负例的查询占比）
///
/// Top-3 里混入矛盾项 Agent 尚可回避，首位就是矛盾项则会被直接当答案。
/// 伪向量下实测 0.56%，这里留出余量但不允许它悄悄变差。
const FIRST_HIT_FORBIDDEN_MAX_RATE: f64 = 0.02;

/// 生产默认拒绝阈值（与 `common::DEFAULT_RETRIEVAL_MIN_SCORE` 保持一致）
const DEFAULT_MIN_SCORE: f32 = common::DEFAULT_RETRIEVAL_MIN_SCORE;

/// 构造一个启用了默认拒绝阈值的检索器
fn thresholded_retriever(
    store: AccountScopedStore<dyn MemoryHubStore>,
    embedder: Arc<dyn Embedder>,
    min_score: f32,
) -> HybridRetriever {
    HybridRetriever::new(
        Arc::new(store) as Arc<dyn MemoryStore>,
        embedder,
        100,
        100,
        [0.50, 0.20, 0.10, 0.10, 0.10],
        0.7,
        1000,
    )
    .with_clock(Arc::new(FixedClock::new(fixed_now())))
    .with_default_min_score(if min_score > 0.0 {
        Some(min_score)
    } else {
        None
    })
}

/// 门禁 1：域外误召回率绝对上限
///
/// 相对门禁无法抓住"域外查询全部被命中"这种**长期存在**的缺陷——
/// 它不随版本退化，因此每次都比基线"没有变差"。
/// 这里给出绝对上限：超过即失败，与基线无关。
#[tokio::test]
async fn test_ood_rejection_gate() {
    let ds = build_dataset();
    let (embedder, cached, label) = eval_embedder();
    let report = run_bench(
        move |store, embedder| thresholded_retriever(store, embedder, DEFAULT_MIN_SCORE),
        embedder,
        format!("ood-gate(min_score={DEFAULT_MIN_SCORE})"),
        &ds,
    )
    .await
    .expect("域外拒绝评测运行失败");
    if let Some(cached) = &cached {
        cached.flush();
    }

    let m = &report.overall;
    println!(
        "[ood-gate] 域外命中率 {:.2}%（上限 {:.2}%）| 总体 Recall@1 {:.2}% Recall@5 {:.2}% \
         MRR@10 {:.4} nDCG@10 {:.4} | 嵌入器 {label}",
        m.negative_hit_rate * 100.0,
        OOD_MAX_HIT_RATE * 100.0,
        m.recall_at_1 * 100.0,
        m.recall_at_5 * 100.0,
        m.mrr_at_10,
        m.ndcg_at_10
    );

    assert!(
        m.negative_hit_rate <= OOD_MAX_HIT_RATE,
        "域外查询误召回率 {:.2}% 超过绝对上限 {:.2}%：\
         没有达到最小可信分的候选时必须返回\"无可靠匹配\"，而不是硬凑结果",
        m.negative_hit_rate * 100.0,
        OOD_MAX_HIT_RATE * 100.0
    );

    // 首位误取率：第 1 条结果就是硬负例——Agent 会直接把它当成答案依据，
    // 这是长期记忆里真正造成错误事实的路径，必须有绝对上限。
    assert!(
        m.first_hit_forbidden_rate <= FIRST_HIT_FORBIDDEN_MAX_RATE,
        "首位误取率 {:.2}% 超过绝对上限 {:.2}%：\
         矛盾事实不得占据首位（长程记忆里它会被直接当作答案依据）",
        m.first_hit_forbidden_rate * 100.0,
        FIRST_HIT_FORBIDDEN_MAX_RATE * 100.0
    );
}

/// 业务正确性检查结果
#[derive(Debug, Default)]
struct ConstraintReport {
    /// 各类违规计数（约束名 → 违规次数）
    violations: BTreeMap<String, usize>,
    /// 违规样例（最多 5 条/类）
    samples: BTreeMap<String, Vec<String>>,
    /// 各约束检查的查询数
    checked: BTreeMap<String, usize>,
    /// 因"语义能力未就绪"而**暂停判定**的约束（仍然计数并报告，不静默忽略）
    suspended: BTreeMap<String, String>,
}

impl ConstraintReport {
    fn record(&mut self, kind: &str, detail: String) {
        *self.violations.entry(kind.to_string()).or_insert(0) += 1;
        let s = self.samples.entry(kind.to_string()).or_default();
        if s.len() < 5 {
            s.push(detail);
        }
    }
}

/// 门禁 2：业务语义正确性（时间有效性 / 场景 / 标签 / 层级 / 环境边界）
///
/// 这些是**硬正确性**：返回一条在参考时点已失效的事实、
/// 或一条属于其它场景 / 其它环境的经验，都是错误答案，
/// 与排序质量无关。任一违规即失败。
#[tokio::test]
async fn test_business_constraint_gate() {
    let ds = build_dataset();
    let (embedder, _cached, _label) = eval_embedder();
    let store = seed_dataset(&ds, &embedder)
        .await
        .expect("写入评测数据集失败");
    let retriever = thresholded_retriever(store, embedder.clone(), DEFAULT_MIN_SCORE);

    let auth = AuthCtx {
        agent_id: "bench-agent".into(),
        account_id: Uuid::from_u128(0xBEEF),
        scopes: vec![Scope::Read, Scope::Write],
        acl: Default::default(),
    };
    let key_by_id: HashMap<Uuid, &str> = ds
        .docs
        .iter()
        .map(|d| (d.entry.id, d.key.as_str()))
        .collect();

    let mut report = ConstraintReport::default();
    for q in &ds.queries {
        let query = SearchQuery {
            namespace: q.namespace.clone(),
            query_text: q.text.clone(),
            kinds: None,
            tags: q.tags.clone(),
            top_k: TOP_K,
            include_archived: false,
            min_score: None,
            scene: q.scene,
            layer: q.layer,
            valid_at: q.valid_at,
            include_superseded: q.include_superseded,
        };
        let results = retriever.search(&auth, &query).await.expect("检索失败");

        for scored in &results {
            let item = &scored.item;
            let key = key_by_id.get(&item.id).copied().unwrap_or("<未知>");

            // 1. 时间有效性：返回的条目必须在参考时点有效
            if let Some(at) = q.valid_at {
                *report.checked.entry("时间有效性".to_string()).or_insert(0) += 1;
                let starts_ok = item.valid_from.is_none_or(|v| v <= at);
                let ends_ok = item.valid_until.is_none_or(|v| v > at);
                if !(starts_ok && ends_ok) {
                    report.record(
                        "时间有效性",
                        format!("查询 {} 返回了在参考时点无效的条目 {key}", q.id),
                    );
                }
            }

            // 2. 场景约束：请求了场景就不得返回其它场景的条目
            if let Some(scene) = q.scene {
                *report.checked.entry("场景约束".to_string()).or_insert(0) += 1;
                if item.scene != Some(scene) {
                    report.record(
                        "场景约束",
                        format!("查询 {} 返回了场景不匹配的条目 {key}", q.id),
                    );
                }
            }

            // 3. 标签约束
            if let Some(tags) = &q.tags {
                *report.checked.entry("标签约束".to_string()).or_insert(0) += 1;
                if !tags.iter().all(|t| item.tags.contains(t)) {
                    report.record(
                        "标签约束",
                        format!("查询 {} 返回了缺少必需标签的条目 {key}", q.id),
                    );
                }
            }

            // 4. 层级约束
            if let Some(layer) = q.layer {
                *report.checked.entry("层级约束".to_string()).or_insert(0) += 1;
                if item.layer != Some(layer) {
                    report.record(
                        "层级约束",
                        format!("查询 {} 返回了层级不匹配的条目 {key}", q.id),
                    );
                }
            }
        }

        // 4.5 时间锚定召回：钉住 valid_at 的查询必须真的召回该时点有效的事实。
        //     这是一条**召回**约束：时间过滤正确但什么都召不回来，同样是错的
        //     （默认阈值曾把历史事实按"距今多久"压到阈值之下）。
        //
        //     但它依赖**语义召回**：历史事实的措辞与查询往往不同，
        //     词法路根本匹配不上，只能靠向量路找回来。伪向量嵌入器下
        //     向量路返回的是随机候选，这条约束量到的是"语义能力缺失"，
        //     不是过滤缺陷——所以无语义信号时**暂停判定但照常计数报告**，
        //     接上真实模型后自动恢复为硬门禁。
        if q.valid_at.is_some() && !q.relevant.is_empty() {
            *report
                .checked
                .entry("时间锚定召回".to_string())
                .or_insert(0) += 1;
            let hit = results.iter().any(|s| {
                key_by_id
                    .get(&s.item.id)
                    .map(|k| q.relevant.iter().any(|r| r == k))
                    .unwrap_or(false)
            });
            if !hit {
                report.record(
                    "时间锚定召回",
                    format!("查询 {} 钉住时点却没有召回任何有效事实", q.id),
                );
            }
        }

        // 5. Working 环境边界：`memory_search` 没有结构化 env 参数，
        //    环境只能靠正文里的标记做词法区分，因此这里能断言的是
        //    **环境匹配的经验必须排在其它环境的等价经验之前**。
        //    （"环境不匹配的经验不得返回"是硬规则，由 Working 经验检索的
        //      strict_env 模式负责，见 working_memory_service 的测试。）
        if let Some(env) = query_env(&q.text) {
            *report.checked.entry("环境边界".to_string()).or_insert(0) += 1;
            let topic = topic_of(&q.id);
            let pos_matched =
                position_of_key(&results, &key_by_id, &format!("{topic}-env-primary"));
            let pos_other = position_of_key(&results, &key_by_id, &format!("{topic}-env-other"));
            if let (Some(matched), Some(other)) = (pos_matched, pos_other) {
                if matched > other {
                    report.record(
                        "环境边界",
                        format!(
                            "查询 {} 指定环境 {env}，环境匹配的经验排在了其它环境经验之后",
                            q.id
                        ),
                    );
                }
            }
        }
    }

    // 语义能力未就绪时，依赖语义召回的约束暂停判定（但照常计数并报告）
    let semantic_ready = embedder.provides_semantic_signal();
    if !semantic_ready {
        report.suspended.insert(
            "时间锚定召回".to_string(),
            "当前嵌入器不提供语义信号：历史事实的措辞与查询不同，词法路匹配不上，\
             只能靠向量路召回。该约束量到的是语义能力缺失而非过滤缺陷，\
             因此在伪向量模态下暂停判定；接入真实嵌入模型后自动恢复为硬门禁。"
                .to_string(),
        );
    }

    println!("\n==================== 业务正确性门禁 ====================");
    for (kind, checked) in &report.checked {
        let v = report.violations.get(kind).copied().unwrap_or(0);
        let note = if report.suspended.contains_key(kind) {
            "（判定已暂停：语义能力未就绪）"
        } else {
            ""
        };
        println!("  {kind:<8} 检查 {checked} 次，违规 {v} 次 {note}");
        for s in report.samples.get(kind).into_iter().flatten() {
            println!("      - {s}");
        }
    }
    for (kind, why) in &report.suspended {
        println!("  · {kind} 暂停判定：{why}");
    }
    println!("=========================================================\n");

    let failures: Vec<String> = report
        .violations
        .iter()
        .filter(|(kind, v)| **v > 0 && !report.suspended.contains_key(*kind))
        .map(|(kind, v)| {
            let samples = report
                .samples
                .get(kind)
                .map(|s| s.join("; "))
                .unwrap_or_default();
            format!("{kind} 违规 {v} 次：{samples}")
        })
        .collect();
    assert!(
        failures.is_empty(),
        "业务语义正确性门禁失败（必须零违规）：\n{}",
        failures.join("\n")
    );
}

/// 从查询 id 里取主题 id：`{topic}-env` → `{topic}`
fn topic_of(query_id: &str) -> String {
    query_id
        .rsplit_once('-')
        .map(|(head, _)| head.to_string())
        .unwrap_or_else(|| query_id.to_string())
}

/// 在结果里找出某个数据集 key 的位置
fn position_of_key(
    results: &[mem_core::Scored<MemoryEntry>],
    key_by_id: &HashMap<Uuid, &str>,
    key: &str,
) -> Option<usize> {
    results
        .iter()
        .position(|s| key_by_id.get(&s.item.id).copied() == Some(key))
}

/// 从查询文本里解析环境标记：`【环境 xxx】`
fn query_env(text: &str) -> Option<String> {
    let start = text.find("【环境 ")? + "【环境 ".len();
    let rest = &text[start..];
    let end = rest.find('】')?;
    Some(rest[..end].trim().to_string())
}

/// 阈值标定：扫描 min_score，输出"域外拒绝对 / 域内召回"的权衡曲线
///
/// 这是**真实模型下重新标定默认阈值**的工具：
/// 接上真实嵌入服务后用 `HUB_EVAL_EMBED_ENDPOINT` 跑一遍，
/// 选���"域外命中率达标且域内 Recall 最高"的阈值，
/// 再用它更新 `common::DEFAULT_RETRIEVAL_MIN_SCORE` 并重新冻结基线。
///
/// 伪向量下跑出的曲线只反映词法分布，**不能**直接当作语义阈值。
#[tokio::test]
#[ignore]
async fn test_min_score_calibration() {
    if std::env::var("HUB_EVAL_CALIBRATE").is_err() {
        println!("[calibrate] 未设置 HUB_EVAL_CALIBRATE=1，跳过阈值标定");
        return;
    }
    let ds = build_dataset();
    let (embedder, cached, label) = eval_embedder();
    let thresholds = [
        0.0f32, 0.40, 0.50, 0.55, 0.60, 0.65, 0.70, 0.75, 0.80, 0.85, 0.90,
    ];

    println!("\n==================== min_score 阈值标定 ====================");
    println!("嵌入器: {label}");
    println!(
        "{:<8} {:>8} {:>8} {:>8} {:>9} {:>10} {:>9} {:>9}",
        "阈值", "R@1", "R@5", "R@10", "域外命中", "硬负例违规", "首位误取", "paraphR@1"
    );
    for t in thresholds {
        let t2 = t;
        let report = run_bench(
            move |store, embedder| thresholded_retriever(store, embedder, t2),
            embedder.clone(),
            format!("calibrate:min_score={t}"),
            &ds,
        )
        .await
        .expect("标定运行失败");
        let m = &report.overall;
        let para = report
            .by_category
            .get("paraphrase")
            .map(|c| c.recall_at_1 * 100.0)
            .unwrap_or(0.0);
        println!(
            "{:<8.2} {:>7.2}% {:>7.2}% {:>7.2}% {:>8.2}% {:>9.2}% {:>8.2}% {:>8.2}%",
            t,
            m.recall_at_1 * 100.0,
            m.recall_at_5 * 100.0,
            m.recall_at_10 * 100.0,
            m.negative_hit_rate * 100.0,
            m.forbidden_rate * 100.0,
            m.first_hit_forbidden_rate * 100.0,
            para
        );
    }
    println!("=========================================================\n");
    if let Some(cached) = &cached {
        cached.flush();
    }
}

// ============================================================================
// PostgreSQL 评测（HUB_EVAL_PG=1 + DATABASE_URL）
// ============================================================================

#[tokio::test]
#[ignore]
async fn test_retrieval_bench_postgres() {
    if std::env::var("HUB_EVAL_PG").is_err() {
        println!("[bench] 未设置 HUB_EVAL_PG=1，跳过 PostgreSQL 评测");
        return;
    }
    let Ok(db_url) = std::env::var("DATABASE_URL") else {
        println!("[bench] 未设置 DATABASE_URL，跳过 PostgreSQL 评测");
        return;
    };
    let ds = build_dataset();
    let (embedder, cached, _label) = eval_embedder();

    let store = store::PostgresHubStore::connect(&db_url, 5, 10_000)
        .await
        .expect("连接 PostgreSQL 失败");
    let account: AccountId = Uuid::from_u128(0xCAFE);
    let scoped = Arc::new(AccountScopedStore::new(
        Arc::new(store) as Arc<dyn MemoryHubStore>,
        account,
    ));

    // 命名空间必须先注册：PostgreSQL 上 memories.namespace 有指向
    // namespaces 的外键，内存存储没有这层约束——不注册会直接撞外键。
    // 必须通过**带作用域**的 store 注册：写入时用的是账户前缀后的命名空间，
    // 注册未加前缀的名字等于没注册。
    let agent = common::AgentId::from("bench-agent");
    for ns in ["bench-chat", "bench-work", "bench-code"] {
        scoped
            .ensure_namespace(ns, &agent)
            .await
            .expect("注册评测命名空间失败");
    }

    // 写入数据集（命名空间带账户前缀，评测数据与真实数据隔离）
    let texts: Vec<String> = ds
        .docs
        .iter()
        .filter(|d| d.has_embedding)
        .map(|d| d.entry.content.clone())
        .collect();
    let mut vectors = Vec::with_capacity(texts.len());
    for chunk in texts.chunks(32) {
        vectors.extend(embedder.embed(chunk).await.expect("嵌入失败"));
    }
    let mut vectors = vectors.into_iter();
    for doc in &ds.docs {
        let mut entry = doc.entry.clone();
        if doc.has_embedding {
            let mut vector = vectors.next().expect("嵌入数量与文档数不匹配");
            mem_core::l2_normalize(&mut vector);
            entry.embedding = Some(vector);
            entry.embedding_model = Some(embedder.model_id().to_string());
        }
        scoped.insert(&entry).await.expect("写入失败");
    }

    // 与内存存储路径用**同一套生产配置**（含默认拒绝阈值）：
    // PG 侧的数字必须与内存侧同口径，否则两边的 p50/p95 与召回都没法互相印证。
    let retriever = HybridRetriever::new(
        scoped.clone() as Arc<dyn MemoryStore>,
        embedder.clone(),
        100,
        100,
        [0.50, 0.20, 0.10, 0.10, 0.10],
        0.7,
        3000,
    )
    .with_clock(Arc::new(FixedClock::new(fixed_now())))
    .with_default_min_score(Some(DEFAULT_MIN_SCORE));

    let auth = AuthCtx {
        agent_id: "bench-agent".into(),
        account_id: account,
        scopes: vec![Scope::Read, Scope::Write],
        acl: Default::default(),
    };

    let mut outcomes = Vec::with_capacity(ds.queries.len());
    for q in &ds.queries {
        let query = SearchQuery {
            namespace: q.namespace.clone(),
            query_text: q.text.clone(),
            kinds: None,
            tags: q.tags.clone(),
            top_k: TOP_K,
            include_archived: false,
            min_score: None,
            scene: q.scene,
            layer: q.layer,
            valid_at: q.valid_at,
            include_superseded: q.include_superseded,
        };
        let start = Instant::now();
        let results = retriever.search(&auth, &query).await.expect("检索失败");
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        outcomes.push(score_outcome(&ds, q, results, elapsed_ms));
    }
    let metrics = metrics_for(&outcomes);
    println!("\n==================== PostgreSQL Bench ====================");
    println!("嵌入器: {}", embedder.model_id());
    println!(
        "Recall@1 {:.2}% | Recall@5 {:.2}% | Recall@10 {:.2}% | MRR@10 {:.4} | nDCG@10 {:.4}",
        metrics.recall_at_1 * 100.0,
        metrics.recall_at_5 * 100.0,
        metrics.recall_at_10 * 100.0,
        metrics.mrr_at_10,
        metrics.ndcg_at_10
    );
    println!(
        "硬负例违规率 {:.2}% | 负控制命中率 {:.2}% | p50 {:.2}ms | p95 {:.2}ms",
        metrics.forbidden_rate * 100.0,
        metrics.negative_hit_rate * 100.0,
        metrics.p50_ms,
        metrics.p95_ms
    );
    println!("=========================================================\n");
    if let Some(cached) = &cached {
        cached.flush();
    }
}
