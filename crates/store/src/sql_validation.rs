#![forbid(unsafe_code)]

//! PostgreSQL 迁移脚本完整性校验测试
//!
//! 验证 DDL 中所有关键数据表、外键级联关系、只追加权限以及索引语法的完整性。

#[cfg(test)]
mod tests {
    #[test]
    fn test_sql_migration_script_integrity() {
        let migration_sql = include_str!("../migrations/20261011000001_initial_schema.sql");

        // 验证关键表定义
        let required_tables = [
            "create table if not exists agents",
            "create table if not exists namespaces",
            "create table if not exists namespace_grants",
            "create table if not exists memories",
            "create table if not exists memory_versions",
            "create table if not exists memory_backups",
            "create table if not exists integration_proposals",
            "create table if not exists refresh_tokens",
            "create table if not exists confirm_tokens",
            "create table if not exists idempotency_keys",
            "create table if not exists rate_limit_buckets",
            "create table if not exists events",
            "create table if not exists audit_log",
        ];

        for table in required_tables {
            assert!(
                migration_sql.contains(table),
                "Missing required table in SQL migration: {}",
                table
            );
        }

        // pgvector 扩展与向量维度
        assert!(migration_sql.contains(r#"create extension if not exists "vector""#));
        assert!(migration_sql.contains("embedding vector(1024)"));

        // 只追加表权限控制
        assert!(
            migration_sql.contains("revoke update, delete, truncate on events from audit_writer")
        );
        assert!(migration_sql
            .contains("revoke update, delete, truncate on audit_log from audit_writer"));

        // 级联删除规范：命名空间删除 → 记忆/授权/提案一并清除
        assert!(migration_sql.contains("references namespaces(namespace) on delete cascade"));
        assert!(migration_sql.contains("references memories(id) on delete cascade"));
        assert!(migration_sql.contains("references agents(agent_id) on delete cascade"));

        // 记忆表的命名空间唯一键（upsert 语义基础）：
        // 初始 schema 声明全量 unique，迁移 003 将其替换为"仅 active 行唯一"的
        // 条件唯一索引（放行 merged 历史保留原 key）。两处都要成立，
        // 否则 upsert 语义与迁移链就与实际库结构脱节。
        assert!(migration_sql.contains("unique (namespace, key)"));
        let key_unique_sql =
            include_str!("../migrations/20261013000003_conditional_key_unique.sql");
        assert!(key_unique_sql.contains("drop constraint if exists memories_namespace_key_key"));
        assert!(
            key_unique_sql.contains("create unique index if not exists idx_memories_ns_key_active")
        );
        assert!(key_unique_sql.contains("on memories (namespace, key)"));
        assert!(key_unique_sql.contains("where status = 'active' and key is not null"));

        // 向量与全文索引
        assert!(migration_sql.contains("using hnsw (embedding vector_cosine_ops)"));
        assert!(migration_sql.contains("using gin (to_tsvector('simple', content))"));
    }

    #[test]
    fn test_migration_15_vector_index_overhaul() {
        let sql = include_str!("../migrations/20261027000015_vector_index_halfvec_partial.sql");

        // 死索引清理：tsv_tokens 接管后 content 表达式 GIN 零查询引用
        assert!(sql.contains("drop index if exists idx_memories_tsv"));

        // halfvec 半精度 + partial：表达式与查询侧 similar() 的 dist 表达式
        // 必须逐字一致（embedding::halfvec(1024)），否则规划器不认表达式索引；
        // 谓词覆盖两种查询形态（active / active+archived），merged 不进索引
        assert!(sql.contains(
            "using hnsw ((embedding::halfvec(1024)) halfvec_cosine_ops)\n  where status in ('active', 'archived')"
        ));
        // 旧全量 fp32 索引移除（查询形态已全部被部分索引覆盖）
        assert!(sql.contains("drop index if exists idx_memories_embedding"));

        // memories 高频 upsert：表级 autovacuum 收紧
        assert!(sql.contains("autovacuum_vacuum_scale_factor = 0.05"));
        assert!(sql.contains("autovacuum_analyze_scale_factor = 0.02"));
    }
}
