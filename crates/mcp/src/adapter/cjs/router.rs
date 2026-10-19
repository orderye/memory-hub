//! 智能命名空间路由与虚拟双路召回 (SmartNamespaceRouter)
//!
//! 1. 命名空间三级映射：
//!    - "shared" / 缺省 => w_{world_id}_shared
//!    - "bot" => w_{world_id}_bot_{bot_id}
//!    - "conv" => w_{world_id}_conv_{conv_id}
//! 2. 虚拟双路并发检索 (memory_search)：
//!    端侧角色提问时，并发 (tokio::join!) 检索世界公共空间与 Bot 私有空间，
//!    在内存中完成去重与相关性排序截断，返回统一的标准 ToolCallResult。

use super::fingerprint::ChuangjieshanFingerprint;

/// 跨命名空间内容近重复的 Jaccard 阈值
const CJS_CONTENT_DUP_THRESHOLD: f32 = 0.85;
use crate::tools::ToolCallResult;
use crate::ToolDispatcher;
use common::{AuthCtx, Result};
use serde_json::{json, Value};
use std::collections::HashSet;

pub struct SmartNamespaceRouter;

impl SmartNamespaceRouter {
    /// 拦截并处理工具路由与多路召回
    pub async fn intercept_or_route(
        dispatcher: &ToolDispatcher,
        auth: &AuthCtx,
        name: &str,
        arguments: &mut Value,
        fp: &ChuangjieshanFingerprint,
    ) -> Result<Option<ToolCallResult>> {
        if !fp.is_chuangjieshan {
            return Ok(None);
        }

        let world_id = fp.world_id.as_deref().unwrap_or("default");

        if name == "memory_search" {
            // 判断是否需要触发虚拟双路召回
            if let Some(bot_id) = &fp.bot_id {
                let ns_opt = arguments
                    .get("namespace")
                    .and_then(Value::as_str)
                    .map(str::trim);

                let is_explicit_single = match ns_opt {
                    Some(s) if s.starts_with("w_") => true,
                    Some("shared") => true,
                    Some("bot") => true,
                    Some("conv") => true,
                    _ => false,
                };

                // 若未指定单一物理命名空间，则扇出为双路检索：世界公共记忆 + Bot 私有记忆
                if !is_explicit_single {
                    let shared_ns = format!("w_{}_shared", world_id);
                    let bot_ns = format!("w_{}_bot_{}", world_id, bot_id);
                    return Self::dual_search(dispatcher, auth, arguments, &shared_ns, &bot_ns)
                        .await;
                }
            }
        }

        // 单路常规路由改写
        Self::rewrite_namespace(arguments, fp);
        Ok(None)
    }

    /// 双路并发检索与结果重排去重
    async fn dual_search(
        dispatcher: &ToolDispatcher,
        auth: &AuthCtx,
        arguments: &Value,
        shared_ns: &str,
        bot_ns: &str,
    ) -> Result<Option<ToolCallResult>> {
        let mut args_shared = arguments.clone();
        let mut args_bot = arguments.clone();

        if let Some(obj) = args_shared.as_object_mut() {
            obj.insert("namespace".to_string(), json!(shared_ns));
        }
        if let Some(obj) = args_bot.as_object_mut() {
            obj.insert("namespace".to_string(), json!(bot_ns));
        }

        // tokio::join! 并发执行双路检索
        let (res_shared, res_bot) = tokio::join!(
            dispatcher.dispatch(auth, "memory_search", args_shared),
            dispatcher.dispatch(auth, "memory_search", args_bot)
        );

        // 如果两路均报错，若是因为命名空间未初始化（空账户/新角色），优雅返回空结果列表
        if res_shared.is_error && res_bot.is_error {
            return Ok(Some(ToolCallResult::success(json!({ "results": [] }))));
        }

        let mut items = Vec::new();

        // 提取结果并合并
        Self::collect_results(&res_shared, &mut items);
        Self::collect_results(&res_bot, &mut items);

        // 内存中按 memory.id 或 key 去重
        let mut seen = HashSet::new();
        let mut deduped = Vec::with_capacity(items.len());

        for item in items {
            let unique_key = item
                .get("memory")
                .and_then(|m| {
                    m.get("id")
                        .and_then(Value::as_str)
                        .or_else(|| m.get("key").and_then(Value::as_str))
                })
                .map(str::to_string);

            if let Some(k) = unique_key {
                if seen.insert(k) {
                    deduped.push(item);
                }
            } else {
                deduped.push(item);
            }
        }

        // 按 score 降序排序
        deduped.sort_by(|a, b| {
            let sa = a.get("score").and_then(Value::as_f64).unwrap_or(0.0);
            let sb = b.get("score").and_then(Value::as_f64).unwrap_or(0.0);
            sb.total_cmp(&sa)
        });

        // 跨命名空间内容去重：shared 与 bot 私有记忆里的近重复内容此前会同时保留
        // （id/key 不同但语义相同）。用与检索同源的分词词元 Jaccard 近似判重，
        // 保留分数更高的那条；这与设计文档"轻量级去重 + 综合 score 裁剪"的表述一致。
        deduped = Self::dedup_by_content_similarity(deduped, CJS_CONTENT_DUP_THRESHOLD);

        // 截断 top_k
        let top_k = arguments
            .get("top_k")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(10);

        if deduped.len() > top_k {
            deduped.truncate(top_k);
        }

        Ok(Some(ToolCallResult::success(json!({ "results": deduped }))))
    }

    /// 内容近重复去重：按当前顺序（分数降序）保留代表条目
    ///
    /// 判定用 `mem_core` 的分词词元 Jaccard 相似度（CJK bigram + ASCII 词），
    /// 阈值取 0.85：同一条记忆在两个命名空间的副本、或措辞微调的重复内容会被合并。
    fn dedup_by_content_similarity(items: Vec<Value>, threshold: f32) -> Vec<Value> {
        use std::collections::HashSet;

        let tokens_of = |item: &Value| -> HashSet<String> {
            item.get("memory")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_str)
                .map(|content| mem_core::fts_doc_tokens(content).into_iter().collect())
                .unwrap_or_default()
        };
        let jaccard = |a: &HashSet<String>, b: &HashSet<String>| -> f32 {
            if a.is_empty() || b.is_empty() {
                return 0.0;
            }
            let inter = a.intersection(b).count() as f32;
            let union = a.union(b).count() as f32;
            if union == 0.0 {
                0.0
            } else {
                inter / union
            }
        };

        let mut kept: Vec<Value> = Vec::with_capacity(items.len());
        let mut kept_tokens: Vec<HashSet<String>> = Vec::with_capacity(items.len());
        for item in items {
            let tokens = tokens_of(&item);
            let duplicate = kept_tokens
                .iter()
                .any(|existing| jaccard(existing, &tokens) >= threshold);
            if !duplicate {
                kept_tokens.push(tokens);
                kept.push(item);
            }
        }
        kept
    }

    /// 从 ToolCallResult 提取 results 列表
    fn collect_results(res: &ToolCallResult, out: &mut Vec<Value>) {
        if res.is_error {
            return;
        }

        if let Some(sc) = &res.structured_content {
            if let Some(arr) = sc.get("results").and_then(Value::as_array) {
                out.extend(arr.iter().cloned());
            }
        }
    }

    /// 改写参数中的命名空间
    pub fn rewrite_namespace(arguments: &mut Value, fp: &ChuangjieshanFingerprint) {
        let obj = match arguments.as_object_mut() {
            Some(o) => o,
            None => return,
        };

        let world_id = fp.world_id.as_deref().unwrap_or("default");

        let current_ns = obj.get("namespace").and_then(Value::as_str).map(str::trim);

        let mapped = match current_ns {
            Some("*") => "*".to_string(),
            Some(s) if s.starts_with("w_") => s.to_string(),
            Some("bot") => {
                if let Some(bot) = &fp.bot_id {
                    format!("w_{}_bot_{}", world_id, bot)
                } else {
                    format!("w_{}_shared", world_id)
                }
            }
            Some("conv") => {
                if let Some(conv) = &fp.conversation_id {
                    format!("w_{}_conv_{}", world_id, conv)
                } else if let Some(bot) = &fp.bot_id {
                    format!("w_{}_bot_{}", world_id, bot)
                } else {
                    format!("w_{}_shared", world_id)
                }
            }
            Some("shared") | Some("") => format!("w_{}_shared", world_id),
            None => {
                // 仅当包含 content/text 时补齐默认世界共享空间（如写入场景）
                if obj.contains_key("content") || obj.contains_key("text") {
                    format!("w_{}_shared", world_id)
                } else {
                    return;
                }
            }
            Some(other) => format!("w_{}_{}", world_id, other),
        };

        obj.insert("namespace".to_string(), json!(mapped));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_rewrite_namespace_default_shared() {
        let mut args = json!({
            "text": "test"
        });
        let fp = ChuangjieshanFingerprint {
            is_chuangjieshan: true,
            world_id: Some("earth_1".to_string()),
            bot_id: Some("bot_1".to_string()),
            ..Default::default()
        };

        SmartNamespaceRouter::rewrite_namespace(&mut args, &fp);
        assert_eq!(
            args.get("namespace").and_then(Value::as_str),
            Some("w_earth_1_shared")
        );
    }

    #[test]
    fn test_rewrite_namespace_bot() {
        let mut args = json!({
            "namespace": "bot",
            "text": "test"
        });
        let fp = ChuangjieshanFingerprint {
            is_chuangjieshan: true,
            world_id: Some("earth_1".to_string()),
            bot_id: Some("hero".to_string()),
            ..Default::default()
        };

        SmartNamespaceRouter::rewrite_namespace(&mut args, &fp);
        assert_eq!(
            args.get("namespace").and_then(Value::as_str),
            Some("w_earth_1_bot_hero")
        );
    }

    #[test]
    fn test_rewrite_namespace_preserves_w_prefix() {
        let mut args = json!({
            "namespace": "w_custom_ns",
            "text": "test"
        });
        let fp = ChuangjieshanFingerprint {
            is_chuangjieshan: true,
            world_id: Some("earth_1".to_string()),
            ..Default::default()
        };

        SmartNamespaceRouter::rewrite_namespace(&mut args, &fp);
        assert_eq!(
            args.get("namespace").and_then(Value::as_str),
            Some("w_custom_ns")
        );
    }
}
