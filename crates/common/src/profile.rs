//! 部署模式（Personal Lite / Production Full）
//!
//! 两种部署形态**共用同一套核心实现**：模式只负责给出不同的**默认值**，
//! 不复制业务逻辑、不引入第二套数据模型、也不取代任何授权或安全配置。
//!
//! - [`DeploymentProfile::Production`]（默认）：现有三进程部署形态，行为与升级前完全一致。
//! - [`DeploymentProfile::Lite`]：单机个人部署，默认关闭嵌入调用（纯关键词检索）、
//!   默认不跑周期性后台作业，其余能力（PostgreSQL/pgvector、OAuth 2.1、ACL、
//!   备份恢复、认知记忆、全部 MCP 工具）一律保留。
//!
//! **默认值永远是 Production**：运维升级后不改变既有实例的行为，是"轻量化"
//! 能够安全落地的前提；Lite 必须由部署者显式选择。
//!
//! 语义边界（实现上必须遵守）：
//! 1. 模式只在 `AppConfig::from_env` 里影响默认值，**显式环境变量永远优先**；
//! 2. 不得让模式改写授权、令牌、日志脱敏等安全项；
//! 3. 不得让两种模式产生互不兼容的 schema 或备份格式。

use serde::{Deserialize, Serialize};

/// 部署模式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentProfile {
    /// 完整生产部署（默认）：保留全部后台作业与向量检索能力
    #[default]
    Production,
    /// 个人轻量部署：默认零嵌入调用、无周期性后台作业
    Lite,
}

impl DeploymentProfile {
    /// 稳定的字符串标识（响应、日志与文档统一使用）
    pub fn as_str(&self) -> &'static str {
        match self {
            DeploymentProfile::Production => "production",
            DeploymentProfile::Lite => "lite",
        }
    }

    /// 解析模式名；未知值返回可读错误，**不做静默猜测**
    ///
    /// 静默猜测一个拼错的模式名，等于让"我以为开了 Lite"变成"其实还在跑
    /// 周期性向量扫描"，因此必须先失败再启动。
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "production" | "prod" => Ok(DeploymentProfile::Production),
            "lite" | "personal" => Ok(DeploymentProfile::Lite),
            other => Err(format!("部署模式只支持 production | lite，当前为 {other}")),
        }
    }

    /// 是否为个人轻量部署
    pub fn is_lite(&self) -> bool {
        matches!(self, DeploymentProfile::Lite)
    }

    /// 中文标签（启动日志与文档用）
    pub fn label_zh(&self) -> &'static str {
        match self {
            DeploymentProfile::Production => "完整生产部署",
            DeploymentProfile::Lite => "个人轻量部署",
        }
    }
}

/// 环境变量名（单一事实来源，避免各进程各写一份字面量）
pub const DEPLOYMENT_PROFILE_ENV: &str = "HUB_DEPLOYMENT_PROFILE";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_is_production() {
        assert_eq!(DeploymentProfile::default(), DeploymentProfile::Production);
        assert!(!DeploymentProfile::default().is_lite());
    }

    #[test]
    fn test_parse_accepts_aliases_but_rejects_unknown() {
        assert_eq!(
            DeploymentProfile::parse("LITE"),
            Ok(DeploymentProfile::Lite)
        );
        assert_eq!(
            DeploymentProfile::parse(" personal "),
            Ok(DeploymentProfile::Lite)
        );
        assert_eq!(
            DeploymentProfile::parse("production"),
            Ok(DeploymentProfile::Production)
        );
        let err = DeploymentProfile::parse("small").unwrap_err();
        assert!(err.contains("production | lite"), "错误信息须可读：{err}");
    }

    #[test]
    fn test_as_str_roundtrip() {
        for p in [DeploymentProfile::Production, DeploymentProfile::Lite] {
            assert_eq!(DeploymentProfile::parse(p.as_str()), Ok(p));
        }
    }
}
