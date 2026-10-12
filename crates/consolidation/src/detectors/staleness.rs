//! 陈旧记忆检测器：时间衰减与置信度综合评估

use crate::types::StaleEntry;
use chrono::{DateTime, Utc};
use mem_core::MemoryEntry;

#[derive(Debug, Clone)]
pub struct StalenessDetector {
    /// 半衰期（天），默认 60 天
    pub half_life_days: f32,
    /// 触发陈旧判定的综合分数下限 [0.0 ~ 1.0]
    pub staleness_threshold: f32,
}

impl Default for StalenessDetector {
    fn default() -> Self {
        Self {
            half_life_days: 60.0,
            staleness_threshold: 0.65,
        }
    }
}

impl StalenessDetector {
    pub fn new(half_life_days: f32, staleness_threshold: f32) -> Self {
        Self {
            half_life_days: half_life_days.max(1.0),
            staleness_threshold: staleness_threshold.clamp(0.0, 1.0),
        }
    }

    /// 评估单条记忆的陈旧度
    pub fn evaluate_entry(&self, entry: &MemoryEntry, now: DateTime<Utc>) -> StaleEntry {
        let age_seconds = (now - entry.updated_at).num_seconds().max(0) as f32;
        let age_days = age_seconds / 86_400.0;

        // 时间久远度因子：使用指数饱和曲线
        let time_decay = 1.0 - (-age_days / self.half_life_days).exp();
        // 低置信度加重陈旧度
        let unconfidence = (1.0 - entry.confidence).clamp(0.0, 1.0);
        // 单次写入未曾迭代更新的条目稍加衰减
        let single_version_penalty = if entry.version <= 1 { 0.2 } else { 0.0 };

        let staleness_score =
            (0.55 * time_decay + 0.35 * unconfidence + 0.10 * single_version_penalty)
                .clamp(0.0, 1.0);

        StaleEntry {
            id: entry.id,
            age_days,
            confidence: entry.confidence,
            version: entry.version,
            staleness_score,
        }
    }

    /// 扫描命名空间中的陈旧记忆列表
    pub fn detect_in_entries(
        &self,
        entries: &[MemoryEntry],
        now: DateTime<Utc>,
    ) -> Vec<StaleEntry> {
        entries
            .iter()
            .map(|e| self.evaluate_entry(e, now))
            .filter(|s| s.staleness_score >= self.staleness_threshold)
            .collect()
    }
}
