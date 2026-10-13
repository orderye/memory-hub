//! MCP 工具 JSON Schema 规范定义
//!
//! 18 个工具全部只做三件事：**存储**、**备份**、**整合**。
//! 每个 schema 都关闭 `additionalProperties` 并给出边界约束——
//! 这是与调用方 Agent 的契约：schema 之外的字段一律拒绝。

use serde_json::{json, Value};

/// 记忆类型枚举（schema 侧同步 domain 定义）
const KIND_ENUM: &[&str] = &[
    "fact",
    "preference",
    "episode",
    "note",
    "skill",
    "procedure",
];

/// 工具元数据与模式描述
pub struct ToolDefinition {
    pub name: &'static str,
    pub description: &'static str,
    pub read_only: bool,
    pub destructive: bool,
    pub idempotent: bool,
    /// 声明的作用域：memory:read | memory:write | memory:admin
    pub required_scope: &'static str,
    pub input_schema: Value,
}

/// 命名空间 schema 片段（全局复用）
fn ns_prop() -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "maxLength": 128,
        "pattern": "^[a-z0-9._*-]+$",
        "description": "记忆命名空间（小写字母/数字/-/_/./*）；首次写入方自动成为 owner"
    })
}

pub fn get_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        // ===== 存储 =====
        ToolDefinition {
            name: "memory_put",
            description: "写入一条长期记忆。同一 namespace + key 已存在时为 upsert（内容修订，版本 +1）；未提供 key 则新建。首次写入某命名空间的 Agent 自动成为其 owner 并获得读写授权。",
            read_only: false,
            destructive: false,
            idempotent: true,
            required_scope: "memory:write",
            input_schema: json!({
                "type": "object",
                "required": ["namespace", "kind", "content"],
                "additionalProperties": false,
                "properties": {
                    "namespace": ns_prop(),
                    "key": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 128,
                        "description": "业务去重键（命名空间内唯一）；提供后 put 走 upsert 语义"
                    },
                    "kind": { "enum": KIND_ENUM, "description": "记忆分类" },
                    "content": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 8192,
                        "description": "记忆正文"
                    },
                    "tags": {
                        "type": "array",
                        "maxItems": 32,
                        "items": { "type": "string", "maxLength": 64 }
                    },
                    "metadata": {
                        "type": "object",
                        "description": "自定义元数据（jsonb），序列化后不超过 16KB"
                    },
                    "confidence": {
                        "type": "number",
                        "minimum": 0,
                        "maximum": 1,
                        "default": 1.0,
                        "description": "置信度，用于检索加权"
                    },
                    "ttl_seconds": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "生存秒数；0 或省略表示不过期。到期由 worker 自动归档"
                    }
                }
            }),
        },
        ToolDefinition {
            name: "memory_get",
            description: "按 ID 或业务键读取一条记忆（返回脱敏后的条目，不含嵌入向量）。",
            read_only: true,
            destructive: false,
            idempotent: true,
            required_scope: "memory:read",
            input_schema: json!({
                "type": "object",
                "required": ["namespace"],
                "additionalProperties": false,
                "properties": {
                    "namespace": ns_prop(),
                    "id": {
                        "type": "string",
                        "format": "uuid",
                        "description": "记忆条目 ID"
                    },
                    "key": {
                        "type": "string",
                        "maxLength": 128,
                        "description": "业务键（与 id 二选一）"
                    }
                }
            }),
        },
        ToolDefinition {
            name: "memory_update",
            description: "修订一条记忆的部分字段（内容、分类、标签、元数据、置信度、过期时间）。每次修订版本 +1 并追加版本历史。",
            read_only: false,
            destructive: false,
            idempotent: true,
            required_scope: "memory:write",
            input_schema: json!({
                "type": "object",
                "required": ["namespace", "id"],
                "additionalProperties": false,
                "properties": {
                    "namespace": ns_prop(),
                    "id": { "type": "string", "format": "uuid" },
                    "content": { "type": "string", "minLength": 1, "maxLength": 8192 },
                    "kind": { "enum": KIND_ENUM },
                    "tags": {
                        "type": "array",
                        "maxItems": 32,
                        "items": { "type": "string", "maxLength": 64 }
                    },
                    "metadata": { "type": "object" },
                    "confidence": { "type": "number", "minimum": 0, "maximum": 1 },
                    "expires_in_seconds": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "重新设置过期：>0 秒后过期；0 清除过期；省略不变"
                    }
                }
            }),
        },
        ToolDefinition {
            name: "memory_delete",
            description: "停止使用一条记忆：软删除（归档为 archived，保留全部历史与备份价值）。已归档条目不可重复删除。",
            read_only: false,
            destructive: true,
            idempotent: true,
            required_scope: "memory:write",
            input_schema: json!({
                "type": "object",
                "required": ["namespace", "id"],
                "additionalProperties": false,
                "properties": {
                    "namespace": ns_prop(),
                    "id": { "type": "string", "format": "uuid" }
                }
            }),
        },
        ToolDefinition {
            name: "memory_search",
            description: "混合检索：关键词（FTS）+ 语义（向量）双路召回，五维加权融合后 MMR 多样性重排。",
            read_only: true,
            destructive: false,
            idempotent: true,
            required_scope: "memory:read",
            input_schema: json!({
                "type": "object",
                "required": ["namespace"],
                "additionalProperties": false,
                "properties": {
                    "namespace": ns_prop(),
                    "query_text": {
                        "type": "string",
                        "maxLength": 2048,
                        "default": "",
                        "description": "自然语言查询；为空时退化为列表召回"
                    },
                    "kinds": {
                        "type": "array",
                        "maxItems": 6,
                        "items": { "enum": KIND_ENUM }
                    },
                    "tags": {
                        "type": "array",
                        "maxItems": 32,
                        "items": { "type": "string", "maxLength": 64 }
                    },
                    "top_k": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 100,
                        "default": 10
                    },
                    "include_archived": {
                        "type": "boolean",
                        "default": false,
                        "description": "是否把已归档条目纳入候选"
                    }
                }
            }),
        },
        ToolDefinition {
            name: "memory_list",
            description: "按条件列举记忆（游标分页）。按 updated_at desc + id desc 稳定排序。",
            read_only: true,
            destructive: false,
            idempotent: true,
            required_scope: "memory:read",
            input_schema: json!({
                "type": "object",
                "required": ["namespace"],
                "additionalProperties": false,
                "properties": {
                    "namespace": ns_prop(),
                    "kinds": {
                        "type": "array",
                        "maxItems": 6,
                        "items": { "enum": KIND_ENUM }
                    },
                    "statuses": {
                        "type": "array",
                        "maxItems": 3,
                        "items": { "enum": ["active", "archived", "merged"] },
                        "default": ["active", "archived"]
                    },
                    "tags": {
                        "type": "array",
                        "maxItems": 32,
                        "items": { "type": "string", "maxLength": 64 }
                    },
                    "source_agent": {
                        "type": "string",
                        "maxLength": 128,
                        "description": "按写入 Agent 过滤"
                    },
                    "text_contains": {
                        "type": "string",
                        "maxLength": 256,
                        "description": "内容子串过滤（不区分大小写）"
                    },
                    "cursor": {
                        "type": "string",
                        "description": "上一页返回的 nextCursor"
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 200,
                        "default": 50
                    }
                }
            }),
        },
        ToolDefinition {
            name: "memory_stats",
            description: "命名空间统计：总量、各状态计数、按类型/Agent 分布、24 小时内到期数量。",
            read_only: true,
            destructive: false,
            idempotent: true,
            required_scope: "memory:read",
            input_schema: json!({
                "type": "object",
                "required": ["namespace"],
                "additionalProperties": false,
                "properties": {
                    "namespace": ns_prop()
                }
            }),
        },
        ToolDefinition {
            name: "memory_history",
            description: "查询一条记忆的版本历史（每次内容修订追加一条，含变更类型与操作者）。",
            read_only: true,
            destructive: false,
            idempotent: true,
            required_scope: "memory:read",
            input_schema: json!({
                "type": "object",
                "required": ["namespace", "id"],
                "additionalProperties": false,
                "properties": {
                    "namespace": ns_prop(),
                    "id": { "type": "string", "format": "uuid" }
                }
            }),
        },
        // ===== 备份 =====
        ToolDefinition {
            name: "memory_backup_create",
            description: "创建备份快照：某命名空间（或全量 `*`）内全部条目的不可变副本，带 SHA-256 校验和。",
            read_only: false,
            destructive: false,
            idempotent: true,
            required_scope: "memory:admin",
            input_schema: json!({
                "type": "object",
                "required": ["namespace"],
                "additionalProperties": false,
                "properties": {
                    "namespace": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 128,
                        "pattern": "^[a-z0-9._*-]+$",
                        "description": "目标命名空间；`*` 表示全量快照（需要通配读授权才能查看）"
                    },
                    "note": {
                        "type": "string",
                        "maxLength": 256,
                        "description": "备注（如变更原因）"
                    }
                }
            }),
        },
        ToolDefinition {
            name: "memory_backup_list",
            description: "列举备份快照（仅返回调用方有权读取的命名空间；全量备份需通配读授权）。",
            read_only: true,
            destructive: false,
            idempotent: true,
            required_scope: "memory:admin",
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "namespace": {
                        "type": "string",
                        "maxLength": 128,
                        "description": "省略表示列出全部有权快照"
                    }
                }
            }),
        },
        ToolDefinition {
            name: "memory_restore",
            description: "从快照恢复。dry_run 只预演差异；merge 合并（推荐）；replace 会先自动创建当前状态的安全备份再重建。恢复前校验校验和，损坏即拒绝。",
            read_only: false,
            destructive: true,
            idempotent: false,
            required_scope: "memory:admin",
            input_schema: json!({
                "type": "object",
                "required": ["backup_id"],
                "additionalProperties": false,
                "properties": {
                    "backup_id": { "type": "string", "format": "uuid" },
                    "mode": {
                        "enum": ["dry_run", "merge", "replace"],
                        "default": "merge"
                    }
                }
            }),
        },
        // ===== 整合 =====
        ToolDefinition {
            name: "memory_integrate_scan",
            description: "扫描命名空间，识别多 Agent 写入产生的重复（高相似）与冲突（同 key 不同内容）并生成整合提案。已有 open 提案的组会自动跳过。",
            read_only: false,
            destructive: false,
            idempotent: true,
            required_scope: "memory:write",
            input_schema: json!({
                "type": "object",
                "required": ["namespace"],
                "additionalProperties": false,
                "properties": {
                    "namespace": ns_prop(),
                    "dry_run": {
                        "type": "boolean",
                        "default": false,
                        "description": "true = 只预演不落提案"
                    },
                    "sim_threshold": {
                        "type": "number",
                        "minimum": 0,
                        "maximum": 1,
                        "description": "覆盖默认重复判定阈值"
                    }
                }
            }),
        },
        ToolDefinition {
            name: "memory_integration_list",
            description: "列举命名空间的整合提案（含成员条目、建议幸存者、状态）。",
            read_only: true,
            destructive: false,
            idempotent: true,
            required_scope: "memory:read",
            input_schema: json!({
                "type": "object",
                "required": ["namespace"],
                "additionalProperties": false,
                "properties": {
                    "namespace": ns_prop()
                }
            }),
        },
        ToolDefinition {
            name: "memory_integration_resolve",
            description: "裁决一个整合提案：merge 保留幸存条目（其余成员标记 merged 并保留溯源），dismiss 保留分立。",
            read_only: false,
            destructive: false,
            idempotent: true,
            required_scope: "memory:write",
            input_schema: json!({
                "type": "object",
                "required": ["proposal_id", "action"],
                "additionalProperties": false,
                "properties": {
                    "proposal_id": { "type": "string", "format": "uuid" },
                    "action": { "enum": ["merge", "dismiss"] },
                    "survivor_id": {
                        "type": "string",
                        "format": "uuid",
                        "description": "merge 时的幸存条目；省略用提案建议值，必须是提案成员"
                    },
                    "note": {
                        "type": "string",
                        "maxLength": 256,
                        "description": "裁决理由"
                    }
                }
            }),
        },
        ToolDefinition {
            name: "memory_consolidate",
            description: "一键触发整合中枢全量整理（重复检测、语义与Key冲突检测、陈旧度评估与可选自动策略合并）。返回全量整合报告与健康度指标。",
            read_only: false,
            destructive: false,
            idempotent: true,
            required_scope: "memory:write",
            input_schema: json!({
                "type": "object",
                "required": ["namespace"],
                "additionalProperties": false,
                "properties": {
                    "namespace": ns_prop(),
                    "dry_run": {
                        "type": "boolean",
                        "default": false,
                        "description": "true = 只预演不落提案"
                    },
                    "sim_threshold": {
                        "type": "number",
                        "minimum": 0,
                        "maximum": 1,
                        "description": "覆盖默认重复判定阈值"
                    },
                    "auto_merge": {
                        "type": "boolean",
                        "default": false,
                        "description": "是否对高相似度重复自动合并"
                    }
                }
            }),
        },
        ToolDefinition {
            name: "memory_consolidation_stats",
            description: "获取命名空间的记忆整合健康度统计指标（活跃条目数、重复组数、冲突组数、陈旧条目数、冗余率、冲突率与综合健康度评分）。",
            read_only: true,
            destructive: false,
            idempotent: true,
            required_scope: "memory:read",
            input_schema: json!({
                "type": "object",
                "required": ["namespace"],
                "additionalProperties": false,
                "properties": {
                    "namespace": ns_prop()
                }
            }),
        },
        // ===== 命名空间共享 =====
        ToolDefinition {
            name: "memory_grant",
            description: "把命名空间的读/写权限授予另一个 Agent。仅命名空间 owner（或通配管理员）可转授。",
            read_only: false,
            destructive: false,
            idempotent: true,
            required_scope: "memory:admin",
            input_schema: json!({
                "type": "object",
                "required": ["namespace", "agent_id"],
                "additionalProperties": false,
                "properties": {
                    "namespace": ns_prop(),
                    "agent_id": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 128,
                        "description": "被授权的 Agent 标识"
                    },
                    "can_read": { "type": "boolean", "default": true },
                    "can_write": { "type": "boolean", "default": false }
                }
            }),
        },
        ToolDefinition {
            name: "memory_revoke",
            description: "撤销某 Agent 对命名空间的访问授权。仅 owner（或通配管理员）可操作。",
            read_only: false,
            destructive: true,
            idempotent: true,
            required_scope: "memory:admin",
            input_schema: json!({
                "type": "object",
                "required": ["namespace", "agent_id"],
                "additionalProperties": false,
                "properties": {
                    "namespace": ns_prop(),
                    "agent_id": { "type": "string", "minLength": 1, "maxLength": 128 }
                }
            }),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_count_and_names() {
        let defs = get_tool_definitions();
        assert_eq!(defs.len(), 18);
        let names: Vec<&str> = defs.iter().map(|d| d.name).collect();
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
            assert!(names.contains(&expected), "缺少工具: {}", expected);
        }
    }

    #[test]
    fn test_tool_names_are_unique() {
        let defs = get_tool_definitions();
        let set: std::collections::HashSet<&str> = defs.iter().map(|d| d.name).collect();
        assert_eq!(set.len(), defs.len(), "工具名必须唯一");
    }

    #[test]
    fn test_scopes_are_declared_and_known() {
        for d in get_tool_definitions() {
            assert!(
                matches!(
                    d.required_scope,
                    "memory:read" | "memory:write" | "memory:admin"
                ),
                "工具 {} 的作用域声明非法: {}",
                d.name,
                d.required_scope
            );
        }
    }

    #[test]
    fn test_read_only_tools_carry_read_or_admin_scope() {
        for d in get_tool_definitions() {
            if d.read_only {
                assert!(
                    matches!(d.required_scope, "memory:read" | "memory:admin"),
                    "只读工具 {} 必须申请 read 或 admin 作用域",
                    d.name
                );
            }
        }
    }

    #[test]
    fn test_destructive_tools_are_not_read_only() {
        for d in get_tool_definitions() {
            if d.destructive {
                assert!(!d.read_only, "破坏性工具 {} 不得标记为只读", d.name);
            }
        }
    }

    #[test]
    fn test_every_tool_has_json_object_schema() {
        for d in get_tool_definitions() {
            assert_eq!(d.input_schema["type"], json!("object"));
            assert!(d.input_schema["properties"].is_object());
            assert!(!d.description.is_empty());
        }
    }

    #[test]
    fn test_write_tools_require_namespace() {
        for d in get_tool_definitions() {
            if d.name.starts_with("memory_") && d.required_scope == "memory:write" {
                let required = d.input_schema["required"].as_array().unwrap();
                let has_ns = required.iter().any(|v| v == "namespace");
                // resolve 的命名空间来自提案本体，其余写工具必须显式带 namespace
                if d.name != "memory_integration_resolve" {
                    assert!(has_ns, "工具 {} 应要求 namespace", d.name);
                }
            }
        }
    }

    #[test]
    fn test_all_schemas_reject_unknown_fields() {
        for d in get_tool_definitions() {
            assert_eq!(
                d.input_schema["additionalProperties"],
                json!(false),
                "工具 {} 必须关闭 additionalProperties",
                d.name
            );
        }
    }
}
