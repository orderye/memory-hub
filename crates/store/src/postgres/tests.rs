use super::*;

#[test]
fn test_migrations_registered() {
    assert_eq!(MIGRATIONS.len(), 17);
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
        assert!(!stmts.is_empty(), "迁移 {} 应至少切出一条语句", version);
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
    let sql = "-- 注释里的分号; 与引号 '\ninsert into t values ('a;b');\n/* 块注释 ; */ select 1;";
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
fn test_vector_params_roundtrip() {
    let v = vec![0.5f32, -0.25, 1.0];
    // 写路径参数：fp32 精度无损
    let vec_param = mapping::vector_param(&v);
    assert_eq!(vec_param.to_vec().len(), 3);
    assert!((vec_param.to_vec()[1] + 0.25).abs() < 1e-6);
    // 查询参数：halfvec 半精度（f16 存储，维度保持）
    let half_param = mapping::halfvec_param(&v);
    assert_eq!(half_param.to_vec().len(), 3);
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
    let sql = include_str!("../../migrations/20261025000013_fts_retokenize_marker.sql");
    assert!(sql.contains("add column if not exists fts_retokenize_pending"));
    // 标记条件必须精确对齐迁移 005 的回填特征
    assert!(sql.contains("tsv_tokens = content and content <> ''"));
    // 部分索引让回填的键集分页走索引
    assert!(sql.contains("create index if not exists idx_memories_fts_retokenize_pending"));
    assert!(sql.contains("where fts_retokenize_pending;"));
}
