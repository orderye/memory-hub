#![forbid(unsafe_code)]

//! 规范包管理与原子热替换（hub-packs）
//!
//! 提供：
//! - `AtomicPackRegistry`：无锁热替换规范包，灰度分桶调度
//! - 核心锁（Core Lock）完整性防篡改强校验

pub mod registry;

pub use registry::AtomicPackRegistry;
