//! MCP 工具 JSON Schema 规范定义（对应设计规范 17.2 节）

use serde_json::{json, Value};

/// 工具元数据与模式描述
pub struct ToolDefinition {
    pub name: &'static str,
    pub description: &'static str,
    pub read_only: bool,
    pub destructive: bool,
    pub idempotent: bool,
    pub required_scope: &'static str,
    pub input_schema: Value,
}

pub fn get_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "hub_context",
            description: "生成回答之前调用：装配本轮可用的用户记忆包、行为规范包、量表与指引",
            read_only: true,
            destructive: false,
            idempotent: true,
            required_scope: "memory:context",
            input_schema: json!({
                "type": "object",
                "required": ["turn_id"],
                "additionalProperties": false,
                "properties": {
                    "conversation_id": { "type": "string", "maxLength": 128 },
                    "turn_id": { "type": "string", "maxLength": 128 },
                    "scene_tags": { "type": "array", "maxItems": 16, "items": { "type": "string", "maxLength": 64 } },
                    "roles": { "type": "array", "maxItems": 8, "items": { "type": "string", "maxLength": 64 } },
                    "interaction_context": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "modality": { "enum": ["text", "voice"] },
                            "shared_device": { "type": "boolean" },
                            "speaker_verified": { "type": "boolean" },
                            "locale": { "type": "string" }
                        }
                    },
                    "user_flags": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": { "minor_mode": { "type": "boolean" } }
                    },
                    "client_hints": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "token_budget": { "type": "integer", "minimum": 64, "maximum": 4096, "default": 600 },
                            "latency_budget_ms": { "type": "integer", "minimum": 50, "maximum": 5000, "default": 300 }
                        }
                    }
                }
            }),
        },
        ToolDefinition {
            name: "hub_observe",
            description: "回传本轮提取的心智假设、预测反应与用户行为事件",
            read_only: false,
            destructive: false,
            idempotent: true,
            required_scope: "memory:write",
            input_schema: json!({
                "type": "object",
                "required": ["turn_id"],
                "additionalProperties": false,
                "properties": {
                    "conversation_id": { "type": "string", "maxLength": 128 },
                    "turn_id": { "type": "string", "maxLength": 128 },
                    "hypotheses": {
                        "type": "array",
                        "maxItems": 8,
                        "items": {
                            "type": "object",
                            "required": ["kind", "content", "confidence"],
                            "properties": {
                                "kind": { "enum": ["belief","desire","intention","emotion","thought","relation_frame","behavior"] },
                                "content": { "type": "string", "maxLength": 300 },
                                "confidence": { "type": "number", "minimum": 0, "maximum": 1 },
                                "evidence": { "type": "array", "maxItems": 4, "items": { "type": "string", "maxLength": 200 } }
                            }
                        }
                    },
                    "predicted_reaction": {
                        "type": "object",
                        "properties": {
                            "kind": { "enum": ["adopt", "reject", "edit"] },
                            "confidence": { "type": "number", "minimum": 0, "maximum": 1 }
                        }
                    },
                    "events": {
                        "type": "array",
                        "maxItems": 16,
                        "items": {
                            "type": "object",
                            "required": ["type"],
                            "properties": {
                                "type": { "enum": ["adopt", "reject", "edit", "regenerate", "follow_up"] },
                                "ts": { "type": "string" }
                            }
                        }
                    }
                }
            }),
        },
        ToolDefinition {
            name: "hub_check",
            description: "回传助理自评与规则命中结果，可选校验草稿",
            read_only: false,
            destructive: false,
            idempotent: true,
            required_scope: "memory:write",
            input_schema: json!({
                "type": "object",
                "required": ["turn_id", "self_eval"],
                "additionalProperties": false,
                "properties": {
                    "turn_id": { "type": "string", "maxLength": 128 },
                    "self_eval": {
                        "type": "object",
                        "required": ["gates", "scores"],
                        "properties": {
                            "gates": { "type": "object" },
                            "scores": { "type": "object" },
                            "revisions": { "type": "integer", "minimum": 0, "maximum": 3 }
                        }
                    },
                    "rule_hits": {
                        "type": "array",
                        "maxItems": 32,
                        "items": {
                            "type": "object",
                            "required": ["rule_id", "hit"],
                            "properties": {
                                "rule_id": { "type": "string" },
                                "hit": { "type": "boolean" }
                            }
                        }
                    },
                    "draft": { "type": "string", "maxLength": 4000 }
                }
            }),
        },
        ToolDefinition {
            name: "hub_feedback",
            description: "回传用户显式反馈（采纳、拒绝、纠错、申诉）",
            read_only: false,
            destructive: false,
            idempotent: true,
            required_scope: "memory:write",
            input_schema: json!({
                "type": "object",
                "required": ["turn_id", "type"],
                "additionalProperties": false,
                "properties": {
                    "turn_id": { "type": "string", "maxLength": 128 },
                    "type": { "enum": ["adopt", "reject", "edit", "regenerate", "complaint", "uncovered_conflict"] },
                    "target": { "type": "string" },
                    "detail": { "type": "string", "maxLength": 1000 }
                }
            }),
        },
        ToolDefinition {
            name: "hub_memory_list",
            description: "查看用户个人记忆列表",
            read_only: true,
            destructive: false,
            idempotent: true,
            required_scope: "memory:manage",
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "filter": { "type": "object" },
                    "cursor": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "default": 50 }
                }
            }),
        },
        ToolDefinition {
            name: "hub_memory_delete",
            description: "删除指定记忆条目或某一层（需确认令牌）",
            read_only: false,
            destructive: true,
            idempotent: false,
            required_scope: "memory:manage",
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "id": { "type": "string" },
                    "scope": { "type": "object" },
                    "confirm_token": { "type": "string" }
                }
            }),
        },
        ToolDefinition {
            name: "hub_memory_pause",
            description: "暂停或恢复个人记忆的使用与采集",
            read_only: false,
            destructive: false,
            idempotent: true,
            required_scope: "memory:manage",
            input_schema: json!({
                "type": "object",
                "required": ["on"],
                "additionalProperties": false,
                "properties": {
                    "on": { "type": "boolean" },
                    "scope": { "enum": ["collect", "use", "both"] }
                }
            }),
        },
    ]
}
