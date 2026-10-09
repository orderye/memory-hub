#![forbid(unsafe_code)]

//! 多路混合检索引擎（hub-retrieval）
//!
//! 提供：
//! - `HybridRetriever`：结合元数据过滤、关键词相关度、向量相似度与 MMR 多样性重排

pub mod hybrid;

pub use hybrid::{HybridRetriever, HybridRetrieverConfig};
