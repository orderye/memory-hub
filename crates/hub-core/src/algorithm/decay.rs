//! 算法 B：置信度更新与时间常数衰减（设计规范 17.4 B）

use crate::domain::{ItemStatus, Layer, Provenance};
use chrono::Duration;

/// 置信度更新事件
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfidenceEvent {
    /// 预测命中
    PredictionHit,
    /// 预测未命中
    PredictionMiss,
    /// 用户明确确认
    UserConfirmed,
    /// 用户明确否定
    UserDenied,
}

/// 置信度更新结果
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConfidenceUpdateResult {
    pub confidence: f32,
    pub status: ItemStatus,
    pub provenance: Option<Provenance>,
}

/// 根据事件更新置信度
///
/// - 预测命中: `c ← c + 0.2·(1 − c)`
/// - 预测未命中: `c ← 0.7·c`
/// - 用户否定: `status = retired`
/// - 用户确认: `provenance = stated`, `c = 1.0`
pub fn update_confidence_on_event(
    current_confidence: f32,
    current_status: ItemStatus,
    event: ConfidenceEvent,
) -> ConfidenceUpdateResult {
    match event {
        ConfidenceEvent::PredictionHit => {
            let new_conf = current_confidence + 0.2 * (1.0 - current_confidence);
            let clamped = new_conf.clamp(0.0, 1.0);
            ConfidenceUpdateResult {
                confidence: clamped,
                status: current_status,
                provenance: None,
            }
        }
        ConfidenceEvent::PredictionMiss => {
            let new_conf = (0.7 * current_confidence).clamp(0.0, 1.0);
            let status = if new_conf < 0.2 {
                ItemStatus::Retired
            } else {
                current_status
            };
            ConfidenceUpdateResult {
                confidence: new_conf,
                status,
                provenance: None,
            }
        }
        ConfidenceEvent::UserConfirmed => ConfidenceUpdateResult {
            confidence: 1.0,
            status: ItemStatus::Active,
            provenance: Some(Provenance::Stated),
        },
        ConfidenceEvent::UserDenied => ConfidenceUpdateResult {
            confidence: 0.0,
            status: ItemStatus::Retired,
            provenance: None,
        },
    }
}

/// 获取各记忆层衰减常数 τ (天数)
pub fn get_decay_tau_days(layer: Layer) -> Option<f32> {
    match layer {
        Layer::State => Some(7.0),
        Layer::Preference => Some(90.0),
        Layer::Relation => Some(365.0),
        Layer::Behavior => Some(90.0),
        Layer::GoalBelief => Some(180.0),
        Layer::NormPref => Some(365.0),
        Layer::Fact | Layer::Ledger => None, // 事实与人情账不按常数衰减，只按 TTL
    }
}

/// 计算时间衰减
///
/// 衰减公式: `c ← c·exp(−Δt / τ)`
/// - `stated` 事实不衰减
/// - 若衰减后置信度低于 0.2，标记为 Retired
pub fn apply_decay(
    confidence: f32,
    layer: Layer,
    provenance: Provenance,
    delta: Duration,
) -> (f32, ItemStatus) {
    // 明说事实不衰减
    if provenance == Provenance::Stated {
        return (confidence, ItemStatus::Active);
    }

    if let Some(tau_days) = get_decay_tau_days(layer) {
        let delta_days = delta.num_seconds() as f32 / 86400.0;
        if delta_days <= 0.0 {
            return (confidence, ItemStatus::Active);
        }
        let decayed = confidence * (-delta_days / tau_days).exp();
        let clamped = decayed.clamp(0.0, 1.0);
        let status = if clamped < 0.2 {
            ItemStatus::Retired
        } else {
            ItemStatus::Active
        };
        (clamped, status)
    } else {
        (confidence, ItemStatus::Active)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prediction_hit_and_miss() {
        let initial = 0.5;
        // Hit: 0.5 + 0.2 * (1 - 0.5) = 0.6
        let res_hit =
            update_confidence_on_event(initial, ItemStatus::Active, ConfidenceEvent::PredictionHit);
        assert!((res_hit.confidence - 0.6).abs() < 1e-4);
        assert_eq!(res_hit.status, ItemStatus::Active);

        // Miss: 0.6 * 0.7 = 0.42
        let res_miss = update_confidence_on_event(
            res_hit.confidence,
            ItemStatus::Active,
            ConfidenceEvent::PredictionMiss,
        );
        assert!((res_miss.confidence - 0.42).abs() < 1e-4);
        assert_eq!(res_miss.status, ItemStatus::Active);
    }

    #[test]
    fn test_user_confirmed_and_denied() {
        let confirmed = update_confidence_on_event(
            0.4,
            ItemStatus::PendingConfirm,
            ConfidenceEvent::UserConfirmed,
        );
        assert_eq!(confirmed.confidence, 1.0);
        assert_eq!(confirmed.status, ItemStatus::Active);
        assert_eq!(confirmed.provenance, Some(Provenance::Stated));

        let denied =
            update_confidence_on_event(0.8, ItemStatus::Active, ConfidenceEvent::UserDenied);
        assert_eq!(denied.confidence, 0.0);
        assert_eq!(denied.status, ItemStatus::Retired);
    }

    #[test]
    fn test_time_decay() {
        // State 层 τ = 7 天，经过 7 天衰减到 1/e ≈ 0.367879
        let initial = 1.0;
        let (decayed, status) = apply_decay(
            initial,
            Layer::State,
            Provenance::Inferred,
            Duration::days(7),
        );
        assert!((decayed - 1.0 / std::f32::consts::E).abs() < 1e-3);
        assert_eq!(status, ItemStatus::Active);

        // 持续衰减使得 confidence < 0.2 则置为 Retired
        let (decayed_low, status_low) = apply_decay(
            initial,
            Layer::State,
            Provenance::Inferred,
            Duration::days(21),
        );
        assert!(decayed_low < 0.2);
        assert_eq!(status_low, ItemStatus::Retired);

        // Stated 事实即使经过 365 天也不衰减
        let (fact_decay, fact_status) =
            apply_decay(0.9, Layer::Fact, Provenance::Stated, Duration::days(365));
        assert_eq!(fact_decay, 0.9);
        assert_eq!(fact_status, ItemStatus::Active);
    }
}
