//! 作业重试骨架：瞬态错误指数退避重试，供各异步提炼作业共用
//!
//! 语义（与 Chat 认知 / 经验提炼两个作业的原始实现逐条一致）：
//! - 只重试 [`HubError::is_retryable`] 为真的错误——`InvalidArgument`
//!   （质量门槛拒绝）等不可重试错误首次即返回；
//! - 指数退避 `initial_backoff_ms * 2^(attempt-1)`；
//! - 每次失败打结构化 warn（带 worker / 作业名 / 尝试次数 / 可重试性）；
//!   最终失败与成功的结果日志由调用方记录（各自有作业语义可表达）。

use common::{HubError, Result};
use std::future::Future;
use std::pin::Pin;
use tracing::warn;

/// 执行 `op` 至多 `max_retries` 次，返回 `(结果, 实际尝试次数)`。
///
/// `op` 每次调用产出**独立**的 Future：闭包需自行克隆本次尝试所需的数据
/// （auth、输入载荷等）。重试很少发生，按次克隆的成本可以忽略。
pub(crate) async fn with_retry<T, F>(
    worker: &str,
    op_name: &str,
    max_retries: usize,
    initial_backoff_ms: u64,
    mut op: F,
) -> Result<(T, usize)>
where
    F: FnMut() -> Pin<Box<dyn Future<Output = Result<T>> + Send>>,
{
    let max_retries = max_retries.max(1);
    let mut attempts = 0usize;
    let mut last_err = None;

    while attempts < max_retries {
        attempts += 1;
        match op().await {
            Ok(value) => return Ok((value, attempts)),
            Err(e) => {
                let retryable = e.is_retryable();
                warn!(
                    worker = %worker,
                    op = %op_name,
                    attempt = attempts,
                    max_retries,
                    retryable,
                    error = %e,
                    "作业尝试失败"
                );

                if !retryable || attempts >= max_retries {
                    last_err = Some(e);
                    break;
                }

                // 瞬态重试退避
                let backoff = initial_backoff_ms * (1u64 << (attempts - 1));
                tokio::time::sleep(tokio::time::Duration::from_millis(backoff)).await;
            }
        }
    }

    Err(last_err.unwrap_or(HubError::Unavailable))
}
