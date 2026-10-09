#![forbid(unsafe_code)]

//! 确定性规则引擎与冲突消解库（hub-rules）
//!
//! 提供：
//! - `DefaultRuleEngine`：纯内存高效规则匹配
//! - C1~C6 预编译冲突消解器

pub mod engine;

pub use engine::DefaultRuleEngine;
