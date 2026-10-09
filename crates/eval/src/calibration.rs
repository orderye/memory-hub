//! 量表参数优化与离线校准管道（基于设计规范 7.4 节）

use serde::{Deserialize, Serialize};

/// 软项权重配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalibratedWeights {
    pub coherence: f32,
    pub empathy: f32,
    pub memory_fit: f32,
    pub anti_sycophancy: f32,
    pub relation: f32,
    pub tau_threshold: f32,
}

impl Default for CalibratedWeights {
    fn default() -> Self {
        // 对应设计规范 7.4 节起步值
        Self {
            coherence: 0.25,
            empathy: 0.20,
            memory_fit: 0.20,
            anti_sycophancy: 0.20,
            relation: 0.15,
            tau_threshold: 0.80,
        }
    }
}

/// 量表校准优化器
pub struct RubricCalibrator;

impl RubricCalibrator {
    /// 校验并校正权重（强制执行反迎合权重下限 >= 0.15）
    pub fn calibrate_and_normalize(mut weights: CalibratedWeights) -> CalibratedWeights {
        const ANTI_SYCOPHANCY_FLOOR: f32 = 0.15;

        let other_sum = weights.coherence + weights.empathy + weights.memory_fit + weights.relation;
        let total = other_sum + weights.anti_sycophancy;

        if total <= 0.0 {
            return CalibratedWeights::default();
        }

        // 检查归一化后反迎合比例是否低于下限
        if weights.anti_sycophancy / total < ANTI_SYCOPHANCY_FLOOR {
            weights.anti_sycophancy = ANTI_SYCOPHANCY_FLOOR;
            let target_other = 1.0 - ANTI_SYCOPHANCY_FLOOR;
            if other_sum > 0.0 {
                let scale = target_other / other_sum;
                weights.coherence *= scale;
                weights.empathy *= scale;
                weights.memory_fit *= scale;
                weights.relation *= scale;
            }
        } else {
            weights.coherence /= total;
            weights.empathy /= total;
            weights.memory_fit /= total;
            weights.anti_sycophancy /= total;
            weights.relation /= total;
        }

        // 3. 约束阈值 tau 范围 (0.5 ~ 0.95)
        weights.tau_threshold = weights.tau_threshold.clamp(0.50, 0.95);

        weights
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_anti_sycophancy_floor_enforcement() {
        let low_anti = CalibratedWeights {
            coherence: 0.40,
            empathy: 0.30,
            memory_fit: 0.20,
            anti_sycophancy: 0.05, // 低于 0.15 下限
            relation: 0.05,
            tau_threshold: 0.85,
        };

        let calibrated = RubricCalibrator::calibrate_and_normalize(low_anti);
        assert!(calibrated.anti_sycophancy >= 0.15);

        let sum = calibrated.coherence
            + calibrated.empathy
            + calibrated.memory_fit
            + calibrated.anti_sycophancy
            + calibrated.relation;
        assert!((sum - 1.0).abs() < 1e-5, "Normalized sum must be 1.0");
    }
}
