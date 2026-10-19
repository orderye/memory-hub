//! 陈旧记忆检测器：时间衰减（真实半衰期） × 低置信放大
//!
//! 口径说明（此前为可加模型，存在"高置信条目永不过阈值"的结构性缺陷）：
//! - 时间项 `1 - 0.5^(age/half_life)` 随年龄单调趋近 1.0，即无论置信度多高，
//!   足够久远的条目最终都会越过陈旧阈值；
//! - 低置信度以乘性因子放大陈旧度（`1 + 0.5*(1-conf)`），使不可信条目更早陈旧；
//! - 未修订（`version <= 1`）的小幅加成只作为轻微提示。
//!
//! `half_life_days` 是**半衰期**（时间项在年龄等于该值时正好为 0.5），命名与实际一致。

use crate::types::StaleEntry;
use chrono::{DateTime, Utc};
use mem_core::MemoryEntry;

#[derive(Debug, Clone)]
pub struct StalenessDetector {
    /// 时间衰减半衰期（天）：年龄达到该值时时间项为 0.5
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

        // 时间项：真实半衰期语义，年龄 →∞ 时趋近 1.0（高置信条目最终也会陈旧）
        let time_decay = 1.0 - 0.5f32.powf(age_days / self.half_life_days);
        // 低置信度放大陈旧度：conf=1.0 → ×1.0；conf=0.0 → ×1.5
        let unconfidence = (1.0 - entry.confidence).clamp(0.0, 1.0);
        let amplification = 1.0 + 0.5 * unconfidence;
        // 单次写入、从未修订的条目轻微加成
        let unrevised = if entry.version <= 1 { 0.05 } else { 0.0 };

        let staleness_score = (time_decay * amplification + unrevised).clamp(0.0, 1.0);

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

#[cfg(test)]
mod tests {
    use super::*;
    use mem_core::{CognitiveLayer, MemoryKind, MemoryScene, MemoryStatus};
    use serde_json::json;
    use uuid::Uuid;

    fn entry(confidence: f32, version: i64, age_days: i64) -> MemoryEntry {
        let now = Utc::now();
        MemoryEntry {
            id: Uuid::new_v4(),
            namespace: "ns".to_string(),
            key: None,
            kind: MemoryKind::Fact,
            content: "内容".to_string(),
            tags: vec![],
            metadata: json!({}),
            embedding: None,
            embedding_model: None,
            source_agent: "a".to_string(),
            confidence,
            status: MemoryStatus::Active,
            version,
            expires_at: None,
            merged_into: None,
            created_at: now - chrono::Duration::days(age_days),
            updated_at: now - chrono::Duration::days(age_days),
            scene: Some(MemoryScene::Chat),
            layer: Some(CognitiveLayer::Fact),
            valid_from: None,
            valid_until: None,
            occurred_at: None,
        }
    }

    #[test]
    fn test_half_life_semantics() {
        let d = StalenessDetector::new(60.0, 0.65);
        let now = Utc::now();
        // 恰好一个半衰期：时间项应为 0.5（conf=1、version>=2 时综合分即 0.5）
        let s = d.evaluate_entry(&entry(1.0, 2, 60), now);
        assert!(
            (s.staleness_score - 0.5).abs() < 1e-3,
            "半衰期处时间项应为 0.5：{}",
            s.staleness_score
        );
    }

    #[test]
    fn test_high_confidence_entries_eventually_stale() {
        // 回归：旧的可加模型下 conf ≥ 0.8 的条目无论多久都不会过阈值
        let d = StalenessDetector::default();
        let now = Utc::now();
        let s = d.evaluate_entry(&entry(1.0, 5, 3 * 365), now);
        assert!(
            s.staleness_score >= d.staleness_threshold,
            "高置信条目三年未更新仍必须判为陈旧：{}",
            s.staleness_score
        );
    }

    #[test]
    fn test_lower_confidence_stales_sooner() {
        let d = StalenessDetector::default();
        let now = Utc::now();
        let high = d.evaluate_entry(&entry(1.0, 2, 120), now);
        let low = d.evaluate_entry(&entry(0.5, 2, 120), now);
        assert!(
            low.staleness_score > high.staleness_score,
            "同样年龄下低置信条目应更陈旧：{} vs {}",
            low.staleness_score,
            high.staleness_score
        );
    }

    #[test]
    fn test_detect_filters_below_threshold() {
        let d = StalenessDetector::new(60.0, 0.65);
        let now = Utc::now();
        let entries = vec![entry(1.0, 2, 5), entry(1.0, 2, 400)];
        let stale = d.detect_in_entries(&entries, now);
        assert_eq!(stale.len(), 1, "只有真正陈旧的条目进入列表");
        assert!(stale[0].age_days > 300.0);
    }
}
