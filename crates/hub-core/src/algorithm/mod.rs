//! 核心算法实现模块（基于设计规范 17.4 节）
//!
//! 包含：
//! - A: 记忆排序打分与 MMR 多样性重排 (ranking)
//! - B: 置信度更新与时间常数衰减 (decay)
//! - C: 推断条目升格为长期信念 (promote)
//! - D: 假设边际判定 (margin)

pub mod decay;
pub mod margin;
pub mod promote;
pub mod ranking;

pub use decay::{apply_decay, update_confidence_on_event, ConfidenceEvent};
pub use margin::{check_margin, MarginResult};
pub use promote::{can_promote_to_belief, PromotionCandidate};
pub use ranking::{calculate_item_score, mmr_rerank, ItemScoreParams};
