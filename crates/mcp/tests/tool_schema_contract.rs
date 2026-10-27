//! MCP 工具契约黄金快照测试
//!
//! 目的：把"升级前 38 个工具的名称、参数、必填项、scope、只读/破坏性/幂等标记"
//! 固化成快照文件，任何改动都必须显式刷新——这样"轻量化不得破坏旧客户端"
//! 就从一个口头承诺变成一条会在 CI 里失败的断言。
//!
//! 刷新方式（**只有确认是有意的契约变更才允许**）：
//!
//! ```bash
//! HUB_WRITE_TOOL_SCHEMA_GOLDEN=1 cargo test -p mcp --test tool_schema_contract
//! ```
//!
//! 刷新后必须人工 review diff：新增工具、新增**可选**参数是兼容的；
//! 删除/重命名工具、修改既有参数类型、把可选参数改成必填，都是破坏性变更。

use serde_json::{json, Value};
use std::path::PathBuf;

/// 快照文件路径（相对 crate 根目录）
fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("data")
        .join("tool_schemas.golden.json")
}

/// 把工具定义序列化成稳定的契约快照
///
/// 排序固定（按名称），确保同一份代码每次生成的字节内容完全一致。
fn snapshot() -> Value {
    let mut defs: Vec<Value> = mcp::get_tool_definitions()
        .iter()
        .map(|d| {
            // 必填参数单独列出：客户端最容易踩的就是"以前不传也行，现在必填了"
            let required = d
                .input_schema
                .get("required")
                .cloned()
                .unwrap_or_else(|| json!([]));
            json!({
                "name": d.name,
                "required_scope": d.required_scope,
                "read_only": d.read_only,
                "destructive": d.destructive,
                "idempotent": d.idempotent,
                "required": required,
                "input_schema": d.input_schema,
            })
        })
        .collect();
    defs.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or_default()
            .cmp(b["name"].as_str().unwrap_or_default())
    });
    json!({
        "format_version": 1,
        "tool_count": defs.len(),
        "tools": defs,
    })
}

#[test]
fn test_tool_contract_matches_snapshot() {
    let path = golden_path();
    let current = snapshot();

    // 只认 "1"：`HUB_WRITE_TOOL_SCHEMA_GOLDEN=0` 之类的误设不得覆盖快照
    if std::env::var("HUB_WRITE_TOOL_SCHEMA_GOLDEN").as_deref() == Ok("1") {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).expect("创建快照目录失败");
        }
        let text = serde_json::to_string_pretty(&current).expect("序列化快照失败");
        std::fs::write(&path, text + "\n").expect("写入快照失败");
        println!("[contract] 契约快照已写入 {}", path.display());
        return;
    }

    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "缺少工具契约快照 {}（{e}）；初始化请运行 HUB_WRITE_TOOL_SCHEMA_GOLDEN=1 cargo test -p mcp --test tool_schema_contract",
            path.display()
        )
    });
    let golden: Value = serde_json::from_str(&raw).expect("快照不是合法 JSON");
    assert_eq!(
        golden,
        current,
        "MCP 工具契约与快照不一致。若是有意的兼容性变更，请刷新快照并 review diff：\n{}",
        path.display()
    );
}

#[test]
fn test_snapshot_covers_all_tools_and_keeps_base_tools() {
    let current = snapshot();
    assert_eq!(
        current["tool_count"],
        json!(38),
        "工具总数应为 18 个基础 + 20 个认知/治理工具"
    );

    let names: Vec<&str> = current["tools"]
        .as_array()
        .expect("tools 应为数组")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();

    // 18 个基础工具：轻量化绝不能以"删工具"的方式实现
    for expected in [
        "memory_put",
        "memory_get",
        "memory_update",
        "memory_delete",
        "memory_search",
        "memory_list",
        "memory_stats",
        "memory_history",
        "memory_backup_create",
        "memory_backup_list",
        "memory_restore",
        "memory_integrate_scan",
        "memory_integration_list",
        "memory_integration_resolve",
        "memory_consolidate",
        "memory_consolidation_stats",
        "memory_grant",
        "memory_revoke",
    ] {
        assert!(names.contains(&expected), "缺少基础工具: {expected}");
    }

    // 10 个认知工具：Lite 模式下同样必须全部保留
    for expected in [
        "cognitive_chat_propose",
        "cognitive_proposal_resolve",
        "cognitive_intentions_list",
        "working_task_start",
        "working_task_finish",
        "working_experience_distill",
        "working_experience_feedback",
        "working_experience_search",
        "working_experience_transition",
        "working_experience_history",
        "coding_thread_create",
        "coding_thread_list",
        "coding_revision_record",
        "coding_revision_list",
        "coding_revision_get",
        "memory_collection_upsert",
        "memory_collection_list",
        "memory_collection_delete",
        "memory_collection_bind",
        "memory_collection_unbind",
    ] {
        assert!(names.contains(&expected), "缺少认知工具: {expected}");
    }
}
