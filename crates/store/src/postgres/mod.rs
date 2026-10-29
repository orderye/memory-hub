#![forbid(unsafe_code)]

//! PostgreSQL 15 + pgvector 持久化实现（记忆 / 备份 / 整合提案 / 命名空间授权 / Agent / 账户）
//!
//! - 行级隔离：所有语句以 `namespace` 为一等谓词；命名空间的账户归属
//!   由 `AccountScopedStore` 以 `{account_id}/{namespace}` 前缀表达；
//! - pgvector ANN：`similar` 走 HNSW 索引的 `<=>` 余弦距离，分数 = 1 - 距离；
//! - 全文检索：`to_tsvector('simple', content)` GIN 索引；
//! - 迁移：版本化清单 + `_hub_migrations` 账本，已应用的迁移不重复执行；
//! - 只追加历史：`memory_versions` 写入幂等（`on conflict do nothing`）。
//!
//! 本实现使用运行时校验查询（非 `sqlx::query!` 宏），编译期无需数据库。

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use common::{AccountId, AgentId, HubError, Result};
use mem_core::{
    AccountRecord, BackupRecord, CodeChangeKind, CodingRevision, CodingStore, CodingThread,
    CognitiveLayer, CognitiveProposalRecord, CognitiveProposalStatus, CollectionBinding,
    CollectionStore, EvidenceSourceKind, EvidenceStore, ExperienceChangeKind, ExperienceStatus,
    ExperienceStore, ExperienceVersion, IntegrationKind, IntegrationProposal, MemoryCollection,
    MemoryEntry, MemoryEvidence, MemoryExperience, MemoryFilter, MemoryKind, MemoryRelation,
    MemoryRelationKind, MemoryScene, MemoryStatus, MemoryTask, MemoryTaskExperience, MemoryVersion,
    Namespace, NamespaceGrantRecord, NamespaceInfo, NewCodingRevision, NewCodingThread,
    NewEvidence, NewExperienceVersion, NewMemoryExperience, NewMemoryRelation, NewMemoryTask,
    ProposalStatus, RelationStore, Scored, SessionRecord, TaskStatus, TaskStore, VersionChange,
};
use sqlx::postgres::{PgConnection, PgPool, PgPoolOptions, PgRow};
use sqlx::{Connection as _, Row};
use uuid::Uuid;

/// 行映射辅助
mod mapping {
    use super::*;

    pub fn parse_kind(s: &str) -> Result<MemoryKind> {
        MemoryKind::parse(s)
            .ok_or_else(|| HubError::Internal(anyhow::anyhow!("未知记忆类型: {}", s)))
    }

    pub fn parse_status(s: &str) -> Result<MemoryStatus> {
        MemoryStatus::parse(s)
            .ok_or_else(|| HubError::Internal(anyhow::anyhow!("未知记忆状态: {}", s)))
    }

    pub fn parse_scene(s: &str) -> Result<MemoryScene> {
        MemoryScene::parse(s)
            .ok_or_else(|| HubError::Internal(anyhow::anyhow!("未知记忆场景: {}", s)))
    }

    pub fn parse_layer(s: &str) -> Result<CognitiveLayer> {
        CognitiveLayer::parse(s)
            .ok_or_else(|| HubError::Internal(anyhow::anyhow!("未知认知分层: {}", s)))
    }

    pub fn parse_relation_kind(s: &str) -> Result<MemoryRelationKind> {
        MemoryRelationKind::parse(s)
            .ok_or_else(|| HubError::Internal(anyhow::anyhow!("未知关系类型: {}", s)))
    }

    pub fn parse_source_kind(s: &str) -> Result<EvidenceSourceKind> {
        EvidenceSourceKind::parse(s)
            .ok_or_else(|| HubError::Internal(anyhow::anyhow!("未知证据来源类型: {}", s)))
    }

    pub fn parse_task_status(s: &str) -> Result<TaskStatus> {
        TaskStatus::parse(s)
            .ok_or_else(|| HubError::Internal(anyhow::anyhow!("未知任务状态: {}", s)))
    }

    pub fn parse_experience_status(s: &str) -> Result<ExperienceStatus> {
        ExperienceStatus::parse(s)
            .ok_or_else(|| HubError::Internal(anyhow::anyhow!("未知经验状态: {}", s)))
    }

    pub fn parse_change_kind(s: &str) -> Result<CodeChangeKind> {
        CodeChangeKind::parse(s)
            .ok_or_else(|| HubError::Internal(anyhow::anyhow!("未知代码变更类型: {}", s)))
    }

    pub fn parse_integration_kind(s: &str) -> Result<IntegrationKind> {
        match s {
            "duplicate" => Ok(IntegrationKind::Duplicate),
            "conflict" => Ok(IntegrationKind::Conflict),
            other => Err(HubError::Internal(anyhow::anyhow!(
                "未知提案类型: {}",
                other
            ))),
        }
    }

    pub fn parse_proposal_status(s: &str) -> Result<ProposalStatus> {
        match s {
            "open" => Ok(ProposalStatus::Open),
            "merged" => Ok(ProposalStatus::Merged),
            "dismissed" => Ok(ProposalStatus::Dismissed),
            "expired" => Ok(ProposalStatus::Expired),
            other => Err(HubError::Internal(anyhow::anyhow!(
                "未知提案状态: {}",
                other
            ))),
        }
    }

    pub fn parse_version_change(s: &str) -> Result<VersionChange> {
        match s {
            "created" => Ok(VersionChange::Created),
            "updated" => Ok(VersionChange::Updated),
            "archived" => Ok(VersionChange::Archived),
            "merged" => Ok(VersionChange::Merged),
            other => Err(HubError::Internal(anyhow::anyhow!(
                "未知变更类型: {}",
                other
            ))),
        }
    }

    /// 查询向量 → `halfvec` 二进制参数（迁移 015 的 halfvec(1024) 表达式索引配套）。
    /// 半精度舍入在客户端完成，与 SQL 侧 `embedding::halfvec(1024)` 的结果一致，
    /// 免去每次查询 ~12KB 文本的序列化与解析。
    pub fn halfvec_param(v: &[f32]) -> pgvector::HalfVector {
        pgvector::HalfVector::from_f32_slice(v)
    }

    /// 写入向量 → `vector` 二进制参数（INSERT / UPDATE 共用）。
    pub fn vector_param(v: &[f32]) -> pgvector::Vector {
        pgvector::Vector::from(v.to_vec())
    }
}

/// SQL 错误 → HubError 的统一映射
pub trait PgResultExt<T> {
    fn pg(self) -> Result<T>;
}

impl<T> PgResultExt<T> for std::result::Result<T, sqlx::Error> {
    fn pg(self) -> Result<T> {
        self.map_err(|e| {
            // 唯一键冲突 → Conflict；未找到 → NotFound；其余 → Internal
            match &e {
                sqlx::Error::Database(db) => {
                    let code = db.code().unwrap_or_default();
                    match code.as_ref() {
                        "23505" => HubError::Conflict,
                        "23503" => HubError::InvalidArgument("外键约束不满足".to_string()),
                        _ => HubError::Internal(anyhow::anyhow!("数据库错误: {}", db.message())),
                    }
                }
                sqlx::Error::RowNotFound => HubError::NotFound,
                other => HubError::Internal(anyhow::anyhow!("数据库查询失败: {}", other)),
            }
        })
    }
}

/// 迁移执行器的事务级咨询锁 key（ASCII "HUBMIGR1"，任意固定 i64 即可，
/// 只需与库内其他 advisory lock 用法不撞车——本库目前仅此一处）。
/// 多进程（滚动发布/多副本）同时启动时，并发方在 `pg_advisory_xact_lock`
/// 上排队而非重复执行 DDL；否则后提交者撞 `_hub_migrations` 主键（23505
/// → `Conflict`），`connect()` 直接失败、服务起不来。
const MIGRATION_ADVISORY_LOCK_KEY: i64 = 0x4855_424D_4947_5231;

/// 内嵌迁移清单：按文件名（即版本号）顺序应用
pub const MIGRATIONS: &[(&str, &str)] = &[
    (
        "20261011000001_initial_schema",
        include_str!("../../migrations/20261011000001_initial_schema.sql"),
    ),
    (
        "20261012000002_accounts_sandbox",
        include_str!("../../migrations/20261012000002_accounts_sandbox.sql"),
    ),
    (
        "20261013000003_conditional_key_unique",
        include_str!("../../migrations/20261013000003_conditional_key_unique.sql"),
    ),
    (
        "20261013000004_query_indexes",
        include_str!("../../migrations/20261013000004_query_indexes.sql"),
    ),
    (
        "20261013000005_cjk_bigram_fts",
        include_str!("../../migrations/20261013000005_cjk_bigram_fts.sql"),
    ),
    (
        "20261018000006_cognitive_evidence_and_relations",
        include_str!("../../migrations/20261018000006_cognitive_evidence_and_relations.sql"),
    ),
    (
        "20261018000007_working_memory_tasks_and_experiences",
        include_str!("../../migrations/20261018000007_working_memory_tasks_and_experiences.sql"),
    ),
    (
        "20261018000008_coding_memory_revisions",
        include_str!("../../migrations/20261018000008_coding_memory_revisions.sql"),
    ),
    (
        "20261019000009_composite_namespace_fk_hardening",
        include_str!("../../migrations/20261019000009_composite_namespace_fk_hardening.sql"),
    ),
    (
        "20261019000010_evidence_retention",
        include_str!("../../migrations/20261019000010_evidence_retention.sql"),
    ),
    (
        "20261021000011_consolidation_proposal_metadata",
        include_str!("../../migrations/20261021000011_consolidation_proposal_metadata.sql"),
    ),
    (
        "20261025000012_cognitive_proposals",
        include_str!("../../migrations/20261025000012_cognitive_proposals.sql"),
    ),
    (
        "20261025000013_fts_retokenize_marker",
        include_str!("../../migrations/20261025000013_fts_retokenize_marker.sql"),
    ),
    (
        "20261027000014_memories_ns_updated_index",
        include_str!("../../migrations/20261027000014_memories_ns_updated_index.sql"),
    ),
    (
        "20261027000015_vector_index_halfvec_partial",
        include_str!("../../migrations/20261027000015_vector_index_halfvec_partial.sql"),
    ),
    (
        "20261028000016_experience_asset_lifecycle",
        include_str!("../../migrations/20261028000016_experience_asset_lifecycle.sql"),
    ),
    (
        "20261028000017_memory_collections",
        include_str!("../../migrations/20261028000017_memory_collections.sql"),
    ),
    (
        "20261028000018_proposal_open_uniqueness",
        include_str!("../../migrations/20261028000018_proposal_open_uniqueness.sql"),
    ),
];

/// PostgreSQL 四合一存储
#[derive(Clone)]
pub struct PostgresHubStore {
    pool: PgPool,
}

// ===== 迁移语句切分 =====

/// 按语句切分迁移脚本。感知单引号字符串、dollar-quoted 块（`$$…$$` / `$tag$…$tag$`）
/// 与 `--` / `/* */` 注释，这些结构内部的分号不作为语句边界；
/// 返回的片段保留结尾分号，便于整段回读排查。
fn split_sql_statements(sql: &str) -> Vec<&str> {
    let b = sql.as_bytes();
    let mut stmts = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    let mut in_single_quote = false;
    let mut dollar_tag: Option<&[u8]> = None;

    while i < b.len() {
        if let Some(tag) = dollar_tag {
            if b[i..].starts_with(tag) {
                i += tag.len();
                dollar_tag = None;
            } else {
                i += 1;
            }
        } else if in_single_quote {
            if b[i] == b'\'' {
                in_single_quote = false;
            }
            i += 1;
        } else if b[i] == b'\'' {
            in_single_quote = true;
            i += 1;
        } else if b[i] == b'$' {
            match parse_dollar_tag(b, i) {
                Some(tag) => {
                    dollar_tag = Some(tag);
                    i += tag.len();
                }
                None => i += 1,
            }
        } else if b[i] == b';' {
            stmts.push(&sql[start..=i]);
            i += 1;
            start = i;
        } else if b[i] == b'-' && i + 1 < b.len() && b[i + 1] == b'-' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
        } else {
            i += 1;
        }
    }
    if start < b.len() {
        let remainder = &sql[start..];
        if !remainder.trim().is_empty() {
            stmts.push(remainder);
        }
    }
    stmts
}

/// 识别位置 `i` 处的 dollar-quote 前导标记（`$$` 或 `$tag$`），返回含首尾 `$` 的完整标记；
/// `$1` 这类非标记情形返回 None
fn parse_dollar_tag(b: &[u8], i: usize) -> Option<&[u8]> {
    let mut j = i + 1;
    while j < b.len() && (b[j] == b'_' || b[j].is_ascii_alphanumeric()) {
        j += 1;
    }
    if j < b.len() && b[j] == b'$' {
        Some(&b[i..=j])
    } else {
        None
    }
}

/// 统一的 tsv_tokens 生成入口。
///
/// 写路径（insert / insert_initial / apply_update_on）与存量回填
/// （retokenized_updates）都必须经由此函数取词，禁止各自直连分词器——
/// 一旦某处绕开，索引词法就会与写路径漂移（迁移 005 的教训）。
fn tsv_tokens_for(content: &str) -> String {
    mem_core::tokenize_for_fts(content)
}

/// FTS 回填纯逻辑：待重分词行 (id, content) → (id, 与写路径同源的重算词元流)。
/// 单独成函数以便无库单测：构造迁移 005 受害行（tsv_tokens=原文），
/// 断言重算结果与写路径对同 content 的产出完全一致。
fn retokenized_updates(rows: Vec<(Uuid, String)>) -> Vec<(Uuid, String)> {
    rows.into_iter()
        .map(|(id, content)| (id, tsv_tokens_for(&content)))
        .collect()
}

impl PostgresHubStore {
    /// 连接并执行迁移
    ///
    /// `statement_timeout_ms` 为 0 时不设置服务器端语句超时。连接池另设
    /// `idle_timeout`/`max_lifetime`：没有它们，云端 LB 切断的空闲连接会长期
    /// 占据池配额，而一条被锁等待拖住的语句会一直占着坑位。
    /// 比较 pgvector 版本号（形如 `0.8.0`）是否不低于给定版本
    fn pgvector_version_at_least(version: &str, required: (u32, u32, u32)) -> bool {
        let parse = |s: &str| s.trim().parse::<u32>().unwrap_or(0);
        let mut parts = version.split('.').map(parse);
        let actual = (
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
        );
        actual >= required
    }

    /// 探测已安装的 pgvector 是否支持 HNSW 迭代索引扫描（>= 0.8.0）
    ///
    /// 带 WHERE 过滤的 ANN 查询在 HNSW 上默认"先扫描后过滤"，可能返回不足 k 条；
    /// 0.8.0 起可用 `hnsw.iterative_scan` 让索引继续扫描直到凑满结果。
    /// 探测使用独立连接，失败（如全新库尚未建扩展）按不支持处理。
    async fn probe_hnsw_iterative_scan(database_url: &str) -> bool {
        let mut conn = match PgConnection::connect(database_url).await {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(error = %e, "pgvector 版本探测建连失败，按不支持迭代索引扫描处理");
                return false;
            }
        };
        let version: Option<String> =
            sqlx::query_scalar("select extversion from pg_extension where extname = 'vector'")
                .fetch_optional(&mut conn)
                .await
                .unwrap_or(None);
        match version {
            Some(v) => Self::pgvector_version_at_least(&v, (0, 8, 0)),
            None => false,
        }
    }

    pub async fn connect(
        database_url: &str,
        max_connections: u32,
        statement_timeout_ms: u64,
    ) -> Result<Self> {
        let mut builder = PgPoolOptions::new()
            .max_connections(max_connections.max(1))
            .min_connections(0)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .idle_timeout(std::time::Duration::from_secs(120))
            .max_lifetime(std::time::Duration::from_secs(1800));

        // 每连接会话设置：语句超时 +（pgvector >= 0.8）HNSW 迭代索引扫描
        let hnsw_iterative = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
            Self::probe_hnsw_iterative_scan(database_url).await,
        ));
        {
            let flag = hnsw_iterative.clone();
            builder = builder.after_connect(move |conn, _| {
                let flag = flag.clone();
                Box::pin(async move {
                    if statement_timeout_ms > 0 {
                        sqlx::query(&format!("set statement_timeout = '{statement_timeout_ms}'"))
                            .execute(&mut *conn)
                            .await?;
                    }
                    if flag.load(std::sync::atomic::Ordering::Relaxed) {
                        sqlx::query("set hnsw.iterative_scan = 'relaxed_order'")
                            .execute(&mut *conn)
                            .await?;
                        // HNSW 图探索宽度（默认 40）对候选池 vec_k=100 过窄：
                        // 迭代扫描下 ef_search 决定单轮探索量，过窄会为凑满 k
                        // 反复回扫。flag 已保证 pgvector >= 0.8，ef_search（0.5
                        // 引入）必可用；召回不足时先调这里，再考虑 vec_k。
                        sqlx::query("set hnsw.ef_search = 100")
                            .execute(&mut *conn)
                            .await?;
                    }
                    Ok(())
                })
            });
        }

        let pool = builder.connect(database_url).await.pg()?;

        let store = Self { pool };
        store.run_migrations().await?;
        store.backfill_fts_tokens().await?;

        // 迁移可能刚创建 vector 扩展：再探测一次并置位（对之后新建的连接生效）
        if !hnsw_iterative.load(std::sync::atomic::Ordering::Relaxed)
            && Self::probe_hnsw_iterative_scan(database_url).await
        {
            hnsw_iterative.store(true, std::sync::atomic::Ordering::Relaxed);
            tracing::info!("pgvector 支持 HNSW 迭代索引扫描：已对新建连接启用（重启后全量生效）");
        } else if hnsw_iterative.load(std::sync::atomic::Ordering::Relaxed) {
            tracing::info!(
                "已启用 pgvector HNSW 迭代索引扫描（hnsw.iterative_scan = relaxed_order）"
            );
        } else {
            tracing::info!(
                "未启用 pgvector HNSW 迭代索引扫描（需要 pgvector >= 0.8.0）：过滤型向量检索可能返回不足 k 条"
            );
        }

        Ok(store)
    }

    /// 连通性探测
    pub async fn ping(&self) -> Result<()> {
        sqlx::query("select 1").execute(&self.pool).await.pg()?;
        Ok(())
    }

    /// 共享连接池句柄：让同一进程内的其他组件（如限流桶）复用同一池，
    /// 而不是每个组件再开一份 `max_connections` 预算。
    pub fn pool(&self) -> PgPool {
        self.pool.clone()
    }

    /// 应用未执行过的迁移（版本化账本）。
    ///
    /// 每个迁移在独立事务内：先取 `pg_advisory_xact_lock(MIGRATION_ADVISORY_LOCK_KEY)`，
    /// 再查账本——查账与 DDL/记账同处锁保护下，消除 check-then-act 竞态。
    /// 并发进程在锁上排队，拿到锁后看到账本已记录即提交空事务跳过；
    /// 锁随事务结束自动释放，无需手动 unlock。
    async fn run_migrations(&self) -> Result<()> {
        // 账本建表与首个迁移同处一把锁下：并发进程同时 create table
        // 虽有 if not exists，但 PG 的 create table 并非完全无竞态
        // （并行建表/建索引可能撞 pg_type 唯一键 23505），一并串行化。
        let mut tx = self.pool.begin().await.pg()?;
        sqlx::query("set local statement_timeout = 0")
            .execute(&mut *tx)
            .await
            .pg()?;
        sqlx::query("select pg_advisory_xact_lock($1)")
            .bind(MIGRATION_ADVISORY_LOCK_KEY)
            .execute(&mut *tx)
            .await
            .pg()?;
        sqlx::query(
            r#"
            create table if not exists _hub_migrations (
              version text primary key,
              applied_at timestamptz not null default now()
            )
            "#,
        )
        .execute(&mut *tx)
        .await
        .pg()?;
        tx.commit().await.pg()?;

        for (version, sql) in MIGRATIONS {
            let mut tx = self.pool.begin().await.pg()?;
            // 迁移可能包含索引构建，远超常规语句超时；本事务内关闭超时限制
            sqlx::query("set local statement_timeout = 0")
                .execute(&mut *tx)
                .await
                .pg()?;
            // 事务级咨询锁：串行化并发进程的迁移执行。
            // set local 必须先于锁获取，避免锁等待本身被超时打断。
            sqlx::query("select pg_advisory_xact_lock($1)")
                .bind(MIGRATION_ADVISORY_LOCK_KEY)
                .execute(&mut *tx)
                .await
                .pg()?;
            // 账本检查在锁内重读：拿到锁时若前一个进程已记账，此处即命中
            let applied: Option<String> =
                sqlx::query_scalar("select version from _hub_migrations where version = $1")
                    .bind(version)
                    .fetch_optional(&mut *tx)
                    .await
                    .pg()?;
            if applied.is_some() {
                // 已应用：提交空事务（同时释放 xact 锁）并跳过
                tx.commit().await.pg()?;
                continue;
            }

            // 迁移脚本按语句顺序整体执行；DDL 幂等（if not exists）
            for stmt in split_sql_statements(sql) {
                let trimmed = stmt.trim();
                if trimmed.is_empty() {
                    continue;
                }
                sqlx::query(trimmed).execute(&mut *tx).await.pg()?;
            }
            sqlx::query("insert into _hub_migrations (version) values ($1)")
                .bind(version)
                .execute(&mut *tx)
                .await
                .pg()?;
            tx.commit().await.pg()?;
        }
        Ok(())
    }

    /// FTS 存量重分词回填（迁移 013 的配套步骤）。
    ///
    /// 迁移 005 曾把原文直接回填进 tsv_tokens，迁移 013 已把受害行标记为
    /// `fts_retokenize_pending`；本方法在 connect() 迁移完成后分批重分词：
    /// 每批 500 行、键集分页（id > 游标）、批内单事务——短事务行级锁，
    /// 回填期间服务读写不受影响。批处理中断可续跑：已完成的行标记被清除，
    /// 剩余行下次启动重新处理。
    async fn backfill_fts_tokens(&self) -> Result<()> {
        let pending: i64 =
            sqlx::query_scalar("select count(*) from memories where fts_retokenize_pending")
                .fetch_one(&self.pool)
                .await
                .pg()?;
        if pending == 0 {
            tracing::info!("FTS 回填：0 行待处理，跳过");
            return Ok(());
        }
        tracing::info!("FTS 回填：{pending} 行待处理，开始分批重分词");

        let mut cursor: Option<Uuid> = None;
        let mut done: usize = 0;
        loop {
            let rows: Vec<(Uuid, String)> = match cursor {
                Some(c) => sqlx::query_as(
                    "select id, content from memories \
                     where fts_retokenize_pending and id > $1 \
                     order by id limit 500",
                )
                .bind(c)
                .fetch_all(&self.pool)
                .await
                .pg()?,
                None => sqlx::query_as(
                    "select id, content from memories \
                     where fts_retokenize_pending \
                     order by id limit 500",
                )
                .fetch_all(&self.pool)
                .await
                .pg()?,
            };
            if rows.is_empty() {
                break;
            }
            let updates = retokenized_updates(rows);
            let mut tx = self.pool.begin().await.pg()?;
            for (id, tokens) in &updates {
                // guard on fts_retokenize_pending：SELECT 到 UPDATE 之间该行若已被
                // apply_update_on 重写（含清标记），此处跳过，避免用旧 content
                // 的分词覆盖新 content 的词元流；两写者取锁有先有后，后到者
                // 的值总是基于更新后的 content，最终一致。
                sqlx::query(
                    "update memories set tsv_tokens = $2, fts_retokenize_pending = false \
                     where id = $1 and fts_retokenize_pending",
                )
                .bind(id)
                .bind(tokens)
                .execute(&mut *tx)
                .await
                .pg()?;
            }
            tx.commit().await.pg()?;
            done += updates.len();
            cursor = updates.last().map(|(id, _)| *id);
        }
        tracing::info!("FTS 回填完成：共重分词 {done} 行");
        Ok(())
    }

    // ===== 行映射 =====

    /// 统一把 `MemoryFilter` 条件压入 SQL（list / count / similar 共用一份实现，
    /// 避免三处过滤语义漂移；带查询词时按词元 OR 部分匹配，交由相关性排序与打分区分）
    fn push_memory_filters(qb: &mut sqlx::QueryBuilder<'_, sqlx::Postgres>, filter: &MemoryFilter) {
        if let Some(ns) = &filter.namespace {
            qb.push(" and namespace = ").push_bind(ns.clone());
        }
        if let Some(kinds) = &filter.kinds {
            qb.push(" and kind = any(")
                .push_bind(
                    kinds
                        .iter()
                        .map(|k| k.as_str().to_string())
                        .collect::<Vec<_>>(),
                )
                .push(")");
        }
        if let Some(statuses) = &filter.statuses {
            qb.push(" and status = any(")
                .push_bind(
                    statuses
                        .iter()
                        .map(|s| s.as_str().to_string())
                        .collect::<Vec<_>>(),
                )
                .push(")");
        }
        if let Some(tags) = &filter.tags {
            qb.push(" and tags @> ").push_bind(tags.clone());
        }
        if let Some(agent) = &filter.source_agent {
            qb.push(" and source_agent = ").push_bind(agent.clone());
        }
        if let Some(key) = &filter.key {
            qb.push(" and key = ").push_bind(key.clone());
        }
        if let Some(text) = &filter.text_contains {
            match mem_core::cjk::fts_or_query(text) {
                Some(or_query) => {
                    qb.push(" and to_tsvector('simple', tsv_tokens) @@ to_tsquery('simple', ")
                        .push_bind(or_query)
                        .push(")");
                }
                // 查询词元全部是低信息量功能词：词法路不应命中任何文档
                None => {
                    qb.push(" and false");
                }
            }
        }
        if let Some(scene) = &filter.scene {
            qb.push(" and scene = ").push_bind(scene.as_str());
        }
        if let Some(layer) = &filter.layer {
            qb.push(" and layer = ").push_bind(layer.as_str());
        }
        if let Some(valid_at) = &filter.valid_at {
            qb.push(" and (valid_from is null or valid_from <= ")
                .push_bind(*valid_at)
                .push(") and (valid_until is null or valid_until > ")
                .push_bind(*valid_at)
                .push(")");
        }
        if let Some((ts, id)) = &filter.before {
            qb.push(" and (updated_at, id) < (")
                .push_bind(*ts)
                .push(", ")
                .push_bind(*id)
                .push(")");
        }
    }

    /// 瘦版行映射：跳过 `embedding` 列解析（对应 [`Self::ENTRY_COLUMNS_NO_EMBEDDING`]）。
    /// 读路径大多不需要向量，避免 ~4KB/行 的 embedding 传输与解析开销；
    /// 需要向量的路径（MMR 二次取回）走 `embeddings_by_ids` 二进制读回。
    fn entry_from_row_slim(r: &PgRow) -> Result<MemoryEntry> {
        let kind: String = r.try_get("kind").pg()?;
        let status: String = r.try_get("status").pg()?;
        let scene_str: Option<String> = r.try_get("scene").pg()?;
        let layer_str: Option<String> = r.try_get("layer").pg()?;
        let scene = match scene_str.as_deref() {
            Some(s) => Some(mapping::parse_scene(s)?),
            None => None,
        };
        let layer = match layer_str.as_deref() {
            Some(s) => Some(mapping::parse_layer(s)?),
            None => None,
        };
        let valid_from: Option<DateTime<Utc>> = r.try_get("valid_from").pg()?;
        let valid_until: Option<DateTime<Utc>> = r.try_get("valid_until").pg()?;
        let occurred_at: Option<DateTime<Utc>> = r.try_get("occurred_at").pg()?;

        Ok(MemoryEntry {
            id: r.try_get("id").pg()?,
            namespace: r.try_get("namespace").pg()?,
            key: r.try_get("key").pg()?,
            kind: mapping::parse_kind(&kind)?,
            content: r.try_get("content").pg()?,
            tags: r.try_get("tags").pg()?,
            metadata: r.try_get("metadata").pg()?,
            embedding: None,
            embedding_model: r.try_get("embedding_model").pg()?,
            source_agent: r.try_get("source_agent").pg()?,
            confidence: r.try_get("confidence").pg()?,
            status: mapping::parse_status(&status)?,
            version: r.try_get("version").pg()?,
            expires_at: r.try_get("expires_at").pg()?,
            merged_into: r.try_get("merged_into").pg()?,
            created_at: r.try_get("created_at").pg()?,
            updated_at: r.try_get("updated_at").pg()?,
            scene,
            layer,
            valid_from,
            valid_until,
            occurred_at,
        })
    }

    fn backup_from_row(r: &PgRow) -> Result<BackupRecord> {
        Ok(BackupRecord {
            id: r.try_get("id").pg()?,
            namespace: r.try_get("namespace").pg()?,
            created_by: r.try_get("created_by").pg()?,
            note: r.try_get("note").pg()?,
            entry_count: r.try_get::<i32, _>("entry_count").pg()? as usize,
            checksum: r.try_get("checksum").pg()?,
            payload: r.try_get("payload").pg()?,
            retain_until: r.try_get("retain_until").pg()?,
            created_at: r.try_get("created_at").pg()?,
        })
    }

    /// 摘要视图行映射：与 [`Self::backup_from_row`] 同构，但不带 `payload` 列。
    /// 列举快照只需要元数据，负载体积是整份命名空间的 JSONB。
    fn backup_meta_from_row(r: &PgRow) -> Result<BackupRecord> {
        Ok(BackupRecord {
            id: r.try_get("id").pg()?,
            namespace: r.try_get("namespace").pg()?,
            created_by: r.try_get("created_by").pg()?,
            note: r.try_get("note").pg()?,
            entry_count: r.try_get::<i32, _>("entry_count").pg()? as usize,
            checksum: r.try_get("checksum").pg()?,
            payload: serde_json::Value::Null,
            retain_until: r.try_get("retain_until").pg()?,
            created_at: r.try_get("created_at").pg()?,
        })
    }

    fn evidence_from_row(r: &PgRow) -> Result<MemoryEvidence> {
        let source_kind_str: String = r.try_get("source_kind").pg()?;
        let source_kind = mapping::parse_source_kind(&source_kind_str)?;
        Ok(MemoryEvidence {
            id: r.try_get("id").pg()?,
            namespace: r.try_get("namespace").pg()?,
            source_kind,
            source_ref: r.try_get("source_ref").pg()?,
            content_hash: r.try_get("content_hash").pg()?,
            excerpt: r.try_get("excerpt").pg()?,
            occurred_at: r.try_get("occurred_at").pg()?,
            created_at: r.try_get("created_at").pg()?,
            expires_at: r.try_get("expires_at").pg()?,
        })
    }

    fn relation_from_row(r: &PgRow) -> Result<MemoryRelation> {
        let kind_str: String = r.try_get("relation_kind").pg()?;
        let relation_kind = mapping::parse_relation_kind(&kind_str)?;
        Ok(MemoryRelation {
            // 迁移 009 后 namespace 为 NOT NULL；领域类型保留 Option 仅为
            // 兼容旧备份载荷的反序列化，存储行恒有值
            namespace: Some(r.try_get("namespace").pg()?),
            from_memory_id: r.try_get("from_memory_id").pg()?,
            to_memory_id: r.try_get("to_memory_id").pg()?,
            relation_kind,
            confidence: r.try_get::<f32, _>("confidence").pg()?,
            source_evidence_id: r.try_get("source_evidence_id").pg()?,
            created_at: r.try_get("created_at").pg()?,
        })
    }

    fn task_from_row(r: &PgRow) -> Result<MemoryTask> {
        let scene_str: String = r.try_get("scene").pg()?;
        let scene = mapping::parse_scene(&scene_str)?;
        let status_str: String = r.try_get("status").pg()?;
        let status = mapping::parse_task_status(&status_str)?;
        Ok(MemoryTask {
            id: r.try_get("id").pg()?,
            namespace: r.try_get("namespace").pg()?,
            scene,
            task: r.try_get("task").pg()?,
            status,
            started_at: r.try_get("started_at").pg()?,
            finished_at: r.try_get("finished_at").pg()?,
            result_summary: r.try_get("result_summary").pg()?,
            failure_reason: r.try_get("failure_reason").pg()?,
            source_evidence_id: r.try_get("source_evidence_id").pg()?,
            metadata: r.try_get("metadata").pg()?,
        })
    }

    fn experience_from_row(r: &PgRow) -> Result<MemoryExperience> {
        let status_str: String = r.try_get("status").pg()?;
        let status = mapping::parse_experience_status(&status_str)?;
        Ok(MemoryExperience {
            id: r.try_get("id").pg()?,
            namespace: r.try_get("namespace").pg()?,
            task: r.try_get("task").pg()?,
            search_keys: r.try_get("search_keys").pg()?,
            solution: r.try_get("solution").pg()?,
            preconditions: r.try_get("preconditions").pg()?,
            boundary_envs: r.try_get("boundary_envs").pg()?,
            failure_modes: r.try_get("failure_modes").pg()?,
            validation: r.try_get("validation").pg()?,
            confidence: r.try_get::<f32, _>("confidence").pg()?,
            status,
            feedback_score: r.try_get::<f32, _>("feedback_score").pg()?,
            version: r.try_get("version").pg()?,
            superseded_by: r.try_get("superseded_by").pg()?,
            verified_at: r.try_get("verified_at").pg()?,
            verified_by: r.try_get("verified_by").pg()?,
            verification_note: r.try_get("verification_note").pg()?,
            status_reason: r.try_get("status_reason").pg()?,
            source_count: r.try_get("source_count").pg()?,
            created_at: r.try_get("created_at").pg()?,
            updated_at: r.try_get("updated_at").pg()?,
        })
    }

    fn experience_version_from_row(r: &PgRow) -> Result<ExperienceVersion> {
        let status_str: String = r.try_get("status").pg()?;
        let change_str: String = r.try_get("change_kind").pg()?;
        Ok(ExperienceVersion {
            id: r.try_get("id").pg()?,
            experience_id: r.try_get("experience_id").pg()?,
            namespace: r.try_get("namespace").pg()?,
            version: r.try_get("version").pg()?,
            snapshot: r.try_get("snapshot").pg()?,
            change_kind: ExperienceChangeKind::parse(&change_str).ok_or_else(|| {
                HubError::Internal(anyhow::anyhow!("未知经验变更类型: {}", change_str))
            })?,
            changed_by: r.try_get("changed_by").pg()?,
            status: mapping::parse_experience_status(&status_str)?,
            note: r.try_get("note").pg()?,
            changed_at: r.try_get("changed_at").pg()?,
        })
    }

    fn collection_from_row(r: &PgRow) -> Result<MemoryCollection> {
        let filter_value: serde_json::Value = r.try_get("filter").pg()?;
        let filter = serde_json::from_value(filter_value)
            .map_err(|e| HubError::Internal(anyhow::anyhow!("集合过滤器解析失败: {}", e)))?;
        Ok(MemoryCollection {
            id: r.try_get("id").pg()?,
            account_id: r.try_get("account_id").pg()?,
            name: r.try_get("name").pg()?,
            description: r.try_get("description").pg()?,
            filter,
            created_by: r.try_get("created_by").pg()?,
            created_at: r.try_get("created_at").pg()?,
            updated_at: r.try_get("updated_at").pg()?,
        })
    }

    fn collection_binding_from_row(r: &PgRow) -> Result<CollectionBinding> {
        Ok(CollectionBinding {
            collection_id: r.try_get("collection_id").pg()?,
            agent_id: r.try_get("agent_id").pg()?,
            created_by: r.try_get("created_by").pg()?,
            created_at: r.try_get("created_at").pg()?,
        })
    }

    fn task_experience_from_row(r: &PgRow) -> Result<MemoryTaskExperience> {
        Ok(MemoryTaskExperience {
            // 迁移 009 后 namespace 为 NOT NULL；Option 仅为兼容旧载荷反序列化
            namespace: Some(r.try_get("namespace").pg()?),
            task_id: r.try_get("task_id").pg()?,
            experience_id: r.try_get("experience_id").pg()?,
            relation_kind: r.try_get("relation_kind").pg()?,
            created_at: r.try_get("created_at").pg()?,
        })
    }

    fn thread_from_row(r: &PgRow) -> Result<CodingThread> {
        Ok(CodingThread {
            id: r.try_get("id").pg()?,
            namespace: r.try_get("namespace").pg()?,
            repository: r.try_get("repository").pg()?,
            branch: r.try_get("branch").pg()?,
            task_id: r.try_get("task_id").pg()?,
            title: r.try_get("title").pg()?,
            description: r.try_get("description").pg()?,
            created_at: r.try_get("created_at").pg()?,
            updated_at: r.try_get("updated_at").pg()?,
        })
    }

    fn revision_from_row(r: &PgRow) -> Result<CodingRevision> {
        let change_kind_str: String = r.try_get("change_kind").pg()?;
        let change_kind = mapping::parse_change_kind(&change_kind_str)?;
        Ok(CodingRevision {
            id: r.try_get("id").pg()?,
            thread_id: r.try_get("thread_id").pg()?,
            namespace: r.try_get("namespace").pg()?,
            repository: r.try_get("repository").pg()?,
            branch: r.try_get("branch").pg()?,
            commit_sha: r.try_get("commit_sha").pg()?,
            parent_sha: r.try_get("parent_sha").pg()?,
            file_path: r.try_get("file_path").pg()?,
            change_kind,
            diff_content: r.try_get("diff_content").pg()?,
            diff_hash: r.try_get("diff_hash").pg()?,
            symbol_fingerprint: r.try_get("symbol_fingerprint").pg()?,
            is_committed: r.try_get("is_committed").pg()?,
            author: r.try_get("author").pg()?,
            occurred_at: r.try_get("occurred_at").pg()?,
            created_at: r.try_get("created_at").pg()?,
            source_evidence_id: r.try_get("source_evidence_id").pg()?,
            source_task_id: r.try_get("source_task_id").pg()?,
        })
    }

    /// 在给定连接上写回条目，返回受影响行数。
    async fn apply_update_on(
        conn: &mut PgConnection,
        entry: &MemoryEntry,
        expected_version: Option<i64>,
    ) -> Result<u64> {
        let embedding_param = entry.embedding.as_ref().map(|v| mapping::vector_param(v));
        let tsv_tokens = tsv_tokens_for(&entry.content);
        let guard = if expected_version.is_some() {
            " and version = $23"
        } else {
            ""
        };
        let sql = format!(
            r#"
            update memories set
              key = $3, kind = $4, content = $5, tags = $6, metadata = $7,
              embedding = $8::vector, embedding_model = $9, source_agent = $10,
              confidence = $11, status = $12, version = $13, expires_at = $14,
              merged_into = $15, updated_at = $16, tsv_tokens = $17,
              fts_retokenize_pending = false,
              scene = $18, layer = $19, valid_from = $20, valid_until = $21, occurred_at = $22
            where namespace = $1 and id = $2{guard}
            "#,
        );
        let mut query = sqlx::query(&sql)
            .bind(&entry.namespace)
            .bind(entry.id)
            .bind(&entry.key)
            .bind(entry.kind.as_str())
            .bind(&entry.content)
            .bind(&entry.tags)
            .bind(&entry.metadata)
            .bind(&embedding_param)
            .bind(&entry.embedding_model)
            .bind(&entry.source_agent)
            .bind(entry.confidence)
            .bind(entry.status.as_str())
            .bind(entry.version)
            .bind(entry.expires_at)
            .bind(entry.merged_into)
            .bind(entry.updated_at)
            .bind(&tsv_tokens)
            .bind(entry.scene.map(|s| s.as_str()))
            .bind(entry.layer.map(|l| l.as_str()))
            .bind(entry.valid_from)
            .bind(entry.valid_until)
            .bind(entry.occurred_at);
        if let Some(v) = expected_version {
            query = query.bind(v);
        }
        let res = query.execute(&mut *conn).await.pg()?;
        Ok(res.rows_affected())
    }

    /// 在给定连接上追加一条版本历史（主键冲突时保持幂等）
    async fn insert_version_on(conn: &mut PgConnection, v: &MemoryVersion) -> Result<()> {
        sqlx::query(
            r#"
            insert into memory_versions
              (memory_id, version, content, tags, metadata, confidence, changed_by, change_type, changed_at,
               scene, layer, valid_from, valid_until, occurred_at)
            values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
            on conflict (memory_id, version) do nothing
            "#,
        )
        .bind(v.memory_id)
        .bind(v.version)
        .bind(&v.content)
        .bind(&v.tags)
        .bind(&v.metadata)
        .bind(v.confidence)
        .bind(&v.changed_by)
        .bind(match v.change_type {
            VersionChange::Created => "created",
            VersionChange::Updated => "updated",
            VersionChange::Archived => "archived",
            VersionChange::Merged => "merged",
        })
        .bind(v.changed_at)
        .bind(v.scene.map(|s| s.as_str()))
        .bind(v.layer.map(|l| l.as_str()))
        .bind(v.valid_from)
        .bind(v.valid_until)
        .bind(v.occurred_at)
        .execute(&mut *conn)
        .await
        .pg()?;
        Ok(())
    }

    /// 在给定连接上插入新条目（INSERT 列清单与参数顺序的唯一来源）
    async fn insert_memory_on(conn: &mut PgConnection, entry: &MemoryEntry) -> Result<()> {
        let embedding_param = entry.embedding.as_ref().map(|v| mapping::vector_param(v));
        let tsv_tokens = tsv_tokens_for(&entry.content);
        sqlx::query(Self::MEMORY_INSERT_SQL)
            .bind(entry.id)
            .bind(&entry.namespace)
            .bind(&entry.key)
            .bind(entry.kind.as_str())
            .bind(&entry.content)
            .bind(&entry.tags)
            .bind(&entry.metadata)
            .bind(&embedding_param)
            .bind(&entry.embedding_model)
            .bind(&entry.source_agent)
            .bind(entry.confidence)
            .bind(entry.status.as_str())
            .bind(entry.version)
            .bind(entry.expires_at)
            .bind(entry.merged_into)
            .bind(entry.created_at)
            .bind(entry.updated_at)
            .bind(&tsv_tokens)
            .bind(entry.scene.map(|s| s.as_str()))
            .bind(entry.layer.map(|l| l.as_str()))
            .bind(entry.valid_from)
            .bind(entry.valid_until)
            .bind(entry.occurred_at)
            .execute(&mut *conn)
            .await
            .pg()?;
        Ok(())
    }

    fn proposal_from_row(r: &PgRow) -> Result<IntegrationProposal> {
        let kind: String = r.try_get("kind").pg()?;
        let status: String = r.try_get("status").pg()?;
        Ok(IntegrationProposal {
            id: r.try_get("id").pg()?,
            namespace: r.try_get("namespace").pg()?,
            kind: mapping::parse_integration_kind(&kind)?,
            member_ids: r.try_get("member_ids").pg()?,
            suggested_survivor: r.try_get("suggested_survivor").pg()?,
            similarity: r.try_get::<f32, _>("similarity").pg()?,
            status: mapping::parse_proposal_status(&status)?,
            resolution_note: r.try_get("resolution_note").pg()?,
            created_at: r.try_get("created_at").pg()?,
            resolved_at: r.try_get("resolved_at").pg()?,
            // 迁移 011 起 NOT NULL DEFAULT '{}'，所有 SELECT 均选出该列
            metadata: r.try_get("metadata").pg()?,
        })
    }

    /// 认知提案全列 SELECT（供行映射复用）：与
    /// `cognitive_proposal_from_row` 一一对应，列漂移时强类型解码直接报错
    const COGNITIVE_PROPOSAL_COLUMNS: &'static str =
        "id, namespace, candidate, source_evidence_id, status, rejection_reason, \
         resulting_memory_id, resolved_by, created_at, updated_at";

    fn cognitive_proposal_from_row(r: &PgRow) -> Result<CognitiveProposalRecord> {
        let status: String = r.try_get("status").pg()?;
        let candidate: serde_json::Value = r.try_get("candidate").pg()?;
        Ok(CognitiveProposalRecord {
            id: r.try_get("id").pg()?,
            namespace: r.try_get("namespace").pg()?,
            candidate: serde_json::from_value(candidate).map_err(|e| {
                HubError::Internal(anyhow::anyhow!("认知提案载荷反序列化失败: {}", e))
            })?,
            source_evidence_id: r.try_get("source_evidence_id").pg()?,
            status: CognitiveProposalStatus::parse(&status).ok_or_else(|| {
                HubError::Internal(anyhow::anyhow!("未知认知提案状态: {}", status))
            })?,
            rejection_reason: r.try_get("rejection_reason").pg()?,
            resulting_memory_id: r.try_get("resulting_memory_id").pg()?,
            resolved_by: r.try_get("resolved_by").pg()?,
            created_at: r.try_get("created_at").pg()?,
            updated_at: r.try_get("updated_at").pg()?,
        })
    }

    /// agents 表行映射：所有列在迁移 002 后均 NOT NULL（或查询自身计算得出）。
    /// `max_scopes` 中无法解析的 scope 字符串按缺失丢弃——scope 集合是授权
    /// 白名单，未知值不可能被授权通过，丢弃比整体报错更符合降级语义。
    fn agent_from_row(r: &PgRow) -> Result<mem_core::AgentRecord> {
        let max_scopes: Vec<String> = r.try_get("max_scopes").pg()?;
        Ok(mem_core::AgentRecord {
            agent_id: r.try_get("agent_id").pg()?,
            name: r.try_get("name").pg()?,
            kind: r.try_get("kind").pg()?,
            status: r.try_get("status").pg()?,
            owner_account_id: r.try_get("owner_account_id").pg()?,
            redirect_uris: r.try_get("redirect_uris").pg()?,
            max_scopes: max_scopes
                .iter()
                .filter_map(|s| common::Scope::parse(s))
                .collect(),
            confidential: r.try_get("confidential").pg()?,
            created_at: r.try_get("created_at").pg()?,
        })
    }

    /// 瘦版列清单：省略 embedding，适用于 list / count / get / similar /
    /// 备份摘要等不需要向量的读路径。single 条目获取也走瘦版——如需 embedding
    /// 再单次查询（embeddings_by_ids，二进制协议）。
    const ENTRY_COLUMNS_NO_EMBEDDING: &'static str =
        "id, namespace, key, kind, content, tags, metadata, \
         embedding_model, source_agent, confidence, status, \
         version, expires_at, merged_into, created_at, updated_at, \
         scene, layer, valid_from, valid_until, occurred_at";

    /// 新条目 INSERT：`insert` 与 `insert_initial` 共用同一列清单与参数顺序
    const MEMORY_INSERT_SQL: &'static str = r#"
        insert into memories
          (id, namespace, key, kind, content, tags, metadata, embedding, embedding_model,
           source_agent, confidence, status, version, expires_at, merged_into,
           created_at, updated_at, tsv_tokens, scene, layer, valid_from, valid_until, occurred_at)
        values
          ($1, $2, $3, $4, $5, $6, $7, $8::vector, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23)
        "#;

    /// `coding_revisions` 全列清单（INSERT / RETURNING / SELECT 共用）
    const REVISION_COLUMNS: &'static str = "id, thread_id, namespace, repository, branch, \
         commit_sha, parent_sha, file_path, change_kind, diff_content, diff_hash, \
         symbol_fingerprint, is_committed, author, occurred_at, created_at, \
         source_evidence_id, source_task_id";

    /// `coding_revisions` 的 18 个占位符，与 [`Self::REVISION_COLUMNS`] 一一对应
    const REVISION_INSERT_VALUES: &'static str =
        "$1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18";
}

#[cfg(test)]
mod tests;

#[cfg(all(test, feature = "pg-integration"))]
mod pg_integration;

mod access;
mod backup;
mod coding;
mod collections;
mod integration;
mod memory;
mod working;
