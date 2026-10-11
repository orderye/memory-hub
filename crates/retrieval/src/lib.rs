//! 多智能体长期记忆服务检索库（retrieval）
//!
//! 提供 `HybridRetriever`：FTS（关键词）+ 向量（语义）双路召回，
//! 五维加权融合后经 MMR 多样性重排，并带软超时保护。

pub mod hybrid;

pub use hybrid::HybridRetriever;
