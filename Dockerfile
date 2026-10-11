# ==============================================================================
# 多智能体长期记忆服务（Memory Hub）生产多阶段镜像构建 Dockerfile
# 基于 Debian Bookworm Slim 与纯 Rust 构建（rustls，不依赖 OpenSSL）
# ==============================================================================

# ----------------- 阶段 1: 构建阶段 -----------------
# 使用浮动的最新稳定版 Rust：本项目用到 1.82+ 的语言特性
FROM rust:1-slim-bookworm AS builder

WORKDIR /build

# 安装基础编译依赖（包含 build-essential，用于支持 jemalloc 生产内存分配器构建）
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    ca-certificates \
    curl \
    build-essential \
    && rm -rf /var/lib/apt/lists/*

# 复制 Cargo Workspace 配置文件与源码
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# 编译所有二进制产物（Release 优化模式，启用 jemalloc 特性以在 Linux 生产环境中抑制内存碎片）
RUN cargo build --release --bins --features jemalloc

# ----------------- 阶段 2: 极简运行镜像 -----------------
FROM debian:bookworm-slim AS runtime

# 安全加固：创建非 root 运行用户
RUN groupadd -r memoryhub && useradd -r -g memoryhub -s /bin/false memoryhub

# 安装最小运行时依赖（SSL 证书；curl 供健康检查使用）
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# 从构建镜像拷贝三大独立二进制
COPY --from=builder /build/target/release/mcp-server /app/mcp-server
COPY --from=builder /build/target/release/auth-server /app/auth-server
COPY --from=builder /build/target/release/backup-worker /app/backup-worker

# 权限分配
RUN chown -R memoryhub:memoryhub /app
USER memoryhub

# 监听端口：
# 8080: OAuth 2.1 授权服务器 + Agent 控制台
# 3000: MCP Server (Streamable HTTP / JSON-RPC 2.0)
EXPOSE 8080 3000

# 默认启动 MCP 服务器；容器化时通过 docker-compose 的 entrypoint 覆盖
ENTRYPOINT ["/app/mcp-server"]
