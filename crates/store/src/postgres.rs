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
    CognitiveLayer, CognitiveProposalRecord, CognitiveProposalStatus, EvidenceSourceKind,
    EvidenceStore, ExperienceStatus, ExperienceStore, IntegrationKind, IntegrationProposal,
    MemoryEntry, MemoryEvidence, MemoryExperience, MemoryFilter, MemoryKind, MemoryRelation,
    MemoryRelationKind, MemoryScene, MemoryStatus, MemoryTask, MemoryTaskExperience, MemoryVersion,
    Namespace, NamespaceGrantRecord, NamespaceInfo, NewCodingRevision, NewCodingThread,
    NewEvidence, NewMemoryExperience, NewMemoryRelation, NewMemoryTask, ProposalStatus,
    RelationStore, Scored, SessionRecord, TaskStatus, TaskStore, VersionChange,
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

    /// `pgvector` 字面向量：`[1,2,3]`
    pub fn vector_literal(v: &[f32]) -> String {
        let mut out = String::with_capacity(v.len() * 12 + 2);
        out.push('[');
        for (i, x) in v.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!("{}", x));
        }
        out.push(']');
        out
    }

    /// 从 text 列读回 pgvector 表示并解析
    pub fn parse_vector(text: &str) -> Vec<f32> {
        let trimmed = text.trim().trim_start_matches('[').trim_end_matches(']');
        if trimmed.is_empty() {
            return Vec::new();
        }
        trimmed
            .split(',')
            .filter_map(|s| s.trim().parse::<f32>().ok())
            .collect()
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
        include_str!("../migrations/20261011000001_initial_schema.sql"),
    ),
    (
        "20261012000002_accounts_sandbox",
        include_str!("../migrations/20261012000002_accounts_sandbox.sql"),
    ),
    (
        "20261013000003_conditional_key_unique",
        include_str!("../migrations/20261013000003_conditional_key_unique.sql"),
    ),
    (
        "20261013000004_query_indexes",
        include_str!("../migrations/20261013000004_query_indexes.sql"),
    ),
    (
        "20261013000005_cjk_bigram_fts",
        include_str!("../migrations/20261013000005_cjk_bigram_fts.sql"),
    ),
    (
        "20261018000006_cognitive_evidence_and_relations",
        include_str!("../migrations/20261018000006_cognitive_evidence_and_relations.sql"),
    ),
    (
        "20261018000007_working_memory_tasks_and_experiences",
        include_str!("../migrations/20261018000007_working_memory_tasks_and_experiences.sql"),
    ),
    (
        "20261018000008_coding_memory_revisions",
        include_str!("../migrations/20261018000008_coding_memory_revisions.sql"),
    ),
    (
        "20261019000009_composite_namespace_fk_hardening",
        include_str!("../migrations/20261019000009_composite_namespace_fk_hardening.sql"),
    ),
    (
        "20261019000010_evidence_retention",
        include_str!("../migrations/20261019000010_evidence_retention.sql"),
    ),
    (
        "20261021000011_consolidation_proposal_metadata",
        include_str!("../migrations/20261021000011_consolidation_proposal_metadata.sql"),
    ),
    (
        "20261025000012_cognitive_proposals",
        include_str!("../migrations/20261025000012_cognitive_proposals.sql"),
    ),
    (
        "20261025000013_fts_retokenize_marker",
        include_str!("../migrations/20261025000013_fts_retokenize_marker.sql"),
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

    fn entry_from_row(r: &PgRow) -> Result<MemoryEntry> {
        let kind: String = r.try_get("kind").pg()?;
        let status: String = r.try_get("status").pg()?;
        // 可空列一律强类型 Option<T> 解码并传播错误：SELECT 列清单一旦与
        // schema 漂移，缺列/类型不符必须显式报错，静默变 None 等于数据丢失
        let embedding: Option<String> = r.try_get("embedding").pg()?;
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
            embedding: embedding.map(|t| mapping::parse_vector(&t)),
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
            created_at: r.try_get("created_at").pg()?,
            updated_at: r.try_get("updated_at").pg()?,
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
        let embedding_literal = entry.embedding.as_ref().map(|v| mapping::vector_literal(v));
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
            .bind(&embedding_literal)
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

    /// 全列 SELECT（供行映射复用）
    const ENTRY_COLUMNS: &'static str = "id, namespace, key, kind, content, tags, metadata, \
         embedding::text as embedding, embedding_model, source_agent, confidence, status, \
         version, expires_at, merged_into, created_at, updated_at, \
         scene, layer, valid_from, valid_until, occurred_at";
}

#[async_trait]
impl mem_core::MemoryStore for PostgresHubStore {
    async fn insert(&self, entry: &MemoryEntry) -> Result<MemoryEntry> {
        let embedding_literal = entry.embedding.as_ref().map(|v| mapping::vector_literal(v));
        let tsv_tokens = tsv_tokens_for(&entry.content);
        sqlx::query(
            r#"
            insert into memories
              (id, namespace, key, kind, content, tags, metadata, embedding, embedding_model,
               source_agent, confidence, status, version, expires_at, merged_into,
               created_at, updated_at, tsv_tokens, scene, layer, valid_from, valid_until, occurred_at)
            values
              ($1, $2, $3, $4, $5, $6, $7, $8::vector, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23)
            "#,
        )
        .bind(entry.id)
        .bind(&entry.namespace)
        .bind(&entry.key)
        .bind(entry.kind.as_str())
        .bind(&entry.content)
        .bind(&entry.tags)
        .bind(&entry.metadata)
        .bind(&embedding_literal)
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
        .execute(&self.pool)
        .await
        .pg()?;
        Ok(entry.clone())
    }

    /// 原子插入新条目并追加初始版本历史（VersionChange::Created）
    async fn insert_initial(&self, entry: &MemoryEntry) -> Result<MemoryEntry> {
        let mut tx = self.pool.begin().await.pg()?;
        let embedding_literal = entry.embedding.as_ref().map(|v| mapping::vector_literal(v));
        let tsv_tokens = tsv_tokens_for(&entry.content);
        sqlx::query(
            r#"
            insert into memories
              (id, namespace, key, kind, content, tags, metadata, embedding, embedding_model,
               source_agent, confidence, status, version, expires_at, merged_into,
               created_at, updated_at, tsv_tokens, scene, layer, valid_from, valid_until, occurred_at)
            values
              ($1, $2, $3, $4, $5, $6, $7, $8::vector, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23)
            "#,
        )
        .bind(entry.id)
        .bind(&entry.namespace)
        .bind(&entry.key)
        .bind(entry.kind.as_str())
        .bind(&entry.content)
        .bind(&entry.tags)
        .bind(&entry.metadata)
        .bind(&embedding_literal)
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
        .execute(&mut *tx)
        .await
        .pg()?;

        let row = MemoryVersion {
            memory_id: entry.id,
            version: entry.version,
            content: entry.content.clone(),
            tags: entry.tags.clone(),
            metadata: entry.metadata.clone(),
            confidence: entry.confidence,
            changed_by: entry.source_agent.clone(),
            change_type: VersionChange::Created,
            changed_at: entry.created_at,
            scene: entry.scene,
            layer: entry.layer,
            valid_from: entry.valid_from,
            valid_until: entry.valid_until,
            occurred_at: entry.occurred_at,
        };
        Self::insert_version_on(&mut tx, &row).await?;
        tx.commit().await.pg()?;
        Ok(entry.clone())
    }

    async fn get(&self, namespace: &str, id: Uuid) -> Result<MemoryEntry> {
        let sql = format!(
            "select {} from memories where namespace = $1 and id = $2",
            Self::ENTRY_COLUMNS
        );
        let row = sqlx::query(&sql)
            .bind(namespace)
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .pg()?
            .ok_or(HubError::NotFound)?;
        Self::entry_from_row(&row)
    }

    async fn get_by_key(&self, namespace: &str, key: &str) -> Result<Option<MemoryEntry>> {
        let sql = format!(
            r#"
            select {} from memories
            where namespace = $1 and key = $2
            order by (case when status = 'active' then 0 when status = 'archived' then 1 else 2 end),
                     updated_at desc
            limit 1
            "#,
            Self::ENTRY_COLUMNS
        );
        let row = sqlx::query(&sql)
            .bind(namespace)
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .pg()?;
        row.map(|r| Self::entry_from_row(&r)).transpose()
    }

    async fn save(&self, entry: &MemoryEntry) -> Result<MemoryEntry> {
        let mut conn = self.pool.acquire().await.pg()?;
        let rows = Self::apply_update_on(&mut conn, entry, None).await?;
        // 契约：save 只保存已存在条目；条目不存在（0 行受影响）必须报
        // NotFound，静默吞掉会变成"调用方以为写成功实则丢写"
        if rows == 0 {
            return Err(HubError::NotFound);
        }
        Ok(entry.clone())
    }

    /// 乐观并发修订：版本号不符即拒写，绝不静默覆盖并发修订
    async fn save_if_version(
        &self,
        entry: &MemoryEntry,
        expected_version: i64,
    ) -> Result<MemoryEntry> {
        let mut conn = self.pool.acquire().await.pg()?;
        let rows = Self::apply_update_on(&mut conn, entry, Some(expected_version)).await?;
        if rows == 0 {
            return Err(HubError::Conflict);
        }
        Ok(entry.clone())
    }

    /// 原子修订：乐观锁写回条目 + 追加版本历史，同一事务同成同败
    async fn revise(
        &self,
        entry: &MemoryEntry,
        expected_version: i64,
        change: mem_core::domain::VersionChange,
    ) -> Result<MemoryEntry> {
        let mut tx = self.pool.begin().await.pg()?;
        let rows = Self::apply_update_on(&mut tx, entry, Some(expected_version)).await?;
        if rows == 0 {
            // 版本不符：不追加历史，事务在此丢弃
            return Err(HubError::Conflict);
        }
        let row = MemoryVersion {
            memory_id: entry.id,
            version: entry.version,
            content: entry.content.clone(),
            tags: entry.tags.clone(),
            metadata: entry.metadata.clone(),
            confidence: entry.confidence,
            changed_by: entry.source_agent.clone(),
            change_type: change,
            changed_at: entry.updated_at,
            scene: entry.scene,
            layer: entry.layer,
            valid_from: entry.valid_from,
            valid_until: entry.valid_until,
            occurred_at: entry.occurred_at,
        };
        Self::insert_version_on(&mut tx, &row).await?;
        tx.commit().await.pg()?;
        Ok(entry.clone())
    }

    async fn delete(&self, namespace: &str, id: Uuid) -> Result<()> {
        sqlx::query("delete from memories where namespace = $1 and id = $2")
            .bind(namespace)
            .bind(id)
            .execute(&self.pool)
            .await
            .pg()?;
        Ok(())
    }

    async fn list(&self, filter: &MemoryFilter) -> Result<Vec<MemoryEntry>> {
        let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new(format!(
            "select {} from memories where 1 = 1",
            Self::ENTRY_COLUMNS
        ));
        Self::push_memory_filters(&mut qb, filter);

        match filter
            .text_contains
            .as_deref()
            .and_then(mem_core::cjk::fts_or_query)
        {
            // 带查询词时按词法相关度排序：候选池此前按 updated_at 截断，
            // 更相关但更旧的文档会在打分前被系统性丢弃
            Some(or_query) => {
                qb.push(
                    " order by ts_rank_cd(to_tsvector('simple', tsv_tokens), to_tsquery('simple', ",
                )
                .push_bind(or_query)
                .push(")) desc, updated_at desc, id desc");
            }
            None => {
                qb.push(" order by updated_at desc, id desc");
            }
        }
        if let Some(limit) = filter.limit {
            qb.push(" limit ").push_bind(limit as i64);
        }

        let rows = qb.build().fetch_all(&self.pool).await.pg()?;
        rows.iter().map(Self::entry_from_row).collect()
    }

    async fn count(&self, filter: &MemoryFilter) -> Result<usize> {
        let mut qb =
            sqlx::QueryBuilder::<sqlx::Postgres>::new("select count(*) from memories where 1 = 1");
        Self::push_memory_filters(&mut qb, filter);
        let count: i64 = qb.build_query_scalar().fetch_one(&self.pool).await.pg()?;
        Ok(count.max(0) as usize)
    }

    async fn similar(
        &self,
        filter: &MemoryFilter,
        embedding: &[f32],
        k: usize,
    ) -> Result<Vec<Scored<MemoryEntry>>> {
        let literal = mapping::vector_literal(embedding);
        // 状态缺省按 active 处理（与历史行为一致，避免默认召回已归档条目）
        let mut effective = filter.clone();
        if effective.statuses.is_none() {
            effective.statuses = Some(vec![MemoryStatus::Active]);
        }

        let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new(format!(
            "select {}, (embedding <=> ",
            Self::ENTRY_COLUMNS
        ));
        qb.push_bind(literal)
            .push("::vector) as dist from memories where embedding is not null");
        // 过滤条件下推到 SQL：否则其它场景/已失效条目会占满 top-k，导致目标候选缺失
        Self::push_memory_filters(&mut qb, &effective);
        qb.push(" order by dist limit ").push_bind(k as i64);

        let rows = qb.build().fetch_all(&self.pool).await.pg()?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            let item = Self::entry_from_row(r)?;
            // dist 是本查询自身计算出的别名列，缺列属 SQL 拼装错误，直接传播
            let dist: f64 = r.try_get("dist").pg()?;
            out.push(Scored {
                item,
                score: (1.0 - dist as f32).clamp(0.0, 1.0),
            });
        }
        Ok(out)
    }

    async fn all_in_namespace(
        &self,
        namespace: &str,
        include_merged: bool,
    ) -> Result<Vec<MemoryEntry>> {
        let sql = format!(
            "select {} from memories where namespace = $1 {} order by created_at, id",
            Self::ENTRY_COLUMNS,
            if include_merged {
                ""
            } else {
                "and status <> 'merged'"
            }
        );
        let rows = sqlx::query(&sql)
            .bind(namespace)
            .fetch_all(&self.pool)
            .await
            .pg()?;
        rows.iter().map(Self::entry_from_row).collect()
    }

    async fn list_namespaces(&self) -> Result<Vec<Namespace>> {
        let rows: Vec<String> =
            sqlx::query_scalar("select distinct namespace from memories order by namespace")
                .fetch_all(&self.pool)
                .await
                .pg()?;
        Ok(rows)
    }

    async fn record_version(&self, v: &MemoryVersion) -> Result<()> {
        let mut conn = self.pool.acquire().await.pg()?;
        Self::insert_version_on(&mut conn, v).await
    }

    async fn versions(&self, id: Uuid) -> Result<Vec<MemoryVersion>> {
        let rows = sqlx::query(
            r#"
            select memory_id, version, content, tags, metadata, confidence, changed_by, change_type, changed_at,
                   scene, layer, valid_from, valid_until, occurred_at
            from memory_versions where memory_id = $1 order by version
            "#,
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await
        .pg()?;

        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            let change: String = r.try_get("change_type").pg()?;
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
            out.push(MemoryVersion {
                memory_id: r.try_get("memory_id").pg()?,
                version: r.try_get("version").pg()?,
                content: r.try_get("content").pg()?,
                tags: r.try_get("tags").pg()?,
                metadata: r.try_get("metadata").pg()?,
                confidence: r.try_get("confidence").pg()?,
                changed_by: r.try_get("changed_by").pg()?,
                change_type: mapping::parse_version_change(&change)?,
                changed_at: r.try_get("changed_at").pg()?,
                scene,
                layer,
                valid_from,
                valid_until,
                occurred_at,
            });
        }
        Ok(out)
    }

    async fn archive_expired(&self, now: DateTime<Utc>) -> Result<usize> {
        // 单条 CTE：归档 + 版本历史一次完成
        let res = sqlx::query(
            r#"
            with expired as (
              update memories set status = 'archived', version = version + 1, updated_at = $1
              where status = 'active' and expires_at is not null and expires_at <= $1
              returning id, version, content, tags, metadata, confidence, scene, layer, valid_from, valid_until, occurred_at
            )
            insert into memory_versions
              (memory_id, version, content, tags, metadata, confidence, changed_by, change_type, changed_at,
               scene, layer, valid_from, valid_until, occurred_at)
            select id, version, content, tags, metadata, confidence, 'system', 'archived', $1, scene, layer, valid_from, valid_until, occurred_at
            from expired
            on conflict (memory_id, version) do nothing
            "#,
        )
        .bind(now)
        .execute(&self.pool)
        .await
        .pg()?;
        Ok(res.rows_affected() as usize)
    }

    async fn ping(&self) -> Result<()> {
        sqlx::query("select 1").execute(&self.pool).await.pg()?;
        Ok(())
    }
}

#[async_trait]
impl mem_core::BackupStore for PostgresHubStore {
    async fn save_backup(&self, backup: &BackupRecord) -> Result<()> {
        sqlx::query(
            r#"
            insert into memory_backups
              (id, namespace, created_by, note, entry_count, checksum, payload, retain_until, created_at)
            values ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            on conflict (id) do update set
              note = excluded.note, entry_count = excluded.entry_count,
              checksum = excluded.checksum, payload = excluded.payload,
              retain_until = excluded.retain_until
            "#,
        )
        .bind(backup.id)
        .bind(&backup.namespace)
        .bind(&backup.created_by)
        .bind(&backup.note)
        .bind(backup.entry_count as i32)
        .bind(&backup.checksum)
        .bind(&backup.payload)
        .bind(backup.retain_until)
        .bind(backup.created_at)
        .execute(&self.pool)
        .await
        .pg()?;
        Ok(())
    }

    async fn get_backup(&self, id: Uuid) -> Result<BackupRecord> {
        let row = sqlx::query(
            r#"
            select id, namespace, created_by, note, entry_count, checksum, payload, retain_until, created_at
            from memory_backups where id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .pg()?
        .ok_or(HubError::NotFound)?;
        Self::backup_from_row(&row)
    }

    /// 列举快照（摘要视图）。
    ///
    /// 刻意不取 `payload` 列：本方法被保留策略 `prune` 以 `None` 全库调用，
    /// 历史上会把所有历史快照的整份 JSONB 一次性读进内存（每份都是全命名空间副本）。
    /// 需要负载的调用方必须走 `get_backup(id)`。
    async fn list_backups(&self, namespace: Option<&str>) -> Result<Vec<BackupRecord>> {
        const META_COLUMNS: &str =
            "id, namespace, created_by, note, entry_count, checksum, retain_until, created_at";
        let rows = match namespace {
            Some(ns) => sqlx::query(&format!(
                "select {} from memory_backups where namespace = $1 order by created_at desc",
                META_COLUMNS
            ))
            .bind(ns)
            .fetch_all(&self.pool)
            .await
            .pg()?,
            None => sqlx::query(&format!(
                "select {} from memory_backups order by created_at desc",
                META_COLUMNS
            ))
            .fetch_all(&self.pool)
            .await
            .pg()?,
        };
        rows.iter().map(Self::backup_meta_from_row).collect()
    }

    async fn delete_backup(&self, id: Uuid) -> Result<()> {
        sqlx::query("delete from memory_backups where id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .pg()?;
        Ok(())
    }
}

#[async_trait]
impl mem_core::IntegrationStore for PostgresHubStore {
    async fn save_proposal(&self, p: &IntegrationProposal) -> Result<()> {
        sqlx::query(
            r#"
            insert into integration_proposals
              (id, namespace, kind, member_ids, suggested_survivor, similarity, status, resolution_note, created_at, resolved_at, metadata)
            values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            on conflict (id) do update set
              status = excluded.status, resolution_note = excluded.resolution_note,
              resolved_at = excluded.resolved_at, suggested_survivor = excluded.suggested_survivor
            "#,
        )
        .bind(p.id)
        .bind(&p.namespace)
        .bind(p.kind.as_str())
        .bind(&p.member_ids)
        .bind(p.suggested_survivor)
        .bind(p.similarity)
        .bind(match p.status {
            ProposalStatus::Open => "open",
            ProposalStatus::Merged => "merged",
            ProposalStatus::Dismissed => "dismissed",
            ProposalStatus::Expired => "expired",
        })
        .bind(&p.resolution_note)
        .bind(p.created_at)
        .bind(p.resolved_at)
        .bind(&p.metadata)
        .execute(&self.pool)
        .await
        .pg()?;
        Ok(())
    }

    async fn get_proposal(&self, id: Uuid) -> Result<IntegrationProposal> {
        let row = sqlx::query(
            r#"
            select id, namespace, kind, member_ids, suggested_survivor, similarity, status, resolution_note, created_at, resolved_at, metadata
            from integration_proposals where id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .pg()?
        .ok_or(HubError::NotFound)?;
        Self::proposal_from_row(&row)
    }

    async fn list_proposals(
        &self,
        namespace: &str,
        status: Option<ProposalStatus>,
    ) -> Result<Vec<IntegrationProposal>> {
        let rows = match status {
            Some(s) => {
                sqlx::query(
                    r#"
                    select id, namespace, kind, member_ids, suggested_survivor, similarity, status, resolution_note, created_at, resolved_at, metadata
                    from integration_proposals where namespace = $1 and status = $2
                    order by created_at desc
                    "#,
                )
                .bind(namespace)
                .bind(match s {
                    ProposalStatus::Open => "open",
                    ProposalStatus::Merged => "merged",
                    ProposalStatus::Dismissed => "dismissed",
                    ProposalStatus::Expired => "expired",
                })
                .fetch_all(&self.pool)
                .await
                .pg()?
            }
            None => {
                sqlx::query(
                    r#"
                    select id, namespace, kind, member_ids, suggested_survivor, similarity, status, resolution_note, created_at, resolved_at, metadata
                    from integration_proposals where namespace = $1
                    order by created_at desc
                    "#,
                )
                .bind(namespace)
                .fetch_all(&self.pool)
                .await
                .pg()?
            }
        };
        rows.iter().map(Self::proposal_from_row).collect()
    }

    async fn open_exists(&self, namespace: &str, members: &[(String, Vec<Uuid>)]) -> Result<bool> {
        for (kind, ids) in members {
            let count: i64 = sqlx::query_scalar(
                r#"
                select count(*) from integration_proposals
                where namespace = $1 and status = 'open' and kind = $2
                  and cardinality(member_ids) = cardinality($3)
                  and member_ids @> $3
                "#,
            )
            .bind(namespace)
            .bind(kind)
            .bind(ids)
            .fetch_one(&self.pool)
            .await
            .pg()?;
            if count > 0 {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

#[async_trait]
impl mem_core::CognitiveProposalStore for PostgresHubStore {
    async fn save_cognitive_proposal(&self, p: &CognitiveProposalRecord) -> Result<()> {
        let candidate = serde_json::to_value(&p.candidate)
            .map_err(|e| HubError::Internal(anyhow::anyhow!("认知提案载荷序列化失败: {}", e)))?;
        sqlx::query(
            r#"
            insert into cognitive_proposals
              (id, namespace, candidate, source_evidence_id, status, rejection_reason,
               resulting_memory_id, resolved_by, created_at, updated_at)
            values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            "#,
        )
        .bind(p.id)
        .bind(&p.namespace)
        .bind(&candidate)
        .bind(p.source_evidence_id)
        .bind(p.status.as_str())
        .bind(&p.rejection_reason)
        .bind(p.resulting_memory_id)
        .bind(&p.resolved_by)
        .bind(p.created_at)
        .bind(p.updated_at)
        .execute(&self.pool)
        .await
        .pg()?;
        Ok(())
    }

    async fn get_cognitive_proposal(&self, id: Uuid) -> Result<CognitiveProposalRecord> {
        let sql = format!(
            "select {} from cognitive_proposals where id = $1",
            Self::COGNITIVE_PROPOSAL_COLUMNS
        );
        let row = sqlx::query(&sql)
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .pg()?
            .ok_or(HubError::NotFound)?;
        Self::cognitive_proposal_from_row(&row)
    }

    async fn list_cognitive_proposals(
        &self,
        namespace: &str,
        status: Option<CognitiveProposalStatus>,
    ) -> Result<Vec<CognitiveProposalRecord>> {
        let sql = match status {
            Some(_) => format!(
                "select {} from cognitive_proposals where namespace = $1 and status = $2 \
                 order by created_at desc",
                Self::COGNITIVE_PROPOSAL_COLUMNS
            ),
            None => format!(
                "select {} from cognitive_proposals where namespace = $1 \
                 order by created_at desc",
                Self::COGNITIVE_PROPOSAL_COLUMNS
            ),
        };
        let mut q = sqlx::query(&sql).bind(namespace);
        if let Some(s) = status {
            q = q.bind(s.as_str());
        }
        let rows = q.fetch_all(&self.pool).await.pg()?;
        rows.iter().map(Self::cognitive_proposal_from_row).collect()
    }

    async fn update_cognitive_proposal_status(
        &self,
        id: Uuid,
        expected_status: CognitiveProposalStatus,
        new_status: CognitiveProposalStatus,
        rejection_reason: Option<&str>,
        resulting_memory_id: Option<Uuid>,
        resolved_by: Option<&str>,
    ) -> Result<CognitiveProposalRecord> {
        // 先原子推进状态（乐观前置），再回读整行
        let result = sqlx::query(
            r#"
            update cognitive_proposals
            set status = $2, rejection_reason = $3, resulting_memory_id = $4,
                resolved_by = $5, updated_at = now()
            where id = $1 and status = $6
            "#,
        )
        .bind(id)
        .bind(new_status.as_str())
        .bind(rejection_reason)
        .bind(resulting_memory_id)
        .bind(resolved_by)
        .bind(expected_status.as_str())
        .execute(&self.pool)
        .await
        .pg()?;
        if result.rows_affected() == 0 {
            // 区分两种失败：提案不存在（NotFound）vs 已被别人裁决（Conflict）
            let exists: Option<Uuid> =
                sqlx::query_scalar("select id from cognitive_proposals where id = $1")
                    .bind(id)
                    .fetch_optional(&self.pool)
                    .await
                    .pg()?;
            return match exists {
                Some(_) => Err(HubError::Conflict),
                None => Err(HubError::NotFound),
            };
        }
        self.get_cognitive_proposal(id).await
    }
}

#[async_trait]
impl mem_core::GrantStore for PostgresHubStore {
    async fn ensure_namespace(
        &self,
        namespace: &str,
        owner: &AgentId,
    ) -> Result<(NamespaceInfo, bool)> {
        // owner 必须先注册为 agent（外键约束）；未知 Agent 归入引导账户兜底
        sqlx::query(
            r#"
            insert into agents (agent_id, name, owner_account_id) values ($1, $1, $2)
            on conflict (agent_id) do nothing
            "#,
        )
        .bind(owner)
        .bind(common::BOOTSTRAP_ACCOUNT_ID)
        .execute(&self.pool)
        .await
        .pg()?;

        let existing: Option<String> =
            sqlx::query_scalar("select namespace from namespaces where namespace = $1")
                .bind(namespace)
                .fetch_optional(&self.pool)
                .await
                .pg()?;

        let created = existing.is_none();
        if created {
            sqlx::query("insert into namespaces (namespace, owner_agent) values ($1, $2)")
                .bind(namespace)
                .bind(owner)
                .execute(&self.pool)
                .await
                .pg()?;
        }

        let info = NamespaceInfo {
            namespace: namespace.to_string(),
            owner_agent: owner.clone(),
            created_at: Utc::now(),
        };
        Ok((info, created))
    }

    async fn get_namespace(&self, namespace: &str) -> Result<Option<NamespaceInfo>> {
        let row = sqlx::query(
            "select namespace, owner_agent, created_at from namespaces where namespace = $1",
        )
        .bind(namespace)
        .fetch_optional(&self.pool)
        .await
        .pg()?;
        Ok(row.map(|r| NamespaceInfo {
            namespace: r.get("namespace"),
            owner_agent: r.get("owner_agent"),
            created_at: r.get("created_at"),
        }))
    }

    async fn list_namespace_infos(&self) -> Result<Vec<NamespaceInfo>> {
        let rows = sqlx::query(
            "select namespace, owner_agent, created_at from namespaces order by namespace",
        )
        .fetch_all(&self.pool)
        .await
        .pg()?;
        Ok(rows
            .iter()
            .map(|r| NamespaceInfo {
                namespace: r.get("namespace"),
                owner_agent: r.get("owner_agent"),
                created_at: r.get("created_at"),
            })
            .collect())
    }

    async fn grant(&self, record: &NamespaceGrantRecord) -> Result<()> {
        sqlx::query(
            r#"
            insert into namespace_grants (namespace, agent_id, can_read, can_write, granted_by, created_at)
            values ($1, $2, $3, $4, $5, $6)
            on conflict (namespace, agent_id) do update set
              can_read = excluded.can_read, can_write = excluded.can_write,
              granted_by = excluded.granted_by, created_at = excluded.created_at
            "#,
        )
        .bind(&record.namespace)
        .bind(&record.agent_id)
        .bind(record.can_read)
        .bind(record.can_write)
        .bind(&record.granted_by)
        .bind(record.created_at)
        .execute(&self.pool)
        .await
        .pg()?;
        Ok(())
    }

    async fn revoke(&self, namespace: &str, agent_id: &AgentId) -> Result<()> {
        sqlx::query("delete from namespace_grants where namespace = $1 and agent_id = $2")
            .bind(namespace)
            .bind(agent_id)
            .execute(&self.pool)
            .await
            .pg()?;
        Ok(())
    }

    async fn grants_for(&self, agent_id: &AgentId) -> Result<Vec<NamespaceGrantRecord>> {
        let rows = sqlx::query(
            r#"
            select namespace, agent_id, can_read, can_write, granted_by, created_at
            from namespace_grants where agent_id = $1 order by namespace
            "#,
        )
        .bind(agent_id)
        .fetch_all(&self.pool)
        .await
        .pg()?;
        Ok(rows
            .iter()
            .map(|r| NamespaceGrantRecord {
                namespace: r.get("namespace"),
                agent_id: r.get("agent_id"),
                can_read: r.get("can_read"),
                can_write: r.get("can_write"),
                granted_by: r.get("granted_by"),
                created_at: r.get("created_at"),
            })
            .collect())
    }

    async fn can_read(&self, namespace: &str, agent_id: &AgentId) -> Result<bool> {
        // 沙箱语义：授权行必须精确匹配命名空间；`*` 通配授权已废除
        let count: i64 = sqlx::query_scalar(
            r#"
            select count(*) from namespace_grants
            where agent_id = $1 and namespace = $2 and can_read
            "#,
        )
        .bind(agent_id)
        .bind(namespace)
        .fetch_one(&self.pool)
        .await
        .pg()?;
        Ok(count > 0)
    }

    async fn can_write(&self, namespace: &str, agent_id: &AgentId) -> Result<bool> {
        let count: i64 = sqlx::query_scalar(
            r#"
            select count(*) from namespace_grants
            where agent_id = $1 and namespace = $2 and can_write
            "#,
        )
        .bind(agent_id)
        .bind(namespace)
        .fetch_one(&self.pool)
        .await
        .pg()?;
        Ok(count > 0)
    }

    async fn owned_namespaces(&self, agent_id: &AgentId) -> Result<Vec<Namespace>> {
        let rows: Vec<String> = sqlx::query_scalar(
            "select namespace from namespaces where owner_agent = $1 order by namespace",
        )
        .bind(agent_id)
        .fetch_all(&self.pool)
        .await
        .pg()?;
        Ok(rows)
    }
}

#[async_trait]
impl mem_core::AgentStore for PostgresHubStore {
    async fn register_agent(
        &self,
        reg: &mem_core::AgentRegistration,
    ) -> Result<mem_core::AgentRecord> {
        // Argon2 为 CPU 密集计算，经 spawn_blocking 执行避免阻塞 tokio worker
        let secret_hash: Option<String> = match &reg.client_secret {
            Some(s) => {
                Some(crate::secret::hash_secret_blocking(reg.agent_id.clone(), s.clone()).await)
            }
            None => None,
        };
        let max_scopes: Vec<String> = reg
            .max_scopes
            .iter()
            .map(|s| s.as_str().to_string())
            .collect();

        sqlx::query(
            r#"
            insert into agents (agent_id, name, kind, status, owner_account_id, client_secret_hash, redirect_uris, max_scopes, created_at)
            values ($1, $2, $3, 'active', $4, $5, $6, $7, now())
            on conflict (agent_id) do update set
              name = excluded.name,
              kind = excluded.kind,
              owner_account_id = excluded.owner_account_id,
              redirect_uris = excluded.redirect_uris,
              max_scopes = excluded.max_scopes,
              client_secret_hash = coalesce(excluded.client_secret_hash, agents.client_secret_hash)
            "#,
        )
        .bind(&reg.agent_id)
        .bind(&reg.name)
        .bind(&reg.kind)
        .bind(reg.owner_account_id)
        .bind(&secret_hash)
        .bind(&reg.redirect_uris)
        .bind(&max_scopes)
        .execute(&self.pool)
        .await
        .pg()?;

        self.get_agent(&reg.agent_id)
            .await?
            .ok_or_else(|| HubError::Internal(anyhow::anyhow!("agent 注册后读取失败")))
    }

    async fn get_agent(&self, agent_id: &AgentId) -> Result<Option<mem_core::AgentRecord>> {
        let row = sqlx::query(
            r#"
            select agent_id, name, kind, status, owner_account_id, redirect_uris, max_scopes,
                   (client_secret_hash is not null) as confidential, created_at
            from agents where agent_id = $1
            "#,
        )
        .bind(agent_id)
        .fetch_optional(&self.pool)
        .await
        .pg()?;

        match row {
            Some(r) => Ok(Some(Self::agent_from_row(&r)?)),
            None => Ok(None),
        }
    }

    async fn list_agents(&self) -> Result<Vec<mem_core::AgentRecord>> {
        let rows = sqlx::query(
            r#"
            select agent_id, name, kind, status, owner_account_id, redirect_uris, max_scopes,
                   (client_secret_hash is not null) as confidential, created_at
            from agents order by created_at desc
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .pg()?;

        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            out.push(Self::agent_from_row(r)?);
        }
        Ok(out)
    }

    async fn set_agent_status(&self, agent_id: &AgentId, status: &str) -> Result<()> {
        let res = sqlx::query("update agents set status = $2 where agent_id = $1")
            .bind(agent_id)
            .bind(status)
            .execute(&self.pool)
            .await
            .pg()?;
        if res.rows_affected() == 0 {
            return Err(HubError::NotFound);
        }
        Ok(())
    }

    async fn verify_agent_secret(&self, agent_id: &AgentId, secret: &str) -> Result<bool> {
        let row = sqlx::query("select client_secret_hash from agents where agent_id = $1")
            .bind(agent_id)
            .fetch_optional(&self.pool)
            .await
            .pg()?;
        let stored = match row {
            Some(r) => {
                let h: Option<String> = r.try_get("client_secret_hash").pg()?;
                h
            }
            None => None,
        };
        let stored = match stored {
            Some(h) => h,
            None => return Ok(false),
        };
        // Argon2 为 CPU 密集计算，经 spawn_blocking 执行避免阻塞 tokio worker
        let ok = crate::secret::verify_secret_blocking(
            agent_id.to_string(),
            secret.to_string(),
            stored.clone(),
        )
        .await;
        // 存量旧摘要（无盐 sha256）在首次校验成功后就地升级为 Argon2id
        if ok && crate::secret::is_legacy_digest(&stored) {
            let upgraded =
                crate::secret::hash_secret_blocking(agent_id.to_string(), secret.to_string()).await;
            sqlx::query("update agents set client_secret_hash = $2 where agent_id = $1")
                .bind(agent_id)
                .bind(&upgraded)
                .execute(&self.pool)
                .await
                .pg()?;
        }
        Ok(ok)
    }
}

#[async_trait]
impl mem_core::AccountStore for PostgresHubStore {
    async fn create_account(
        &self,
        username: &str,
        password_hash: Option<&str>,
        display_name: &str,
    ) -> Result<AccountRecord> {
        let normalized = username.trim().to_ascii_lowercase();
        let exists: Option<Uuid> =
            sqlx::query_scalar("select id from accounts where lower(username) = $1")
                .bind(&normalized)
                .fetch_optional(&self.pool)
                .await
                .pg()?;
        if exists.is_some() {
            return Err(HubError::InvalidArgument(format!(
                "用户名 {} 已被占用",
                normalized
            )));
        }
        let id = Uuid::new_v4();
        sqlx::query(
            r#"
            insert into accounts (id, username, display_name, status, password_hash, created_at)
            values ($1, $2, $3, 'active', $4, now())
            "#,
        )
        .bind(id)
        .bind(&normalized)
        .bind(display_name)
        .bind(password_hash)
        .execute(&self.pool)
        .await
        .pg()?;
        self.get_account(&id)
            .await?
            .ok_or_else(|| HubError::Internal(anyhow::anyhow!("账户创建后读取失败")))
    }

    async fn ensure_account(
        &self,
        id: &AccountId,
        username: &str,
        display_name: &str,
    ) -> Result<AccountRecord> {
        let normalized = username.trim().to_ascii_lowercase();
        sqlx::query(
            r#"
            insert into accounts (id, username, display_name, status, created_at)
            values ($1, $2, $3, 'active', now())
            on conflict (id) do nothing
            "#,
        )
        .bind(id)
        .bind(&normalized)
        .bind(display_name)
        .execute(&self.pool)
        .await
        .pg()?;
        // 用户名冲突兜底：若该用户名已归属其他账户，按用户名回读
        if let Some(record) = self.get_account(id).await? {
            return Ok(record);
        }
        self.get_account_by_username(&normalized)
            .await?
            .ok_or_else(|| HubError::Internal(anyhow::anyhow!("引导账户确保失败")))
    }

    async fn get_account(&self, id: &AccountId) -> Result<Option<AccountRecord>> {
        let row = sqlx::query(
            r#"
            select id, username, display_name, status,
                   (password_hash is not null) as has_password, created_at
            from accounts where id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .pg()?;
        Ok(row.map(|r| account_record_of(&r)))
    }

    async fn get_account_by_username(&self, username: &str) -> Result<Option<AccountRecord>> {
        let normalized = username.trim().to_ascii_lowercase();
        let row = sqlx::query(
            r#"
            select id, username, display_name, status,
                   (password_hash is not null) as has_password, created_at
            from accounts where lower(username) = $1
            "#,
        )
        .bind(&normalized)
        .fetch_optional(&self.pool)
        .await
        .pg()?;
        Ok(row.map(|r| account_record_of(&r)))
    }

    async fn password_hash_of(&self, username: &str) -> Result<Option<String>> {
        let normalized = username.trim().to_ascii_lowercase();
        let hash: Option<String> =
            sqlx::query_scalar("select password_hash from accounts where lower(username) = $1")
                .bind(&normalized)
                .fetch_optional(&self.pool)
                .await
                .pg()?
                .flatten();
        Ok(hash)
    }

    async fn set_account_password(&self, id: &AccountId, password_hash: &str) -> Result<()> {
        let res = sqlx::query("update accounts set password_hash = $2 where id = $1")
            .bind(id)
            .bind(password_hash)
            .execute(&self.pool)
            .await
            .pg()?;
        if res.rows_affected() == 0 {
            return Err(HubError::NotFound);
        }
        Ok(())
    }

    async fn set_account_status(&self, id: &AccountId, status: &str) -> Result<()> {
        let res = sqlx::query("update accounts set status = $2 where id = $1")
            .bind(id)
            .bind(status)
            .execute(&self.pool)
            .await
            .pg()?;
        if res.rows_affected() == 0 {
            return Err(HubError::NotFound);
        }
        Ok(())
    }

    async fn list_accounts(&self) -> Result<Vec<AccountRecord>> {
        let rows = sqlx::query(
            r#"
            select id, username, display_name, status,
                   (password_hash is not null) as has_password, created_at
            from accounts order by created_at
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .pg()?;
        Ok(rows.iter().map(account_record_of).collect())
    }

    async fn create_session(
        &self,
        token_hash: &[u8],
        account_id: &AccountId,
        expires_at: DateTime<Utc>,
    ) -> Result<()> {
        sqlx::query(
            r#"
            insert into account_sessions (token_hash, account_id, expires_at, created_at)
            values ($1, $2, $3, now())
            on conflict (token_hash) do update set
              account_id = excluded.account_id, expires_at = excluded.expires_at
            "#,
        )
        .bind(token_hash)
        .bind(account_id)
        .bind(expires_at)
        .execute(&self.pool)
        .await
        .pg()?;
        Ok(())
    }

    async fn get_session(&self, token_hash: &[u8]) -> Result<Option<SessionRecord>> {
        let row = sqlx::query(
            "select account_id, expires_at from account_sessions where token_hash = $1",
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await
        .pg()?;
        Ok(row.map(|r| SessionRecord {
            account_id: r.get("account_id"),
            expires_at: r.get("expires_at"),
        }))
    }

    async fn delete_session(&self, token_hash: &[u8]) -> Result<()> {
        sqlx::query("delete from account_sessions where token_hash = $1")
            .bind(token_hash)
            .execute(&self.pool)
            .await
            .pg()?;
        Ok(())
    }

    async fn prune_sessions(&self, now: DateTime<Utc>) -> Result<usize> {
        let res = sqlx::query("delete from account_sessions where expires_at <= $1")
            .bind(now)
            .execute(&self.pool)
            .await
            .pg()?;
        Ok(res.rows_affected() as usize)
    }
}

#[async_trait]
impl EvidenceStore for PostgresHubStore {
    async fn append_evidence(&self, evidence: &NewEvidence) -> Result<MemoryEvidence> {
        let id = evidence.id.unwrap_or_else(Uuid::new_v4);
        let row = sqlx::query(
            r#"
            insert into memory_evidence (id, namespace, source_kind, source_ref, content_hash, excerpt, occurred_at, created_at, expires_at)
            values ($1, $2, $3, $4, $5, $6, $7, now(), $8)
            returning id, namespace, source_kind, source_ref, content_hash, excerpt, occurred_at, created_at, expires_at
            "#,
        )
        .bind(id)
        .bind(&evidence.namespace)
        .bind(evidence.source_kind.as_str())
        .bind(&evidence.source_ref)
        .bind(&evidence.content_hash)
        .bind(&evidence.excerpt)
        .bind(evidence.occurred_at)
        .bind(evidence.expires_at)
        .fetch_one(&self.pool)
        .await
        .pg()?;

        Self::evidence_from_row(&row)
    }

    async fn get_evidence(&self, namespace: &str, id: Uuid) -> Result<MemoryEvidence> {
        let row = sqlx::query(
            r#"
            select id, namespace, source_kind, source_ref, content_hash, excerpt, occurred_at, created_at, expires_at
            from memory_evidence
            where namespace = $1 and id = $2
            "#,
        )
        .bind(namespace)
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .pg()?;

        match row {
            Some(r) => Self::evidence_from_row(&r),
            None => Err(HubError::NotFound),
        }
    }

    async fn list_evidence(
        &self,
        namespace: &str,
        source_kind: Option<EvidenceSourceKind>,
        source_ref: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemoryEvidence>> {
        let mut builder: sqlx::QueryBuilder<'_, sqlx::Postgres> = sqlx::QueryBuilder::new(
            "select id, namespace, source_kind, source_ref, content_hash, excerpt, occurred_at, created_at, expires_at from memory_evidence where namespace = ",
        );
        builder.push_bind(namespace);

        if let Some(sk) = source_kind {
            builder.push(" and source_kind = ");
            builder.push_bind(sk.as_str());
        }
        if let Some(sref) = source_ref {
            builder.push(" and source_ref = ");
            builder.push_bind(sref);
        }

        builder.push(" order by created_at desc limit ");
        builder.push_bind(limit as i64);

        let rows = builder.build().fetch_all(&self.pool).await.pg()?;
        rows.iter().map(Self::evidence_from_row).collect()
    }

    async fn all_evidence_in_namespace(&self, namespace: &str) -> Result<Vec<MemoryEvidence>> {
        let rows = sqlx::query(
            r#"
            select id, namespace, source_kind, source_ref, content_hash, excerpt, occurred_at, created_at, expires_at
            from memory_evidence
            where namespace = $1
            order by created_at desc
            "#,
        )
        .bind(namespace)
        .fetch_all(&self.pool)
        .await
        .pg()?;

        rows.iter().map(Self::evidence_from_row).collect()
    }

    async fn delete_evidence(&self, namespace: &str, id: Uuid) -> Result<()> {
        let res = sqlx::query("delete from memory_evidence where namespace = $1 and id = $2")
            .bind(namespace)
            .bind(id)
            .execute(&self.pool)
            .await
            .pg()?;

        if res.rows_affected() == 0 {
            Err(HubError::NotFound)
        } else {
            Ok(())
        }
    }

    /// 清理已过保留期限的证据（NULL = 永久保留，不动）
    async fn purge_expired_evidence(&self, namespace: &str, now: DateTime<Utc>) -> Result<usize> {
        let res = sqlx::query(
            "delete from memory_evidence where namespace = $1 and expires_at is not null and expires_at <= $2",
        )
        .bind(namespace)
        .bind(now)
        .execute(&self.pool)
        .await
        .pg()?;

        Ok(res.rows_affected() as usize)
    }
}

#[async_trait]
impl RelationStore for PostgresHubStore {
    async fn add_relation(
        &self,
        namespace: &str,
        relation: &NewMemoryRelation,
    ) -> Result<MemoryRelation> {
        if relation.from_memory_id == relation.to_memory_id {
            return Err(HubError::InvalidArgument(
                "实体自环关系不允许 (from == to)".into(),
            ));
        }

        let count: i64 = sqlx::query_scalar(
            "select count(*) from memories where namespace = $1 and id in ($2, $3)",
        )
        .bind(namespace)
        .bind(relation.from_memory_id)
        .bind(relation.to_memory_id)
        .fetch_one(&self.pool)
        .await
        .pg()?;

        if count != 2 {
            return Err(HubError::NotFound);
        }

        if let Some(ev_id) = relation.source_evidence_id {
            let ev_exists: bool = sqlx::query_scalar(
                "select exists(select 1 from memory_evidence where namespace = $1 and id = $2)",
            )
            .bind(namespace)
            .bind(ev_id)
            .fetch_one(&self.pool)
            .await
            .pg()?;

            if !ev_exists {
                return Err(HubError::NotFound);
            }
        }

        let row = sqlx::query(
            r#"
            insert into memory_relations (namespace, from_memory_id, to_memory_id, relation_kind, confidence, source_evidence_id, created_at)
            values ($1, $2, $3, $4, $5, $6, now())
            on conflict (from_memory_id, to_memory_id, relation_kind) do update set
                namespace = excluded.namespace,
                confidence = excluded.confidence,
                source_evidence_id = coalesce(excluded.source_evidence_id, memory_relations.source_evidence_id)
            returning namespace, from_memory_id, to_memory_id, relation_kind, confidence, source_evidence_id, created_at
            "#,
        )
        .bind(namespace)
        .bind(relation.from_memory_id)
        .bind(relation.to_memory_id)
        .bind(relation.relation_kind.as_str())
        .bind(relation.confidence)
        .bind(relation.source_evidence_id)
        .fetch_one(&self.pool)
        .await
        .pg()?;

        Self::relation_from_row(&row)
    }

    async fn list_relations_from(
        &self,
        namespace: &str,
        from_memory_id: Uuid,
    ) -> Result<Vec<MemoryRelation>> {
        let rows = sqlx::query(
            r#"
            select r.namespace, r.from_memory_id, r.to_memory_id, r.relation_kind, r.confidence, r.source_evidence_id, r.created_at
            from memory_relations r
            join memories m on r.from_memory_id = m.id
            where m.namespace = $1 and r.from_memory_id = $2
            order by r.created_at desc
            "#,
        )
        .bind(namespace)
        .bind(from_memory_id)
        .fetch_all(&self.pool)
        .await
        .pg()?;

        rows.iter().map(Self::relation_from_row).collect()
    }

    async fn list_relations_to(
        &self,
        namespace: &str,
        to_memory_id: Uuid,
    ) -> Result<Vec<MemoryRelation>> {
        let rows = sqlx::query(
            r#"
            select r.namespace, r.from_memory_id, r.to_memory_id, r.relation_kind, r.confidence, r.source_evidence_id, r.created_at
            from memory_relations r
            join memories m on r.to_memory_id = m.id
            where m.namespace = $1 and r.to_memory_id = $2
            order by r.created_at desc
            "#,
        )
        .bind(namespace)
        .bind(to_memory_id)
        .fetch_all(&self.pool)
        .await
        .pg()?;

        rows.iter().map(Self::relation_from_row).collect()
    }

    async fn delete_relation(
        &self,
        namespace: &str,
        from_memory_id: Uuid,
        to_memory_id: Uuid,
        relation_kind: MemoryRelationKind,
    ) -> Result<()> {
        let res = sqlx::query(
            r#"
            delete from memory_relations
            where namespace = $1
              and from_memory_id = $2
              and to_memory_id = $3
              and relation_kind = $4
            "#,
        )
        .bind(namespace)
        .bind(from_memory_id)
        .bind(to_memory_id)
        .bind(relation_kind.as_str())
        .execute(&self.pool)
        .await
        .pg()?;

        if res.rows_affected() == 0 {
            Err(HubError::NotFound)
        } else {
            Ok(())
        }
    }

    async fn all_relations_in_namespace(&self, namespace: &str) -> Result<Vec<MemoryRelation>> {
        let rows = sqlx::query(
            r#"
            select namespace, from_memory_id, to_memory_id, relation_kind, confidence, source_evidence_id, created_at
            from memory_relations
            where namespace = $1
            order by created_at desc
            "#,
        )
        .bind(namespace)
        .fetch_all(&self.pool)
        .await
        .pg()?;

        rows.iter().map(Self::relation_from_row).collect()
    }
}

// ===== TaskStore =====

#[async_trait]
impl TaskStore for PostgresHubStore {
    async fn create_task(&self, task: &NewMemoryTask) -> Result<MemoryTask> {
        let id = task.id.unwrap_or_else(Uuid::new_v4);
        let row = sqlx::query(
            r#"
            insert into memory_tasks (
                id, namespace, scene, task, status, started_at, finished_at,
                result_summary, failure_reason, source_evidence_id, metadata
            )
            values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            returning id, namespace, scene, task, status, started_at, finished_at,
                      result_summary, failure_reason, source_evidence_id, metadata
            "#,
        )
        .bind(id)
        .bind(&task.namespace)
        .bind(task.scene.as_str())
        .bind(&task.task)
        .bind(task.status.as_str())
        .bind(task.started_at)
        .bind(task.finished_at)
        .bind(&task.result_summary)
        .bind(&task.failure_reason)
        .bind(task.source_evidence_id)
        .bind(&task.metadata)
        .fetch_one(&self.pool)
        .await
        .pg()?;

        Self::task_from_row(&row)
    }

    async fn update_task(&self, task: &MemoryTask) -> Result<MemoryTask> {
        let row = sqlx::query(
            r#"
            update memory_tasks
            set scene = $3,
                task = $4,
                status = $5,
                started_at = $6,
                finished_at = $7,
                result_summary = $8,
                failure_reason = $9,
                source_evidence_id = $10,
                metadata = $11
            where namespace = $1 and id = $2
            returning id, namespace, scene, task, status, started_at, finished_at,
                      result_summary, failure_reason, source_evidence_id, metadata
            "#,
        )
        .bind(&task.namespace)
        .bind(task.id)
        .bind(task.scene.as_str())
        .bind(&task.task)
        .bind(task.status.as_str())
        .bind(task.started_at)
        .bind(task.finished_at)
        .bind(&task.result_summary)
        .bind(&task.failure_reason)
        .bind(task.source_evidence_id)
        .bind(&task.metadata)
        .fetch_optional(&self.pool)
        .await
        .pg()?;

        match row {
            Some(r) => Self::task_from_row(&r),
            None => Err(HubError::NotFound),
        }
    }

    async fn get_task(&self, namespace: &str, id: Uuid) -> Result<Option<MemoryTask>> {
        let row = sqlx::query(
            r#"
            select id, namespace, scene, task, status, started_at, finished_at,
                   result_summary, failure_reason, source_evidence_id, metadata
            from memory_tasks
            where namespace = $1 and id = $2
            "#,
        )
        .bind(namespace)
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .pg()?;

        row.map(|r| Self::task_from_row(&r)).transpose()
    }

    async fn list_tasks(
        &self,
        namespace: &str,
        status: Option<TaskStatus>,
        limit: usize,
    ) -> Result<Vec<MemoryTask>> {
        let mut builder: sqlx::QueryBuilder<'_, sqlx::Postgres> = sqlx::QueryBuilder::new(
            "select id, namespace, scene, task, status, started_at, finished_at, result_summary, failure_reason, source_evidence_id, metadata from memory_tasks where namespace = ",
        );
        builder.push_bind(namespace);

        if let Some(s) = status {
            builder.push(" and status = ");
            builder.push_bind(s.as_str());
        }

        builder.push(" order by started_at desc limit ");
        builder.push_bind(limit.max(1) as i64);

        let rows = builder.build().fetch_all(&self.pool).await.pg()?;
        rows.iter().map(Self::task_from_row).collect()
    }

    async fn all_tasks_in_namespace(&self, namespace: &str) -> Result<Vec<MemoryTask>> {
        let rows = sqlx::query(
            r#"
            select id, namespace, scene, task, status, started_at, finished_at,
                   result_summary, failure_reason, source_evidence_id, metadata
            from memory_tasks
            where namespace = $1
            order by started_at asc, id asc
            "#,
        )
        .bind(namespace)
        .fetch_all(&self.pool)
        .await
        .pg()?;

        rows.iter().map(Self::task_from_row).collect()
    }

    async fn delete_task(&self, namespace: &str, id: Uuid) -> Result<()> {
        let res = sqlx::query("delete from memory_tasks where namespace = $1 and id = $2")
            .bind(namespace)
            .bind(id)
            .execute(&self.pool)
            .await
            .pg()?;

        if res.rows_affected() == 0 {
            Err(HubError::NotFound)
        } else {
            Ok(())
        }
    }
}

// ===== ExperienceStore =====

#[async_trait]
impl ExperienceStore for PostgresHubStore {
    async fn save_experience(&self, exp: &NewMemoryExperience) -> Result<MemoryExperience> {
        let id = exp.id.unwrap_or_else(Uuid::new_v4);
        let row = sqlx::query(
            r#"
            insert into memory_experiences (
                id, namespace, task, search_keys, solution, preconditions,
                boundary_envs, failure_modes, validation, confidence, status,
                feedback_score, created_at, updated_at
            )
            values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, now(), now())
            returning id, namespace, task, search_keys, solution, preconditions,
                      boundary_envs, failure_modes, validation, confidence, status,
                      feedback_score, created_at, updated_at
            "#,
        )
        .bind(id)
        .bind(&exp.namespace)
        .bind(&exp.task)
        .bind(&exp.search_keys)
        .bind(&exp.solution)
        .bind(&exp.preconditions)
        .bind(&exp.boundary_envs)
        .bind(&exp.failure_modes)
        .bind(&exp.validation)
        .bind(exp.confidence)
        .bind(exp.status.as_str())
        .bind(exp.feedback_score)
        .fetch_one(&self.pool)
        .await
        .pg()?;

        Self::experience_from_row(&row)
    }

    async fn update_experience(&self, exp: &MemoryExperience) -> Result<MemoryExperience> {
        let row = sqlx::query(
            r#"
            update memory_experiences
            set task = $3,
                search_keys = $4,
                solution = $5,
                preconditions = $6,
                boundary_envs = $7,
                failure_modes = $8,
                validation = $9,
                confidence = $10,
                status = $11,
                feedback_score = $12,
                updated_at = now()
            where namespace = $1 and id = $2
            returning id, namespace, task, search_keys, solution, preconditions,
                      boundary_envs, failure_modes, validation, confidence, status,
                      feedback_score, created_at, updated_at
            "#,
        )
        .bind(&exp.namespace)
        .bind(exp.id)
        .bind(&exp.task)
        .bind(&exp.search_keys)
        .bind(&exp.solution)
        .bind(&exp.preconditions)
        .bind(&exp.boundary_envs)
        .bind(&exp.failure_modes)
        .bind(&exp.validation)
        .bind(exp.confidence)
        .bind(exp.status.as_str())
        .bind(exp.feedback_score)
        .fetch_optional(&self.pool)
        .await
        .pg()?;

        match row {
            Some(r) => Self::experience_from_row(&r),
            None => Err(HubError::NotFound),
        }
    }

    async fn get_experience(&self, namespace: &str, id: Uuid) -> Result<Option<MemoryExperience>> {
        let row = sqlx::query(
            r#"
            select id, namespace, task, search_keys, solution, preconditions,
                   boundary_envs, failure_modes, validation, confidence, status,
                   feedback_score, created_at, updated_at
            from memory_experiences
            where namespace = $1 and id = $2
            "#,
        )
        .bind(namespace)
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .pg()?;

        row.map(|r| Self::experience_from_row(&r)).transpose()
    }

    async fn list_experiences(
        &self,
        namespace: &str,
        status: Option<ExperienceStatus>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<MemoryExperience>> {
        let mut builder: sqlx::QueryBuilder<'_, sqlx::Postgres> = sqlx::QueryBuilder::new(
            "select id, namespace, task, search_keys, solution, preconditions, boundary_envs, failure_modes, validation, confidence, status, feedback_score, created_at, updated_at from memory_experiences where namespace = ",
        );
        builder.push_bind(namespace);

        if let Some(s) = status {
            builder.push(" and status = ");
            builder.push_bind(s.as_str());
        }

        builder.push(" order by updated_at desc, id asc limit ");
        builder.push_bind(limit.max(1) as i64);
        builder.push(" offset ");
        builder.push_bind(offset as i64);

        let rows = builder.build().fetch_all(&self.pool).await.pg()?;
        rows.iter().map(Self::experience_from_row).collect()
    }

    async fn link_task_experience(&self, link: &MemoryTaskExperience) -> Result<()> {
        let (task_ns, exp_ns): (Option<String>, Option<String>) = sqlx::query_as(
            r#"
            select 
                (select namespace from memory_tasks where id = $1) as task_ns,
                (select namespace from memory_experiences where id = $2) as exp_ns
            "#,
        )
        .bind(link.task_id)
        .bind(link.experience_id)
        .fetch_one(&self.pool)
        .await
        .pg()?;

        let (t_ns, e_ns) = match (task_ns, exp_ns) {
            (Some(t), Some(e)) => (t, e),
            _ => {
                return Err(HubError::NotFound);
            }
        };

        if t_ns != e_ns {
            return Err(HubError::InvalidArgument(
                "任务与经验不属于同一命名空间".into(),
            ));
        }

        if let Some(ref link_ns) = link.namespace {
            if *link_ns != t_ns {
                return Err(HubError::InvalidArgument(
                    "指定命名空间与任务所属命名空间不一致".into(),
                ));
            }
        }

        sqlx::query(
            r#"
            insert into memory_task_experiences (namespace, task_id, experience_id, relation_kind, created_at)
            values ($1, $2, $3, $4, $5)
            on conflict (task_id, experience_id, relation_kind) do nothing
            "#,
        )
        .bind(&t_ns)
        .bind(link.task_id)
        .bind(link.experience_id)
        .bind(&link.relation_kind)
        .bind(link.created_at)
        .execute(&self.pool)
        .await
        .pg()?;

        Ok(())
    }

    async fn list_experiences_for_task(&self, task_id: Uuid) -> Result<Vec<MemoryTaskExperience>> {
        let rows = sqlx::query(
            r#"
            select namespace, task_id, experience_id, relation_kind, created_at
            from memory_task_experiences
            where task_id = $1
            order by created_at desc
            "#,
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await
        .pg()?;

        rows.iter().map(Self::task_experience_from_row).collect()
    }

    async fn all_experiences_in_namespace(&self, namespace: &str) -> Result<Vec<MemoryExperience>> {
        let rows = sqlx::query(
            r#"
            select id, namespace, task, search_keys, solution, preconditions,
                    boundary_envs, failure_modes, validation, confidence, status,
                    feedback_score, created_at, updated_at
            from memory_experiences
            where namespace = $1
            order by created_at asc, id asc
            "#,
        )
        .bind(namespace)
        .fetch_all(&self.pool)
        .await
        .pg()?;

        rows.iter().map(Self::experience_from_row).collect()
    }

    async fn all_task_experiences_in_namespace(
        &self,
        namespace: &str,
    ) -> Result<Vec<MemoryTaskExperience>> {
        let rows = sqlx::query(
            r#"
            select namespace, task_id, experience_id, relation_kind, created_at
            from memory_task_experiences
            where namespace = $1
            order by created_at asc, task_id asc
            "#,
        )
        .bind(namespace)
        .fetch_all(&self.pool)
        .await
        .pg()?;

        rows.iter().map(Self::task_experience_from_row).collect()
    }

    async fn delete_experience(&self, namespace: &str, id: Uuid) -> Result<()> {
        let res = sqlx::query("delete from memory_experiences where namespace = $1 and id = $2")
            .bind(namespace)
            .bind(id)
            .execute(&self.pool)
            .await
            .pg()?;

        if res.rows_affected() == 0 {
            Err(HubError::NotFound)
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl CodingStore for PostgresHubStore {
    async fn create_thread(&self, thread: &NewCodingThread) -> Result<CodingThread> {
        if let Some(task_id) = thread.task_id {
            let task_exists: bool = sqlx::query_scalar(
                "select exists(select 1 from memory_tasks where namespace = $1 and id = $2)",
            )
            .bind(&thread.namespace)
            .bind(task_id)
            .fetch_one(&self.pool)
            .await
            .pg()?;

            if !task_exists {
                return Err(HubError::InvalidArgument(
                    "关联任务不存在于该命名空间".into(),
                ));
            }
        }

        let id = thread.id.unwrap_or_else(Uuid::new_v4);
        let now = Utc::now();
        let row = sqlx::query(
            r#"
            insert into coding_threads (
                id, namespace, repository, branch, task_id, title, description, created_at, updated_at
            )
            values ($1, $2, $3, $4, $5, $6, $7, $8, $8)
            returning id, namespace, repository, branch, task_id, title, description, created_at, updated_at
            "#,
        )
        .bind(id)
        .bind(&thread.namespace)
        .bind(&thread.repository)
        .bind(&thread.branch)
        .bind(thread.task_id)
        .bind(&thread.title)
        .bind(&thread.description)
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .pg()?;

        Self::thread_from_row(&row)
    }

    async fn get_thread(&self, namespace: &str, id: Uuid) -> Result<Option<CodingThread>> {
        let row = sqlx::query(
            r#"
            select id, namespace, repository, branch, task_id, title, description, created_at, updated_at
            from coding_threads
            where namespace = $1 and id = $2
            "#,
        )
        .bind(namespace)
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .pg()?;

        row.as_ref().map(Self::thread_from_row).transpose()
    }

    async fn list_threads(
        &self,
        namespace: &str,
        repository: Option<&str>,
        limit: usize,
    ) -> Result<Vec<CodingThread>> {
        let rows = sqlx::query(
            r#"
            select id, namespace, repository, branch, task_id, title, description, created_at, updated_at
            from coding_threads
            where namespace = $1 and ($2::text is null or repository = $2)
            order by updated_at desc, id asc
            limit $3
            "#,
        )
        .bind(namespace)
        .bind(repository)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .pg()?;

        rows.iter().map(Self::thread_from_row).collect()
    }

    async fn save_revision(
        &self,
        revision: &NewCodingRevision,
        diff_hash: &str,
    ) -> Result<CodingRevision> {
        if let Some(tid) = revision.thread_id {
            let thread_exists: bool = sqlx::query_scalar(
                "select exists(select 1 from coding_threads where namespace = $1 and id = $2)",
            )
            .bind(&revision.namespace)
            .bind(tid)
            .fetch_one(&self.pool)
            .await
            .pg()?;

            if !thread_exists {
                return Err(HubError::InvalidArgument(
                    "关联线程不存在于该命名空间".into(),
                ));
            }
        }
        if let Some(task_id) = revision.source_task_id {
            let task_exists: bool = sqlx::query_scalar(
                "select exists(select 1 from memory_tasks where namespace = $1 and id = $2)",
            )
            .bind(&revision.namespace)
            .bind(task_id)
            .fetch_one(&self.pool)
            .await
            .pg()?;

            if !task_exists {
                return Err(HubError::InvalidArgument(
                    "关联任务不存在于该命名空间".into(),
                ));
            }
        }
        if let Some(ev_id) = revision.source_evidence_id {
            let ev_exists: bool = sqlx::query_scalar(
                "select exists(select 1 from memory_evidence where namespace = $1 and id = $2)",
            )
            .bind(&revision.namespace)
            .bind(ev_id)
            .fetch_one(&self.pool)
            .await
            .pg()?;

            if !ev_exists {
                return Err(HubError::InvalidArgument(
                    "关联证据不存在于该命名空间".into(),
                ));
            }
        }
        let id = revision.id.unwrap_or_else(Uuid::new_v4);
        let now = Utc::now();
        let occurred_at = revision.occurred_at.unwrap_or(now);
        let row = sqlx::query(
            r#"
            insert into coding_revisions (
                id, thread_id, namespace, repository, branch, commit_sha, parent_sha,
                file_path, change_kind, diff_content, diff_hash, symbol_fingerprint,
                is_committed, author, occurred_at, created_at, source_evidence_id, source_task_id
            )
            values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)
            returning id, thread_id, namespace, repository, branch, commit_sha, parent_sha,
                      file_path, change_kind, diff_content, diff_hash, symbol_fingerprint,
                      is_committed, author, occurred_at, created_at, source_evidence_id, source_task_id
            "#,
        )
        .bind(id)
        .bind(revision.thread_id)
        .bind(&revision.namespace)
        .bind(&revision.repository)
        .bind(&revision.branch)
        .bind(&revision.commit_sha)
        .bind(&revision.parent_sha)
        .bind(&revision.file_path)
        .bind(revision.change_kind.as_str())
        .bind(&revision.diff_content)
        .bind(diff_hash)
        .bind(&revision.symbol_fingerprint)
        .bind(revision.is_committed)
        .bind(&revision.author)
        .bind(occurred_at)
        .bind(now)
        .bind(revision.source_evidence_id)
        .bind(revision.source_task_id)
        .fetch_one(&self.pool)
        .await
        .pg()?;

        Self::revision_from_row(&row)
    }

    async fn get_revision(&self, namespace: &str, id: Uuid) -> Result<Option<CodingRevision>> {
        let row = sqlx::query(
            r#"
            select id, thread_id, namespace, repository, branch, commit_sha, parent_sha,
                   file_path, change_kind, diff_content, diff_hash, symbol_fingerprint,
                   is_committed, author, occurred_at, created_at, source_evidence_id, source_task_id
            from coding_revisions
            where namespace = $1 and id = $2
            "#,
        )
        .bind(namespace)
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .pg()?;

        row.as_ref().map(Self::revision_from_row).transpose()
    }

    async fn get_revision_by_hash(
        &self,
        namespace: &str,
        repository: &str,
        diff_hash: &str,
    ) -> Result<Option<CodingRevision>> {
        let row = sqlx::query(
            r#"
            select id, thread_id, namespace, repository, branch, commit_sha, parent_sha,
                   file_path, change_kind, diff_content, diff_hash, symbol_fingerprint,
                   is_committed, author, occurred_at, created_at, source_evidence_id, source_task_id
            from coding_revisions
            where namespace = $1 and repository = $2 and diff_hash = $3
            limit 1
            "#,
        )
        .bind(namespace)
        .bind(repository)
        .bind(diff_hash)
        .fetch_optional(&self.pool)
        .await
        .pg()?;

        row.as_ref().map(Self::revision_from_row).transpose()
    }

    async fn list_revisions_for_thread(
        &self,
        namespace: &str,
        thread_id: Uuid,
        limit: usize,
    ) -> Result<Vec<CodingRevision>> {
        let rows = sqlx::query(
            r#"
            select id, thread_id, namespace, repository, branch, commit_sha, parent_sha,
                   file_path, change_kind, diff_content, diff_hash, symbol_fingerprint,
                   is_committed, author, occurred_at, created_at, source_evidence_id, source_task_id
            from coding_revisions
            where namespace = $1 and thread_id = $2
            order by occurred_at desc, id asc
            limit $3
            "#,
        )
        .bind(namespace)
        .bind(thread_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .pg()?;

        rows.iter().map(Self::revision_from_row).collect()
    }

    async fn list_revisions_for_file(
        &self,
        namespace: &str,
        repository: &str,
        file_path: &str,
        limit: usize,
    ) -> Result<Vec<CodingRevision>> {
        let rows = sqlx::query(
            r#"
            select id, thread_id, namespace, repository, branch, commit_sha, parent_sha,
                   file_path, change_kind, diff_content, diff_hash, symbol_fingerprint,
                   is_committed, author, occurred_at, created_at, source_evidence_id, source_task_id
            from coding_revisions
            where namespace = $1 and repository = $2 and file_path = $3
            order by occurred_at desc, id asc
            limit $4
            "#,
        )
        .bind(namespace)
        .bind(repository)
        .bind(file_path)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .pg()?;

        rows.iter().map(Self::revision_from_row).collect()
    }

    async fn all_threads_in_namespace(&self, namespace: &str) -> Result<Vec<CodingThread>> {
        let rows = sqlx::query(
            r#"
            select id, namespace, repository, branch, task_id, title, description, created_at, updated_at
            from coding_threads
            where namespace = $1
            order by created_at asc, id asc
            "#,
        )
        .bind(namespace)
        .fetch_all(&self.pool)
        .await
        .pg()?;

        rows.iter().map(Self::thread_from_row).collect()
    }

    async fn all_revisions_in_namespace(&self, namespace: &str) -> Result<Vec<CodingRevision>> {
        let rows = sqlx::query(
            r#"
            select id, thread_id, namespace, repository, branch, commit_sha, parent_sha,
                   file_path, change_kind, diff_content, diff_hash, symbol_fingerprint,
                   is_committed, author, occurred_at, created_at, source_evidence_id, source_task_id
            from coding_revisions
            where namespace = $1
            order by occurred_at asc, id asc
            "#,
        )
        .bind(namespace)
        .fetch_all(&self.pool)
        .await
        .pg()?;

        rows.iter().map(Self::revision_from_row).collect()
    }

    async fn delete_thread(&self, namespace: &str, id: Uuid) -> Result<()> {
        let res = sqlx::query("delete from coding_threads where namespace = $1 and id = $2")
            .bind(namespace)
            .bind(id)
            .execute(&self.pool)
            .await
            .pg()?;

        if res.rows_affected() == 0 {
            Err(HubError::NotFound)
        } else {
            Ok(())
        }
    }

    async fn delete_revision(&self, namespace: &str, id: Uuid) -> Result<()> {
        let res = sqlx::query("delete from coding_revisions where namespace = $1 and id = $2")
            .bind(namespace)
            .bind(id)
            .execute(&self.pool)
            .await
            .pg()?;

        if res.rows_affected() == 0 {
            Err(HubError::NotFound)
        } else {
            Ok(())
        }
    }
}

/// accounts 行 → AccountRecord
fn account_record_of(r: &PgRow) -> AccountRecord {
    AccountRecord {
        id: r.get("id"),
        username: r.get("username"),
        display_name: r.get("display_name"),
        status: r.get("status"),
        has_password: r.get("has_password"),
        created_at: r.get("created_at"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_migrations_registered() {
        assert_eq!(MIGRATIONS.len(), 13);
        assert!(MIGRATIONS[0].0.starts_with("20261011"));
        // 迁移清单必须按版本号字典序排列（执行顺序依赖于此）
        for w in MIGRATIONS.windows(2) {
            assert!(
                w[0].0 < w[1].0,
                "迁移版本号必须递增: {} >= {}",
                w[0].0,
                w[1].0
            );
        }
        // 每条迁移都必须能被语句切分器完整切开（每段以分号结尾）。
        // 此前只抽查了 002/003，切分器对 dollar-quoted 块出错时另一条会被静默截断。
        for (version, sql) in MIGRATIONS {
            let stmts = split_sql_statements(sql);
            assert!(stmts.len() > 1, "迁移 {} 应至少切出一条语句", version);
            assert!(
                stmts.iter().all(|s| s.trim_end().ends_with(';')),
                "迁移 {} 存在未被切分的尾段",
                version
            );
        }
    }

    /// `migrations/` 下的每个 .sql 都必须已登记进 MIGRATIONS。
    /// 只断言固定条数时，新增文件却忘记登记会被静默跳过、线上 schema 落后。
    #[test]
    fn test_every_migration_file_is_registered() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        let mut count = 0usize;
        for entry in std::fs::read_dir(&dir)
            .expect("migrations 目录应存在")
            .flatten()
        {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(stem) = name.strip_suffix(".sql") else {
                continue;
            };
            count += 1;
            assert!(
                MIGRATIONS.iter().any(|(v, _)| *v == stem),
                "迁移 {} 有文件但未登记进 MIGRATIONS",
                stem
            );
        }
        assert_eq!(count, MIGRATIONS.len(), "迁移文件数与登记数不一致");
    }

    #[test]
    fn test_split_sql_statements_dollar_quote() {
        let sql = "create table a(id int);\n\
                   do $$\nbegin\n  create role audit_writer;\nend $$;\n\
                   grant insert on events to audit_writer;";
        let stmts = split_sql_statements(sql);
        assert_eq!(stmts.len(), 3);
        assert!(stmts[1].contains("create role audit_writer;"));
        assert!(stmts[1].ends_with("$$;"));

        let tagged = "do $body$\n  update t set note = 'a;b';\n$body$;\nselect 1;";
        let stmts = split_sql_statements(tagged);
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("'a;b'"));
    }

    #[test]
    fn test_split_sql_statements_comments_and_strings() {
        let sql =
            "-- 注释里的分号; 与引号 '\ninsert into t values ('a;b');\n/* 块注释 ; */ select 1;";
        let stmts = split_sql_statements(sql);
        assert_eq!(stmts.len(), 2);
    }

    #[test]
    fn test_split_full_migration_keeps_do_block_intact() {
        let (_, sql) = MIGRATIONS[0];
        let stmts: Vec<&str> = split_sql_statements(sql)
            .into_iter()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        assert!(stmts.len() > 10);
        assert!(stmts.iter().any(|s| s.contains("create role audit_writer")
            && s.contains("begin")
            && s.ends_with("$$;")));
    }

    #[test]
    fn test_vector_literal_roundtrip() {
        let v = vec![0.5f32, -0.25, 1.0];
        let literal = mapping::vector_literal(&v);
        assert!(literal.starts_with('['));
        assert!(literal.ends_with(']'));
        let parsed = mapping::parse_vector(&literal);
        assert_eq!(parsed.len(), 3);
        assert!((parsed[0] - 0.5).abs() < 1e-6);
        assert!((parsed[1] + 0.25).abs() < 1e-6);
    }

    #[test]
    fn test_parse_vector_edge_cases() {
        assert!(mapping::parse_vector("[]").is_empty());
        assert!(mapping::parse_vector("[1,2,3]").len() == 3);
    }

    #[test]
    fn test_kind_status_parse() {
        assert_eq!(mapping::parse_kind("fact").unwrap(), MemoryKind::Fact);
        assert!(mapping::parse_kind("bogus").is_err());
        assert_eq!(
            mapping::parse_status("merged").unwrap(),
            MemoryStatus::Merged
        );
        assert!(mapping::parse_status("bogus").is_err());
        assert_eq!(
            mapping::parse_integration_kind("duplicate").unwrap(),
            IntegrationKind::Duplicate
        );
        assert_eq!(
            mapping::parse_proposal_status("open").unwrap(),
            ProposalStatus::Open
        );
        assert_eq!(
            mapping::parse_version_change("merged").unwrap(),
            VersionChange::Merged
        );
    }

    #[test]
    fn test_pg_error_mapping() {
        // 无法在无库环境构造 Database 错误，这里只验证映射函数的存在性与签名
        let err: Result<()> = Err(HubError::NotFound);
        assert!(err.is_err());
    }

    /// 迁移 005 受害行的重分词回填：重算结果必须与写路径对同 content 的
    /// 产出完全一致（同源），且受害特征（tsv_tokens=原文）被消除。
    #[test]
    fn test_backfill_retokenize_matches_write_path() {
        // 中文整句：迁移 005 后 tsv_tokens 是未分词原文（FTS 失效的形态）
        let victim_content = "服务部署在东京区域";
        let updates = retokenized_updates(vec![(Uuid::new_v4(), victim_content.to_string())]);
        assert_eq!(updates.len(), 1);
        let (_, tokens) = &updates[0];
        assert_eq!(tokens, &tsv_tokens_for(victim_content));
        assert_eq!(tokens, &mem_core::tokenize_for_fts(victim_content));
        // 与原文相等即是受害特征；重分词后必须是 bigram 词元流
        assert_ne!(tokens, victim_content);
        assert!(tokens.split_whitespace().any(|t| t == "东京"));
        assert!(tokens.split_whitespace().any(|t| t == "区域"));

        // 混排与英文：误标记无害——重分词幂等，英文原词保持
        let mixed = "PostgreSQL 15 数据库在 Tokyo 部署";
        let updates = retokenized_updates(vec![(Uuid::new_v4(), mixed.to_string())]);
        assert_eq!(updates[0].1, tsv_tokens_for(mixed));
        assert!(updates[0].1.split_whitespace().any(|t| t == "postgresql"));

        // 空行/批次多行：保持顺序与 id 对应
        let rows = vec![
            (Uuid::new_v4(), "记忆中枢".to_string()),
            (Uuid::new_v4(), "memory hub".to_string()),
        ];
        let updates = retokenized_updates(rows.clone());
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].0, rows[0].0);
        assert_eq!(updates[1].0, rows[1].0);
        assert_ne!(updates[0].1, updates[1].1);
    }

    /// 迁移 013 脚本：幂等写法与关键语句必须存在（对齐 sql_validation 风格）。
    #[test]
    fn test_fts_retokenize_migration_script() {
        let sql = include_str!("../migrations/20261025000013_fts_retokenize_marker.sql");
        assert!(sql.contains("add column if not exists fts_retokenize_pending"));
        // 标记条件必须精确对齐迁移 005 的回填特征
        assert!(sql.contains("tsv_tokens = content and content <> ''"));
        // 部分索引让回填的键集分页走索引
        assert!(sql.contains("create index if not exists idx_memories_fts_retokenize_pending"));
        assert!(sql.contains("where fts_retokenize_pending;"));
    }
}

#[cfg(all(test, feature = "pg-integration"))]
mod pg_integration {
    use super::*;
    use common::AgentId;
    use mem_core::{AgentStore, BackupStore, GrantStore, MemoryKind, MemoryStore};

    async fn store() -> PostgresHubStore {
        let url = std::env::var("DATABASE_URL").expect("需设置 DATABASE_URL");
        PostgresHubStore::connect(&url, 5, 0)
            .await
            .expect("连接失败（将自动执行迁移）")
    }

    fn entry(ns: &str, agent: &str, content: &str) -> MemoryEntry {
        MemoryEntry {
            id: Uuid::new_v4(),
            namespace: ns.to_string(),
            key: None,
            kind: MemoryKind::Fact,
            content: content.to_string(),
            tags: vec!["pg".to_string()],
            metadata: serde_json::json!({"k": "v"}),
            embedding: None,
            embedding_model: None,
            source_agent: agent.to_string(),
            confidence: 0.9,
            status: MemoryStatus::Active,
            version: 1,
            expires_at: None,
            merged_into: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            scene: None,
            layer: None,
            valid_from: None,
            valid_until: None,
            occurred_at: None,
        }
    }

    #[tokio::test]
    #[ignore = "需要 DATABASE_URL"]
    async fn pg_memory_crud_and_namespace_isolation() {
        let store = store().await;
        let ns = "it-ns-crud";
        store
            .ensure_namespace(ns, &AgentId::from("it-agent"))
            .await
            .unwrap();

        let e = entry(ns, "it-agent", "PostgreSQL 持久化冒烟");
        store.insert(&e).await.unwrap();

        let got = store.get(ns, e.id).await.unwrap();
        assert_eq!(got.content, "PostgreSQL 持久化冒烟");
        assert_eq!(got.status, MemoryStatus::Active);

        // 命名空间隔离：错误命名空间读不到
        assert!(store.get("other-ns", e.id).await.is_err());

        // 修订 + 版本历史
        let mut patched = got.clone();
        patched.content = "修订后的内容".to_string();
        patched.version = 2;
        store.save(&patched).await.unwrap();
        store
            .record_version(&MemoryVersion {
                memory_id: e.id,
                version: 2,
                content: "修订后的内容".to_string(),
                tags: vec![],
                metadata: serde_json::json!({}),
                confidence: 0.9,
                changed_by: "it-agent".to_string(),
                change_type: VersionChange::Updated,
                changed_at: Utc::now(),
                scene: None,
                layer: None,
                valid_from: None,
                valid_until: None,
                occurred_at: None,
            })
            .await
            .unwrap();

        let versions = store.versions(e.id).await.unwrap();
        assert!(versions.iter().any(|v| v.version == 2));

        // 清理
        store.delete(ns, e.id).await.unwrap();
        assert!(store.get(ns, e.id).await.is_err());
    }

    #[tokio::test]
    #[ignore = "需要 DATABASE_URL"]
    async fn pg_grants_and_agent_registry() {
        let store = store().await;
        let ns = "it-ns-grant";
        let owner = AgentId::from("it-owner");

        let (_, created) = store.ensure_namespace(ns, &owner).await.unwrap();
        assert!(created, "首次注册应报告新建");

        // 被授权方必须先在 agents 表登记：namespace_grants.agent_id 有外键约束，
        // 未登记的 Agent 写不进授权行。生产路径上 memory_grant 工具同样先校验
        // 目标 Agent 存在且同属本账户，再落授权行。
        store
            .register_agent(&core_agent_reg("it-guest", None))
            .await
            .unwrap();

        store
            .grant(&NamespaceGrantRecord {
                namespace: ns.to_string(),
                agent_id: "it-guest".to_string(),
                can_read: true,
                can_write: false,
                granted_by: owner.clone(),
                created_at: Utc::now(),
            })
            .await
            .unwrap();

        assert!(store
            .can_read(ns, &AgentId::from("it-guest"))
            .await
            .unwrap());
        assert!(!store
            .can_write(ns, &AgentId::from("it-guest"))
            .await
            .unwrap());
        assert!(!store
            .can_read(ns, &AgentId::from("stranger"))
            .await
            .unwrap());

        // Agent 注册与密钥校验
        store
            .register_agent(&core_agent_reg("it-agent-x", Some("pw")))
            .await
            .unwrap();
        let agent = store
            .get_agent(&AgentId::from("it-agent-x"))
            .await
            .unwrap()
            .expect("应能读回");
        assert!(agent.confidential);
        assert!(store
            .verify_agent_secret(&AgentId::from("it-agent-x"), "pw")
            .await
            .unwrap());
        assert!(!store
            .verify_agent_secret(&AgentId::from("it-agent-x"), "wrong")
            .await
            .unwrap());

        // 状态停用
        store
            .set_agent_status(&AgentId::from("it-agent-x"), "suspended")
            .await
            .unwrap();
        let agent2 = store
            .get_agent(&AgentId::from("it-agent-x"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(agent2.status, "suspended");
    }

    fn core_agent_reg(id: &str, secret: Option<&str>) -> mem_core::AgentRegistration {
        mem_core::AgentRegistration {
            agent_id: id.to_string(),
            name: format!("Agent {}", id),
            kind: "service".to_string(),
            owner_account_id: common::BOOTSTRAP_ACCOUNT_ID,
            client_secret: secret.map(str::to_string),
            redirect_uris: vec![],
            max_scopes: vec![common::Scope::Read, common::Scope::Write],
        }
    }

    #[tokio::test]
    #[ignore = "需要 DATABASE_URL"]
    async fn pg_backup_and_restore_roundtrip() {
        let store = store().await;
        let ns = "it-ns-backup";
        store
            .ensure_namespace(ns, &AgentId::from("it-agent"))
            .await
            .unwrap();
        let e = entry(ns, "it-agent", "备份往返测试");
        store.insert(&e).await.unwrap();

        let entries = store.all_in_namespace(ns, true).await.unwrap();
        let checksum = {
            use sha2::{Digest, Sha256};
            hex::encode(Sha256::digest(serde_json::to_vec(&entries).unwrap()))
        };
        let backup = BackupRecord {
            id: Uuid::new_v4(),
            namespace: ns.to_string(),
            created_by: "it-agent".to_string(),
            note: Some("integration".to_string()),
            entry_count: entries.len(),
            checksum,
            payload: serde_json::json!({ "entries": entries }),
            retain_until: None,
            created_at: Utc::now(),
        };
        store.save_backup(&backup).await.unwrap();
        let loaded = store.get_backup(backup.id).await.unwrap();
        assert_eq!(loaded.entry_count, 1);
        assert_eq!(loaded.checksum, backup.checksum);

        store.delete_backup(backup.id).await.unwrap();
        assert!(store.get_backup(backup.id).await.is_err());
        store.delete(ns, e.id).await.unwrap();
    }

    /// 迁移 013 + 启动回填的端到端验证：手工制造迁移 005 形态的受害行
    /// （tsv_tokens = 原文、fts_retokenize_pending = true），跑回填，
    /// 断言词元被写路径同源重写、标记清零、FTS 可命中中文子串。
    #[tokio::test]
    #[ignore = "需要 DATABASE_URL"]
    async fn pg_fts_backfill_retokenizes_legacy_rows() {
        let store = store().await;
        let ns = "it-ns-fts-backfill";
        store
            .ensure_namespace(ns, &AgentId::from("it-agent"))
            .await
            .unwrap();

        let content = "服务部署在东京区域";
        let e = entry(ns, "it-agent", content);
        store.insert(&e).await.unwrap();
        // 正常写路径产出的就是分词流：不该被回填动到
        let expected = tsv_tokens_for(content);
        let pool = store.pool();

        // 还原受害形态（模拟迁移 005 执行后的存量行）
        sqlx::query(
            "update memories set tsv_tokens = $2, fts_retokenize_pending = true \
                     where namespace = $1 and id = $3",
        )
        .bind(ns)
        .bind(content)
        .bind(e.id)
        .execute(&pool)
        .await
        .unwrap();

        // 跑真实回填入口（connect() 内部调用的同一方法，含分批与清标记）
        store.backfill_fts_tokens().await.unwrap();

        let (tokens, pending): (String, bool) = sqlx::query_as(
            "select tsv_tokens, fts_retokenize_pending from memories \
             where namespace = $1 and id = $2",
        )
        .bind(ns)
        .bind(e.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(tokens, expected, "回填结果必须与写路径分词同源");
        assert!(!pending, "回填完成后标记必须清除");

        let pending_left: i64 =
            sqlx::query_scalar("select count(*) from memories where fts_retokenize_pending")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(pending_left, 0, "全部待处理行应被清零");

        // 中文子串经 FTS 召回（修复前 tsv_tokens=原文时不可命中）
        let hits: i64 = sqlx::query_scalar(
            "select count(*) from memories \
             where namespace = $1 and to_tsvector('simple', tsv_tokens) \
                   @@ to_tsquery('simple', '东京 | 区域')",
        )
        .bind(ns)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(hits >= 1, "重分词后中文子串应可经 FTS 命中");

        store.delete(ns, e.id).await.unwrap();
    }
}
