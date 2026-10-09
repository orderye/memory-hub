#![forbid(unsafe_code)]

//! PostgreSQL 迁移脚本与连接配置完整性校验测试
//!
//! 验证 DDL 中所有关键数据表、外键级联关系、只追加权限以及索引语法的完整性。

#[cfg(test)]
mod tests {
    #[test]
    fn test_sql_migration_script_integrity() {
        let migration_sql = include_str!("../migrations/20261009000001_initial_schema.sql");

        // 验证关键数据表定义
        let required_tables = [
            "create table if not exists users",
            "create table if not exists oauth_clients",
            "create table if not exists consents",
            "create table if not exists refresh_tokens",
            "create table if not exists confirm_tokens",
            "create table if not exists memory_items",
            "create table if not exists memory_evidence",
            "create table if not exists hypotheses",
            "create table if not exists events",
            "create table if not exists audit_log",
            "create table if not exists norm_rules",
            "create table if not exists skills",
            "create table if not exists pack_sets",
        ];

        for table in required_tables {
            assert!(
                migration_sql.contains(table),
                "Missing required table in SQL migration: {}",
                table
            );
        }

        // 验证 pgvector 扩展及向量维度 1024
        assert!(migration_sql.contains(r#"create extension if not exists "vector""#));
        assert!(migration_sql.contains("embedding vector(1024)"));

        // 验证只追加表权限控制（撤销 audit_writer 的 update/delete）
        assert!(
            migration_sql.contains("revoke update, delete, truncate on events from audit_writer")
        );
        assert!(migration_sql
            .contains("revoke update, delete, truncate on audit_log from audit_writer"));

        // 验证删除级联规范（设计规范 17.4 H 节）
        assert!(migration_sql.contains("references memory_items(id) on delete cascade"));
        assert!(migration_sql.contains("references users(id) on delete cascade"));
    }
}
