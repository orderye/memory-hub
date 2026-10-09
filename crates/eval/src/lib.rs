#![forbid(unsafe_code)]

//! 评测台与量表校准系统（eval）
//!
//! 提供：
//! - `ScenarioRunner`：关系圈层场景集自动化测试套件
//! - `RubricCalibrator`：软项权重校准与反迎合底线约束

pub mod calibration;
pub mod scenario;

pub use calibration::{CalibratedWeights, RubricCalibrator};
pub use scenario::{EvalScenario, ScenarioReport, ScenarioRunner};
