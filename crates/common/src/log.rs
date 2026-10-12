//! 日志初始化与脱敏过滤器（基于设计规范 19.3 节数据红线）

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// 初始化全局日志（强制脱敏，禁止打印原始对话与敏感实体）
///
/// 当 `stdio_mode` 为 true 时，日志强制输出到 stderr，保证 stdout 纯净用于 JSON-RPC 帧流传输；
/// 否则输出到标准输出 stdout。
pub fn init_tracing_with_stdio(stdio_mode: bool) {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,memory_hub=debug"));

    if stdio_mode {
        let fmt_layer = tracing_subscriber::fmt::layer()
            .with_target(true)
            .with_thread_ids(true)
            .with_level(true)
            .with_writer(std::io::stderr)
            .json();

        let _ = tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt_layer)
            .try_init();
    } else {
        let fmt_layer = tracing_subscriber::fmt::layer()
            .with_target(true)
            .with_thread_ids(true)
            .with_level(true)
            .with_writer(std::io::stdout)
            .json();

        let _ = tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt_layer)
            .try_init();
    }
}

/// 兼容传统调用：若环境指示 HUB_SERVE_MODE=stdio 则自动定向至 stderr，否则输出至 stdout
pub fn init_tracing() {
    let stdio_mode = std::env::var("HUB_SERVE_MODE")
        .map(|v| v.eq_ignore_ascii_case("stdio"))
        .unwrap_or(false);
    init_tracing_with_stdio(stdio_mode);
}
