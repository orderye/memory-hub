//! 日志初始化与脱敏过滤器（基于设计规范 19.3 节数据红线）

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// 默认把本工作区各 crate 提到 debug，其余保持 info。
///
/// tracing 的 target 是模块路径（首段为 crate 名，连字符写作下划线）。历史默认值写的
/// 是 `memory_hub`，它不匹配任何真实 target，因此那条 debug 指令从未生效——
/// 生产排查时误以为"日志太少"。
pub const LOG_TARGETS_DEBUG: &str = "mem_core=debug,store=debug,mcp=debug,auth_web=debug,\
     auth_core=debug,authz=debug,retrieval=debug,consolidation=debug,embed=debug,worker=debug,\
     common=debug";

/// 初始化全局日志（强制脱敏，禁止打印原始对话与敏感实体）
///
/// 当 `stdio_mode` 为 true 时，日志强制输出到 stderr，保证 stdout 纯净用于 JSON-RPC 帧流传输；
/// 否则输出到标准输出 stdout。
pub fn init_tracing_with_stdio(stdio_mode: bool) {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("info,{LOG_TARGETS_DEBUG}")));

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
