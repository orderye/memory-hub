#![forbid(unsafe_code)]

//! 记忆中枢存储实现库（hub-store）
//!
//! 提供：
//! - `InMemoryMemoryStore`：内存级存储，用于开发、快速测试与断言验证
//! - 数据库迁移脚本（PostgreSQL 15+ 模式）

pub mod in_memory;

pub use in_memory::InMemoryMemoryStore;
