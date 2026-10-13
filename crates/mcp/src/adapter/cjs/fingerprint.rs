//! 创界山客户端特征指纹探测器 (FingerprintDetector)
//!
//! 负责从 HTTP Header、JSON-RPC 请求与工具参数中交叉识别创界山端侧特征，
//! 提取世界 ID (world_id)、角色 ID (bot_id)、会话 ID (conversation_id) 与客户端版本。

use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 创界山客户端特征指纹
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChuangjieshanFingerprint {
    /// 是否为创界山生态客户端
    pub is_chuangjieshan: bool,
    /// 客户端版本号（如 1.0.4）
    pub app_version: Option<String>,
    /// 当前世界/沙盒 ID
    pub world_id: Option<String>,
    /// 当前智能体/Bot ID
    pub bot_id: Option<String>,
    /// 当前会话 ID
    pub conversation_id: Option<String>,
}

pub struct FingerprintDetector;

impl FingerprintDetector {
    /// 交叉多层探测创界山特征
    pub fn detect(
        headers: Option<&HeaderMap>,
        arguments: Option<&Value>,
        default_world: &str,
    ) -> ChuangjieshanFingerprint {
        let mut fp = ChuangjieshanFingerprint::default();

        // 1. Header 探测层
        if let Some(headers) = headers {
            if let Some(val) = headers.get("x-client-source").and_then(|v| v.to_str().ok()) {
                if val.trim().eq_ignore_ascii_case("chuangjieshan") {
                    fp.is_chuangjieshan = true;
                }
            }

            if let Some(val) = headers.get("x-cjs-world-id").and_then(|v| v.to_str().ok()) {
                let s = val.trim();
                if !s.is_empty() {
                    fp.is_chuangjieshan = true;
                    fp.world_id = Some(s.to_string());
                }
            }

            if let Some(val) = headers.get("x-cjs-bot-id").and_then(|v| v.to_str().ok()) {
                let s = val.trim();
                if !s.is_empty() {
                    fp.is_chuangjieshan = true;
                    fp.bot_id = Some(s.to_string());
                }
            }

            if let Some(val) = headers
                .get("x-cjs-conversation-id")
                .or_else(|| headers.get("x-cjs-conv-id"))
                .and_then(|v| v.to_str().ok())
            {
                let s = val.trim();
                if !s.is_empty() {
                    fp.is_chuangjieshan = true;
                    fp.conversation_id = Some(s.to_string());
                }
            }

            if let Some(val) = headers
                .get("x-cjs-app-version")
                .and_then(|v| v.to_str().ok())
            {
                let s = val.trim();
                if !s.is_empty() {
                    fp.app_version = Some(s.to_string());
                }
            }
        }

        // 2. 参数与元数据探测层
        if let Some(args) = arguments {
            // AIGC 元数据水印探测
            if let Some(metadata) = args.get("metadata").and_then(Value::as_object) {
                if let Some(src) = metadata
                    .get("AIGC_Source")
                    .or_else(|| metadata.get("aigc_source"))
                    .and_then(Value::as_str)
                {
                    if src.trim().eq_ignore_ascii_case("chuangjieshan") {
                        fp.is_chuangjieshan = true;
                    }
                }

                if fp.world_id.is_none() {
                    if let Some(w) = metadata
                        .get("world_id")
                        .or_else(|| metadata.get("worldId"))
                        .and_then(Value::as_str)
                    {
                        fp.world_id = Some(w.trim().to_string());
                    }
                }

                if fp.bot_id.is_none() {
                    if let Some(b) = metadata
                        .get("bot_id")
                        .or_else(|| metadata.get("botId"))
                        .and_then(Value::as_str)
                    {
                        fp.bot_id = Some(b.trim().to_string());
                    }
                }

                if fp.conversation_id.is_none() {
                    if let Some(c) = metadata
                        .get("conversation_id")
                        .or_else(|| metadata.get("conversationId"))
                        .or_else(|| metadata.get("conv_id"))
                        .and_then(Value::as_str)
                    {
                        fp.conversation_id = Some(c.trim().to_string());
                    }
                }
            }

            // 直接参数字段探测
            if let Some(src) = args.get("client_source").and_then(Value::as_str) {
                if src.trim().eq_ignore_ascii_case("chuangjieshan") {
                    fp.is_chuangjieshan = true;
                }
            }

            if let Some(client_info) = args.get("clientInfo").and_then(Value::as_object) {
                if let Some(name) = client_info.get("name").and_then(Value::as_str) {
                    if name.trim().eq_ignore_ascii_case("chuangjieshan") {
                        fp.is_chuangjieshan = true;
                    }
                }
                if fp.app_version.is_none() {
                    if let Some(ver) = client_info.get("version").and_then(Value::as_str) {
                        fp.app_version = Some(ver.trim().to_string());
                    }
                }
            }

            if fp.world_id.is_none() {
                if let Some(w) = args.get("world_id").and_then(Value::as_str) {
                    fp.world_id = Some(w.trim().to_string());
                }
            }
            if fp.bot_id.is_none() {
                if let Some(b) = args.get("bot_id").and_then(Value::as_str) {
                    fp.bot_id = Some(b.trim().to_string());
                }
            }
            if fp.conversation_id.is_none() {
                if let Some(c) = args
                    .get("conversation_id")
                    .or_else(|| args.get("conv_id"))
                    .and_then(Value::as_str)
                {
                    fp.conversation_id = Some(c.trim().to_string());
                }
            }
        }

        // 若确认是创界山请求但未传 world_id，填充默认世界 ID
        if fp.is_chuangjieshan && fp.world_id.is_none() {
            fp.world_id = Some(default_world.to_string());
        }

        fp
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use serde_json::json;

    #[test]
    fn test_normal_client_not_detected() {
        let headers = HeaderMap::new();
        let args = json!({
            "namespace": "work",
            "text": "hello world"
        });
        let fp = FingerprintDetector::detect(Some(&headers), Some(&args), "default");
        assert!(!fp.is_chuangjieshan);
        assert!(fp.world_id.is_none());
        assert!(fp.bot_id.is_none());
    }

    #[test]
    fn test_detect_via_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("X-Client-Source", HeaderValue::from_static("chuangjieshan"));
        headers.insert("X-CJS-World-Id", HeaderValue::from_static("world_alpha"));
        headers.insert("X-CJS-Bot-Id", HeaderValue::from_static("bot_9527"));

        let fp = FingerprintDetector::detect(Some(&headers), None, "default");
        assert!(fp.is_chuangjieshan);
        assert_eq!(fp.world_id.as_deref(), Some("world_alpha"));
        assert_eq!(fp.bot_id.as_deref(), Some("bot_9527"));
    }

    #[test]
    fn test_detect_via_metadata() {
        let args = json!({
            "text": "test item",
            "metadata": {
                "AIGC_Source": "chuangjieshan",
                "world_id": "world_beta"
            }
        });
        let fp = FingerprintDetector::detect(None, Some(&args), "default");
        assert!(fp.is_chuangjieshan);
        assert_eq!(fp.world_id.as_deref(), Some("world_beta"));
    }

    #[test]
    fn test_fallback_default_world() {
        let mut headers = HeaderMap::new();
        headers.insert("X-Client-Source", HeaderValue::from_static("chuangjieshan"));

        let fp = FingerprintDetector::detect(Some(&headers), None, "custom_default");
        assert!(fp.is_chuangjieshan);
        assert_eq!(fp.world_id.as_deref(), Some("custom_default"));
    }
}
