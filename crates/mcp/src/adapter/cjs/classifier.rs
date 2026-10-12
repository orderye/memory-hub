//! 数据分类自萃取与法规水印 (AutoClassifier)
//!
//! 1. 启发式推导记忆类型 (Kind): Preference / Fact / Episode，并追加对应标签；
//! 2. 注入 AIGC 国家规范元数据水印 (AIGC_Source, AIGC_Compliant, AIGC_Timestamp)。

use super::fingerprint::ChuangjieshanFingerprint;
use super::CjsAdapterConfig;
use chrono::Utc;
use serde_json::{json, Value};

pub struct AutoClassifier;

impl AutoClassifier {
    /// 对传入参数进行分类推导与合规水印注入
    pub fn process(
        arguments: &mut Value,
        fp: &ChuangjieshanFingerprint,
        config: &CjsAdapterConfig,
    ) {
        if !fp.is_chuangjieshan {
            return;
        }

        // 仅处理对象类型参数
        let obj = match arguments.as_object_mut() {
            Some(o) => o,
            None => return,
        };

        // 1. 启发式 Kind 分类推导
        if config.auto_classify {
            Self::infer_kind_and_tags(obj);
        }

        // 2. AIGC 国家法规合规水印隐式注入
        if config.aigc_watermark {
            Self::inject_aigc_watermark(obj);
        }
    }

    /// 启发式规则：根据正文关键词识别人设偏好、世界设定或剧情事件
    fn infer_kind_and_tags(obj: &mut serde_json::Map<String, Value>) {
        // 兼容客户端传入 text 或 content
        if !obj.contains_key("content") {
            if let Some(t) = obj.get("text").cloned() {
                obj.insert("content".to_string(), t);
            }
        }

        let text = match obj.get("content").or_else(|| obj.get("text")).and_then(Value::as_str) {
            Some(t) => t,
            None => return,
        };

        // 如果客户端未显式指定 kind，或指定为泛化的 "note"
        let current_kind = obj
            .get("kind")
            .and_then(Value::as_str)
            .map(|s| s.to_ascii_lowercase());

        let needs_classification = match current_kind.as_deref() {
            None | Some("") | Some("note") => true,
            _ => false,
        };

        if !needs_classification {
            return;
        }

        const PREF_KEYWORDS: &[&str] = &[
            "喜欢", "厌恶", "习惯于", "人设立场", "讨厌", "偏好", "倾向于", "喜好", "嗜好",
        ];
        const FACT_KEYWORDS: &[&str] = &[
            "法则", "世界观", "地理位于", "不可违背", "设定", "定律", "地理", "禁制", "规则",
        ];
        const EPISODE_KEYWORDS: &[&str] = &[
            "发生了", "击败了", "启程前往", "遇见了", "击败", "经历了", "战斗", "抵达", "遭遇",
        ];

        let mut inferred_kind = None;
        let mut added_tag = None;

        if PREF_KEYWORDS.iter().any(|&k| text.contains(k)) {
            inferred_kind = Some("preference");
            added_tag = Some("persona_pref");
        } else if FACT_KEYWORDS.iter().any(|&k| text.contains(k)) {
            inferred_kind = Some("fact");
            added_tag = Some("world_rule");
        } else if EPISODE_KEYWORDS.iter().any(|&k| text.contains(k)) {
            inferred_kind = Some("episode");
            added_tag = Some("plot_event");
        }

        if let Some(kind_str) = inferred_kind {
            obj.insert("kind".to_string(), json!(kind_str));
        } else {
            // 创界山端侧写入未指定类型且无特定关键词时，兜底为 Fact（事实类记忆）
            obj.insert("kind".to_string(), json!("fact"));
        }

        if let Some(tag_str) = added_tag {
            let tags_entry = obj.entry("tags".to_string()).or_insert_with(|| json!([]));
            if let Some(tags_arr) = tags_entry.as_array_mut() {
                if !tags_arr.iter().any(|v| v.as_str() == Some(tag_str)) {
                    tags_arr.push(json!(tag_str));
                }
            }
        }
    }

    /// 注入 AIGC 国家规范元数据水印
    fn inject_aigc_watermark(obj: &mut serde_json::Map<String, Value>) {
        let metadata_val = obj
            .entry("metadata".to_string())
            .or_insert_with(|| json!({}));

        if let Some(metadata) = metadata_val.as_object_mut() {
            if !metadata.contains_key("AIGC_Source") && !metadata.contains_key("aigc_source") {
                metadata.insert("AIGC_Source".to_string(), json!("chuangjieshan"));
            }
            if !metadata.contains_key("AIGC_Compliant") && !metadata.contains_key("aigc_compliant") {
                metadata.insert("AIGC_Compliant".to_string(), json!(true));
            }
            if !metadata.contains_key("AIGC_Timestamp") && !metadata.contains_key("aigc_timestamp") {
                metadata.insert("AIGC_Timestamp".to_string(), json!(Utc::now().to_rfc3339()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_infer_preference() {
        let mut args = json!({
            "text": "这个角色极度讨厌甜食，习惯于在夜间行动"
        });
        let fp = ChuangjieshanFingerprint {
            is_chuangjieshan: true,
            ..Default::default()
        };
        let cfg = CjsAdapterConfig::default();
        AutoClassifier::process(&mut args, &fp, &cfg);

        assert_eq!(args.get("kind").and_then(Value::as_str), Some("preference"));
        let tags = args.get("tags").and_then(Value::as_array).unwrap();
        assert!(tags.iter().any(|t| t == "persona_pref"));
    }

    #[test]
    fn test_infer_fact() {
        let mut args = json!({
            "text": "创界山第七层的不可违背世界观法则：重力常数为负数"
        });
        let fp = ChuangjieshanFingerprint {
            is_chuangjieshan: true,
            ..Default::default()
        };
        let cfg = CjsAdapterConfig::default();
        AutoClassifier::process(&mut args, &fp, &cfg);

        assert_eq!(args.get("kind").and_then(Value::as_str), Some("fact"));
        let tags = args.get("tags").and_then(Value::as_array).unwrap();
        assert!(tags.iter().any(|t| t == "world_rule"));
    }

    #[test]
    fn test_inject_watermark() {
        let mut args = json!({
            "text": "普通内容"
        });
        let fp = ChuangjieshanFingerprint {
            is_chuangjieshan: true,
            ..Default::default()
        };
        let cfg = CjsAdapterConfig::default();
        AutoClassifier::process(&mut args, &fp, &cfg);

        let meta = args.get("metadata").and_then(Value::as_object).unwrap();
        assert_eq!(meta.get("AIGC_Source").and_then(Value::as_str), Some("chuangjieshan"));
        assert_eq!(meta.get("AIGC_Compliant").and_then(Value::as_bool), Some(true));
        assert!(meta.contains_key("AIGC_Timestamp"));
    }
}
