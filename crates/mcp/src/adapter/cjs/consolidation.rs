//! 角色扮演视角防冲突策略 (RoleplayConsolidationPolicy)
//!
//! 在多智能体角色扮演场景中，不同 Bot 对同一事件可能持有完全对立的人设观点（主观偏好与立场），
//! 这类分歧属于戏剧张力与人设特色，绝不可被通用整合引擎强行合并为一个中立平庸的事实。
//!
//! 防护机制：
//! - 拦截 `memory_consolidate`：若命名空间属于 Bot 私有空间 (`w_*_bot_*`) 或非公共空间，
//!   强制关闭 `auto_merge`（即便端侧误传了 `auto_merge: true`），防止人设被自动化融合破坏；
//! - 仅允许在世界公共空间 (`w_*_shared`) 且基于客观事实 (`Fact`) 时执行自动合并。

use super::fingerprint::ChuangjieshanFingerprint;
use crate::tools::ToolCallResult;
use common::Result;
use serde_json::Value;

pub struct RoleplayConsolidationPolicy;

impl RoleplayConsolidationPolicy {
    /// 拦截并修正整合参数或判定冲突策略
    pub fn intercept(
        name: &str,
        arguments: &mut Value,
        fp: &ChuangjieshanFingerprint,
    ) -> Result<Option<ToolCallResult>> {
        if !fp.is_chuangjieshan {
            return Ok(None);
        }

        if name == "memory_consolidate" {
            if let Some(obj) = arguments.as_object_mut() {
                let ns = obj
                    .get("namespace")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();

                let is_bot_private = ns.contains("_bot_") || ns == "bot" || fp.bot_id.is_some();
                let is_shared = ns.ends_with("_shared") || ns == "shared";

                // 如果是 Bot 私有记忆空间，或非明确公共空间，严禁自动合并
                if is_bot_private || !is_shared {
                    if let Some(auto_merge) = obj.get_mut("auto_merge") {
                        if auto_merge.as_bool() == Some(true) {
                            tracing::info!(
                                namespace = %ns,
                                "创界山角色扮演防护：拦截并禁用私有/角色空间的 auto_merge，保留主观人设独立视角"
                            );
                            *auto_merge = serde_json::json!(false);
                        }
                    }
                }
            }
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_prevent_auto_merge_in_bot_namespace() {
        let mut args = json!({
            "namespace": "w_world1_bot_botA",
            "auto_merge": true
        });
        let fp = ChuangjieshanFingerprint {
            is_chuangjieshan: true,
            bot_id: Some("botA".to_string()),
            ..Default::default()
        };

        RoleplayConsolidationPolicy::intercept("memory_consolidate", &mut args, &fp).unwrap();
        assert_eq!(args.get("auto_merge").and_then(Value::as_bool), Some(false));
    }

    #[test]
    fn test_allow_auto_merge_in_shared_namespace() {
        let mut args = json!({
            "namespace": "w_world1_shared",
            "auto_merge": true
        });
        let fp = ChuangjieshanFingerprint {
            is_chuangjieshan: true,
            bot_id: None,
            ..Default::default()
        };

        RoleplayConsolidationPolicy::intercept("memory_consolidate", &mut args, &fp).unwrap();
        assert_eq!(args.get("auto_merge").and_then(Value::as_bool), Some(true));
    }
}
