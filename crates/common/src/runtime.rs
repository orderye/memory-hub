#![forbid(unsafe_code)]

//! Tokio 异步运行时配置与启动辅助
//!
//! 支持通过 `TOKIO_WORKER_THREADS` 环境变量严格对齐容器 CPU 配额；
//! 启动前执行强校验（Fail-Fast），避免非法配置导致运行时隐式 panic。

use anyhow::{anyhow, Result};
use std::env;
use tokio::runtime::{Builder, Runtime};
use tracing::info;

/// 校验 `TOKIO_WORKER_THREADS` 环境变量配置
pub fn parse_worker_threads() -> Result<Option<usize>> {
    if let Ok(raw) = env::var("TOKIO_WORKER_THREADS") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            let threads = trimmed
                .parse::<usize>()
                .map_err(|_| anyhow!("TOKIO_WORKER_THREADS 不是合法的正整数: {}", raw))?;
            if threads == 0 {
                return Err(anyhow!("TOKIO_WORKER_THREADS 必须大于 0"));
            }
            return Ok(Some(threads));
        }
    }
    Ok(None)
}

/// 读取 `TOKIO_WORKER_THREADS` 环境变量或默认配置并构建多线程运行时
pub fn build_runtime() -> Result<Runtime> {
    let threads = parse_worker_threads()?;

    let mut builder = Builder::new_multi_thread();
    builder.enable_all();

    if let Some(n) = threads {
        builder.worker_threads(n);
        info!(worker_threads = n, "Tokio 工作线程上限已配置");
    }

    builder
        .build()
        .map_err(|e| anyhow!("初始化 Tokio 运行时失败: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn test_parse_worker_threads_semantics() {
        let _guard = ENV_LOCK.lock().unwrap();

        std::env::remove_var("TOKIO_WORKER_THREADS");
        assert_eq!(parse_worker_threads().unwrap(), None);

        std::env::set_var("TOKIO_WORKER_THREADS", "4");
        assert_eq!(parse_worker_threads().unwrap(), Some(4));

        std::env::set_var("TOKIO_WORKER_THREADS", "0");
        let err = parse_worker_threads().unwrap_err();
        assert!(err.to_string().contains("必须大于 0"));

        std::env::set_var("TOKIO_WORKER_THREADS", "not-a-number");
        let err2 = parse_worker_threads().unwrap_err();
        assert!(err2.to_string().contains("不是合法的正整数"));

        std::env::remove_var("TOKIO_WORKER_THREADS");
    }

    #[test]
    fn test_build_runtime_execution() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TOKIO_WORKER_THREADS");

        let rt = build_runtime().expect("构建默认运行时应该成功");
        let res = rt.block_on(async { 42 });
        assert_eq!(res, 42);

        std::env::set_var("TOKIO_WORKER_THREADS", "2");
        let rt2 = build_runtime().expect("构建指定线程运行时应该成功");
        let res2 = rt2.block_on(async { 100 });
        assert_eq!(res2, 100);

        std::env::remove_var("TOKIO_WORKER_THREADS");
    }
}
