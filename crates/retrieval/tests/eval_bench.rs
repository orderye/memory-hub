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
    AccountScopedStore, Embedder, MemoryHubStore, MemoryStore, Retriever, SearchQuery,
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
/// 门禁容差：任一指标相对基线允许的最大退化（绝对值）
const MAX_REGRESSION: f64 = 0.02;

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
    for (i, (content, query)) in [
        (
            "客户投诉工单升级规则：超过 4 小时未响应的 P1 工单自动升级到值班主管",
            "工单升级规则 4 小时",
        ),
        (
            "灰度发布分批策略：1% → 10% → 50% → 100%，每档观察 30 分钟",
            "灰度发布分批 观察时长",
        ),
        (
            "供应链对账口径：以到货签收日为准，差异超过 0.5% 触发人工复核",
            "对账口径 到货签收",
        ),
        (
            "移动端崩溃率红线：单版本崩溃率超过 0.3% 立即回滚",
            "崩溃率红线 回滚阈值",
        ),
        (
            "内部 API 分页约定：游标式分页，默认每页 50 条，禁止使用 offset",
            "内部 API 分页约定 游标",
        ),
        (
            "成本归集标签规范：所有云资源必须打 cost-center 标签才能上线",
            "成本归集标签 cost-center",
        ),
    ]
    .iter()
    .enumerate()
    {
        let key = format!("no-embed-{}", i);
        let doc_content = content.to_string();
        push_doc(
            &mut ds,
            &mut doc_seq,
            key.clone(),
            "bench-chat",
            &doc_content,
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
            text: query.to_string(),
            scene: Some(MemoryScene::Chat),
            layer: None,
            tags: None,
            valid_at: None,
            include_superseded: false,
            relevant: vec![key.clone()],
            forbidden: vec![],
        });
    }

    // ===== 负控制查询：期望零命中 =====
    for (id, text, ns) in [
        ("neg-1", "火星殖民基地的氧气循环故障日志", "bench-chat"),
        ("neg-2", "大熊猫繁育基地的饲养排班表", "bench-work"),
        ("neg-3", "深海潜水器耐压壳焊接工艺", "bench-code"),
    ] {
        ds.queries.push(EvalQuery {
            id: id.to_string(),
            category: "negative",
            namespace: ns.to_string(),
            text: text.to_string(),
            scene: None,
            layer: None,
            tags: None,
            valid_at: None,
            include_superseded: false,
            relevant: vec![],
            forbidden: vec![],
        });
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
    /// 负控制查询被命中的比例（越低越好）
    negative_hit_rate: f64,
    p50_ms: f64,
    p95_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BenchReport {
    dataset_fingerprint: String,
    dataset_docs: usize,
    dataset_queries: usize,
    embedder: String,
    config: String,
    overall: MetricSet,
    by_scene: BTreeMap<String, MetricSet>,
    by_category: BTreeMap<String, MetricSet>,
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
    let mut negatives_hit = 0usize;
    let mut negative_total = 0usize;
    let mut latencies: Vec<f64> = Vec::with_capacity(n);

    for o in outcomes {
        latencies.push(o.elapsed_ms);
        if o.category == "negative" {
            negative_total += 1;
            if o.rank_first_relevant.is_some() || o.violated_forbidden {
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

    let overall = metrics_for(&outcomes);
    Ok(BenchReport {
        dataset_fingerprint: dataset_fingerprint(ds),
        dataset_docs: ds.docs.len(),
        dataset_queries: ds.queries.len(),
        embedder: embedder.model_id().to_string(),
        config: label_config,
        overall,
        by_scene: scene_metrics,
        by_category: category_metrics,
    })
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

    let mut rank_first_relevant = None;
    let mut retrieved_relevant = 0usize;
    let mut violated_forbidden = false;
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
        elapsed_ms,
    }
}

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
}

// ============================================================================
// 基线读写与门禁
// ============================================================================

fn baseline_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("crates/retrieval/tests/data/eval_baseline.json")
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
        "       : 硬负例违规率 {:.2}% | 负控制命中率 {:.2}% | p50 {:.2}ms | p95 {:.2}ms",
        m.forbidden_rate * 100.0,
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

#[tokio::test]
async fn test_retrieval_bench_gate() {
    let ds = build_dataset();
    let (embedder, cached, label) = eval_embedder();
    let report = run_bench(
        default_retriever,
        embedder,
        "default(strict, w=0.5/0.2/0.1/0.1/0.1, λ=0.7)".into(),
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

    if base.dataset_fingerprint != report.dataset_fingerprint {
        println!(
            "[bench] 数据集指纹变化（{} → {}），跳过逐指标门禁；确认无回归后重新冻结基线",
            base.dataset_fingerprint, report.dataset_fingerprint
        );
        return;
    }
    assert_eq!(
        base.embedder, report.embedder,
        "嵌入器与基线不一致（{} vs {}）：伪向量与真实模型不可比，请用相同嵌入器重跑或重新冻结基线",
        base.embedder, report.embedder
    );

    let mut regressions = Vec::new();
    let compare = |name: &str, base_v: f64, cur_v: f64, higher_is_better: bool| -> Option<String> {
        let delta = cur_v - base_v;
        let bad = if higher_is_better {
            -delta > MAX_REGRESSION
        } else {
            delta > MAX_REGRESSION
        };
        if bad {
            Some(format!(
                "{name}: {:.4} → {:.4}（退化 {:.4}，容差 {:.2}）",
                base_v,
                cur_v,
                if higher_is_better { -delta } else { delta },
                MAX_REGRESSION
            ))
        } else {
            None
        }
    };
    let pairs: [(&str, f64, f64, bool); 6] = [
        (
            "Recall@1",
            base.overall.recall_at_1,
            report.overall.recall_at_1,
            true,
        ),
        (
            "Recall@5",
            base.overall.recall_at_5,
            report.overall.recall_at_5,
            true,
        ),
        (
            "Recall@10",
            base.overall.recall_at_10,
            report.overall.recall_at_10,
            true,
        ),
        (
            "MRR@10",
            base.overall.mrr_at_10,
            report.overall.mrr_at_10,
            true,
        ),
        (
            "nDCG@10",
            base.overall.ndcg_at_10,
            report.overall.ndcg_at_10,
            true,
        ),
        (
            "硬负例违规率",
            base.overall.forbidden_rate,
            report.overall.forbidden_rate,
            false,
        ),
    ];
    for (name, b, c, higher) in pairs {
        if let Some(msg) = compare(name, b, c, higher) {
            regressions.push(msg);
        }
    }
    assert!(
        regressions.is_empty(),
        "检索质量相对基线退化：\n{}",
        regressions.join("\n")
    );

    println!(
        "[bench] 门禁通过：{}（嵌入器 {}，数据集指纹 {}）",
        label, report.embedder, report.dataset_fingerprint
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

    let retriever = HybridRetriever::new(
        scoped.clone() as Arc<dyn MemoryStore>,
        embedder.clone(),
        100,
        100,
        [0.50, 0.20, 0.10, 0.10, 0.10],
        0.7,
        3000,
    )
    .with_clock(Arc::new(FixedClock::new(fixed_now())));

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
