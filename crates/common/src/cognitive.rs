//! 认知抽取器（Chat / Working 提炼）的部署模式
//!
//! 生产二进制只允许在**显式开启**时装配抽取器：
//! - `disabled`（默认）：不装配 `ChatCognitiveService`，`cognitive_chat_propose`
//!   返回 `E_METHOD_NOT_FOUND` 并说明未启用，不写证据、不产出提案；
//! - `mock`：装配 `MockChatCognitiveExtractor`（占位实现，**仅限开发/联调**），
//!   它会照常写证据与认知提案，但不会理解对话内容。
//!
//! 真实 LLM 抽取器尚未接入；在它落地之前，默认关掉是唯一能保证
//! "不把占位产物写进正式数据"的结构性做法（仅靠 mock 默认返回空列表不可靠：
//! 那是实现细节，不是部署契约）。

/// 认知抽取器部署模式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CognitiveExtractorMode {
    /// 不装配抽取器（默认）：认知提炼入口明确不可用
    #[default]
    Disabled,
    /// 装配测试用占位抽取器（开发/联调专用）
    Mock,
}

impl CognitiveExtractorMode {
    /// 稳定字符串标识（启动日志与文档统一使用）
    pub fn as_str(&self) -> &'static str {
        match self {
            CognitiveExtractorMode::Disabled => "disabled",
            CognitiveExtractorMode::Mock => "mock",
        }
    }

    /// 解析模式名；未知值返回可读错误，**不做静默猜测**
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "disabled" | "off" | "" => Ok(CognitiveExtractorMode::Disabled),
            "mock" => Ok(CognitiveExtractorMode::Mock),
            other => Err(format!(
                "认知抽取器只支持 disabled | mock，当前为 {}",
                other
            )),
        }
    }

    /// 中文标签（启动日志用）
    pub fn label_zh(&self) -> &'static str {
        match self {
            CognitiveExtractorMode::Disabled => "未启用（不装配抽取器）",
            CognitiveExtractorMode::Mock => "占位抽取器（仅限开发）",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_disabled() {
        assert_eq!(
            CognitiveExtractorMode::default(),
            CognitiveExtractorMode::Disabled
        );
    }

    #[test]
    fn parses_known_values_case_insensitively() {
        assert_eq!(
            CognitiveExtractorMode::parse("DISABLED").unwrap(),
            CognitiveExtractorMode::Disabled
        );
        assert_eq!(
            CognitiveExtractorMode::parse(" mock ").unwrap(),
            CognitiveExtractorMode::Mock
        );
        assert_eq!(
            CognitiveExtractorMode::parse("").unwrap(),
            CognitiveExtractorMode::Disabled
        );
    }

    #[test]
    fn rejects_unknown_value() {
        let err = CognitiveExtractorMode::parse("llm").unwrap_err();
        assert!(err.contains("disabled | mock"), "{err}");
    }
}
