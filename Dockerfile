# ==============================================================================
# 记忆中枢（Memory Hub）生产多阶段镜像构建 Dockerfile
# 基于 Debian Bookworm Slim 与纯 Rust 静态/动态安全构建
# ==============================================================================

# ----------------- 阶段 1: 构建阶段 -----------------
FROM rust:1.80-slim-bookworm AS builder

WORKDIR /build

# 安装基础编译依赖
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# 复制 Cargo Workspace 配置文件与源码
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# 编译所有二进制产物（Release 优化模式）
RUN cargo build --release --bins

# ----------------- 阶段 2: 极简运行镜像 -----------------
FROM debian:bookworm-slim AS runtime

# 安全加固：创建非 root 运行用户
RUN groupadd -r memoryhub && useradd -r -g memoryhub -s /bin/false memoryhub

# 安装最小运行时依赖（SSL 证书与动态库）
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# 从构建镜像拷贝 4 大独立二进制
COPY --from=builder /build/target/release/mcp-server /app/mcp-server
COPY --from=builder /build/target/release/auth-server /app/auth-server
COPY --from=builder /build/target/release/offline-worker /app/offline-worker
COPY --from=builder /build/target/release/editor-server /app/editor-server

# 权限分配
RUN chown -R memoryhub:memoryhub /app
USER memoryhub

# 默认暴露端口：
# 8080: OAuth 2.1 授权服务器
# 8081: 规则编审工作台
# 3000: MCP Server (HTTP Streamable)
EXPOSE 8080 8081 3000

# 默认启动 MCP 服务器（容器化时可通过 CMD/ENTRYPOINT 覆盖）
ENTRYPOINT ["/app/mcp-server"]
