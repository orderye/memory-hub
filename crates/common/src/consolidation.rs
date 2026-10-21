//! 记忆整合模式（候选发现策略）的共享类型
//!
//! 整合中枢支持两种**候选发现**模式，二者共用同一套确定性决策层
//! （关系分类、冲突检查、提案治理、自动合并安全门槛）：
//!
//! - [`ConsolidationMode::Rules`]：纯本地规则整合。不请求 embedding、不查询向量相似度，
//!   只使用规范化指纹、结构化字段与本地词法索引发现候选。
//! - [`ConsolidationMode::Hybrid`]：向量辅助整合。在规则候选之外，额外用嵌入向量召回
//!   语义相近候选。向量只负责"找候选"，任何合并结论都必须由共享决策层给出。
//!
//! 本模块位于 `common` 而非 `mem-core`：`AppConfig`（定义在 `common::config`）
//! 需要持有这些类型，而依赖方向是 `mem-core → common`，故只能向下放置。

use serde::{Deserialize, Serialize};

/// 候选发现模式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsolidationMode {
    /// 纯本地规则整合（默认）：不请求 embedding、不查询向量相似度
    #[default]
    Rules,
    /// 向量辅助整合：本地规则候选 + 嵌入向量召回的语义候选
    Hybrid,
}

impl ConsolidationMode {
    /// 稳定的字符串标识（响应与日志统一使用）
    pub fn as_str(&self) -> &'static str {
        match self {
            ConsolidationMode::Rules => "rules",
            ConsolidationMode::Hybrid => "hybrid",
        }
    }

    /// 解析模式名；未知值返回可读错误，**不做静默猜测**
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "rules" => Ok(ConsolidationMode::Rules),
            "hybrid" => Ok(ConsolidationMode::Hybrid),
            other => Err(format!("整合模式只支持 rules | hybrid，当前为 {}", other)),
        }
    }

    /// 中文标签（启动日志与文档用）
    pub fn label_zh(&self) -> &'static str {
        match self {
            ConsolidationMode::Rules => "纯本地规则整合",
            ConsolidationMode::Hybrid => "向量辅助整合",
        }
    }
}

/// 向量模式不可用时的降级策略
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsolidationFallback {
    /// 降级为纯规则整合，并在响应中显式标记降级
    #[default]
    Rules,
    /// 直接返回可诊断错误，不执行任何整合
    Error,
}

impl ConsolidationFallback {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConsolidationFallback::Rules => "rules",
            ConsolidationFallback::Error => "error",
        }
    }

    /// 解析降级策略；未知值返回可读错误
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "rules" => Ok(ConsolidationFallback::Rules),
            "error" => Ok(ConsolidationFallback::Error),
            other => Err(format!(
                "整合降级策略只支持 rules | error，当前为 {}",
                other
            )),
        }
    }
}

/// 候选策略版本：随规则/阈值语义变化递增，用于可复现标注与审计
pub const CONSOLIDATION_STRATEGY_VERSION: &str = "consolidation-v1";

/// 一次整合运行的状态快照
///
/// 与检索的 `semantic.ready` 是**两回事**：前者描述"本次整合实际用了什么候选策略"，
/// 后者描述"检索链路是否具备真实语义能力"。整合在纯规则模式下正常运行，
/// 不代表也不依赖检索的语义能力。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsolidationModeStatus {
    /// 调用方请求的模式（`rules` | `hybrid`）
    pub requested_mode: String,
    /// 实际执行的模式（`rules` | `hybrid`）
    pub effective_mode: String,
    /// 候选策略标识（`local_rules` | `local_rules+embedding`）
    pub candidate_strategy: String,
    /// 本次运行是否使用了嵌入向量
    pub embedding_used: bool,
    /// 本次运行对嵌入提供方的调用次数
    pub embedding_calls: usize,
    /// 本次运行新计算的向量条目数
    pub embedded_entries: usize,
    /// 是否发生降级
    pub fallback_applied: bool,
    /// 降级原因（未降级时为空）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
    /// 候选策略版本
    pub strategy_version: String,
    /// 使用的相似度阈值是否经过真实模型标定
    pub similarity_threshold_calibrated: bool,
}

impl Default for ConsolidationModeStatus {
    fn default() -> Self {
        Self {
            requested_mode: ConsolidationMode::Rules.as_str().to_string(),
            effective_mode: ConsolidationMode::Rules.as_str().to_string(),
            candidate_strategy: "local_rules".to_string(),
            embedding_used: false,
            embedding_calls: 0,
            embedded_entries: 0,
            fallback_applied: false,
            fallback_reason: None,
            strategy_version: CONSOLIDATION_STRATEGY_VERSION.to_string(),
            similarity_threshold_calibrated: false,
        }
    }
}

impl ConsolidationModeStatus {
    /// 纯规则运行的状态
    pub fn rules() -> Self {
        Self::default()
    }

    /// 向量模式成功运行的状态
    pub fn hybrid(embedding_calls: usize, embedded_entries: usize) -> Self {
        Self {
            requested_mode: ConsolidationMode::Hybrid.as_str().to_string(),
            effective_mode: ConsolidationMode::Hybrid.as_str().to_string(),
            candidate_strategy: "local_rules+embedding".to_string(),
            embedding_used: true,
            embedding_calls,
            embedded_entries,
            fallback_applied: false,
            fallback_reason: None,
            strategy_version: CONSOLIDATION_STRATEGY_VERSION.to_string(),
            similarity_threshold_calibrated: true,
        }
    }

    /// 向量模式降级为规则模式的状态
    ///
    /// `requested_mode` 保持 `hybrid`——降级必须**显式可见**，
    /// 不得把降级结果伪装成成功的向量整合。
    pub fn fallback_to_rules(reason: &str) -> Self {
        Self {
            requested_mode: ConsolidationMode::Hybrid.as_str().to_string(),
            effective_mode: ConsolidationMode::Rules.as_str().to_string(),
            candidate_strategy: "local_rules".to_string(),
            embedding_used: false,
            embedding_calls: 0,
            embedded_entries: 0,
            fallback_applied: true,
            fallback_reason: Some(reason.to_string()),
            strategy_version: CONSOLIDATION_STRATEGY_VERSION.to_string(),
            similarity_threshold_calibrated: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_mode_is_rules() {
        assert_eq!(ConsolidationMode::default(), ConsolidationMode::Rules);
    }

    #[test]
    fn test_parse_rejects_unknown_mode() {
        assert!(ConsolidationMode::parse("hybrid").is_ok());
        assert!(ConsolidationMode::parse("RULES").is_ok());
        let err = ConsolidationMode::parse("semantic").unwrap_err();
        assert!(err.contains("rules | hybrid"), "错误信息须可读：{err}");
    }

    #[test]
    fn test_fallback_status_keeps_requested_mode() {
        let s = ConsolidationModeStatus::fallback_to_rules("embedding_provider_unavailable");
        assert_eq!(s.requested_mode, "hybrid");
        assert_eq!(s.effective_mode, "rules");
        assert!(s.fallback_applied);
        assert!(!s.embedding_used);
        assert_eq!(
            s.fallback_reason.as_deref(),
            Some("embedding_provider_unavailable")
        );
    }
}
