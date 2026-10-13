//! 多记忆融合器：正文、标签与元数据深度融合

use async_trait::async_trait;
use common::Result;
use mem_core::MemoryEntry;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// 融合策略
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeStrategy {
    /// 仅保留最新幸存者内容（原样保留）
    KeepLatest,
    /// 片段去重拼接（保留所有非冗余信息）
    ConcatenateUnique,
}

/// 融合输出结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergedOutput {
    pub content: String,
    pub tags: Vec<String>,
    pub metadata: serde_json::Value,
    pub confidence: f32,
    pub strategy: MergeStrategy,
}

/// 内容融合器契约
#[async_trait]
pub trait ContentMerger: Send + Sync {
    async fn merge(&self, survivor: &MemoryEntry, members: &[MemoryEntry]) -> Result<MergedOutput>;
}

/// 分层内容融合器
#[derive(Debug, Clone, Default)]
pub struct TieredMerger {
    pub default_strategy: Option<MergeStrategy>,
}

impl TieredMerger {
    pub fn new() -> Self {
        Self {
            default_strategy: None,
        }
    }

    pub fn with_strategy(strategy: MergeStrategy) -> Self {
        Self {
            default_strategy: Some(strategy),
        }
    }

    /// 标签集合合并（保序去重）
    pub fn merge_tags(survivor: &MemoryEntry, members: &[MemoryEntry]) -> Vec<String> {
        let mut tags = survivor.tags.clone();
        for m in members {
            for t in &m.tags {
                if !tags.contains(t) {
                    tags.push(t.clone());
                }
            }
        }
        tags
    }

    /// 元数据深度合并（记录 merged_from 溯源列表）
    pub fn merge_metadata(survivor: &MemoryEntry, members: &[MemoryEntry]) -> serde_json::Value {
        let mut base = match survivor.metadata.clone() {
            serde_json::Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };

        // 收集所有被合并条目的溯源 ID 与来源 Agent
        let mut source_entries = Vec::new();
        for m in members {
            if m.id != survivor.id {
                source_entries.push(serde_json::json!({
                    "id": m.id,
                    "source_agent": m.source_agent,
                    "updated_at": m.updated_at.to_rfc3339(),
                }));
            }
        }

        base.insert(
            "_merged_sources".to_string(),
            serde_json::Value::Array(source_entries),
        );
        serde_json::Value::Object(base)
    }

    /// 文本片段去重拼接
    pub fn concatenate_unique_text(survivor: &MemoryEntry, members: &[MemoryEntry]) -> String {
        let mut combined = survivor.content.clone();
        let mut seen_lines: HashSet<String> = combined
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();

        for m in members {
            if m.id == survivor.id {
                continue;
            }
            let mut diff_lines = Vec::new();
            for line in m.content.lines() {
                let trimmed = line.trim();
                if !trimmed.is_empty() && !seen_lines.contains(trimmed) {
                    diff_lines.push(line);
                    seen_lines.insert(trimmed.to_string());
                }
            }
            if !diff_lines.is_empty() {
                combined.push_str("\n\n---\n[合并补充事实]\n");
                combined.push_str(&diff_lines.join("\n"));
            }
        }

        combined
    }
}

#[async_trait]
impl ContentMerger for TieredMerger {
    async fn merge(&self, survivor: &MemoryEntry, members: &[MemoryEntry]) -> Result<MergedOutput> {
        let tags = Self::merge_tags(survivor, members);
        let metadata = Self::merge_metadata(survivor, members);
        let confidence = members
            .iter()
            .map(|m| m.confidence)
            .fold(survivor.confidence, f32::max);

        let strategy = self
            .default_strategy
            .unwrap_or(MergeStrategy::ConcatenateUnique);

        let content = match strategy {
            MergeStrategy::KeepLatest => survivor.content.clone(),
            MergeStrategy::ConcatenateUnique => Self::concatenate_unique_text(survivor, members),
        };

        // 融合正文的长度护栏：schema 里有 `length(content) between 1 and 8192` 的 CHECK。
        // 拼接不去限时，大组自动合并必然在写库时炸出约束错误，而调用方把它咽成"生成提案"，
        // 表现为"整合跑了但没收敛"。这里提前判失败，让引擎留下待审提案而不是撞库。
        if content.chars().count() > mem_core::memory_service::MAX_CONTENT_CHARS {
            return Err(common::HubError::InvalidArgument(format!(
                "合并正文 {} 字符，超出单条上限 {}，需人工裁决",
                content.chars().count(),
                mem_core::memory_service::MAX_CONTENT_CHARS
            )));
        }

        Ok(MergedOutput {
            content,
            tags,
            metadata,
            confidence,
            strategy,
        })
    }
}
