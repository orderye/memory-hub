# ==============================================================================
# CNB 云原生开发环境（仅预览模式：账户控制台；保留 SSH 客户端接入）
#
# 由 .cnb.yml 的 .vscode_preview 片段（main / `$` 两处引用）通过
# build: .ide/Dockerfile 构建；启动脚本是 .ide/preview.sh。
# 镜像里保留 code-server 与 openssh-server：仅预览模式本身不支持 WebIDE，
# 但「头像 → 我的云原生开发」里的客户端/SSH 接入仍然可用。
#
# 与仓库根 Dockerfile 的分工：那个是生产运行时（只编译、不带开发工具）；
# 这里用 rust:1-bookworm（buildpack-deps 已含 gcc/git/curl），装齐开发期工具。
# ==============================================================================
FROM rust:1-bookworm

# 节点在腾讯云内网：默认走 tencentyun 的 apt 与 rsproxy 的 crates 稀疏索引，
# 与 .cnb.yml 生产构建 / 质量门禁保持一致。换网络构建时可 --build-arg 覆盖。
ARG APT_MIRROR=mirrors.tencentyun.com
ARG CRATES_MIRROR=sparse+https://rsproxy.cn/index/

# build-essential + pkg-config : tikv-jemalloc-sys 要过一遍 C 编译器
# postgresql-client            : 手工跑 crates/store/migrations/*.sql、连真库调试
# openssh-server               : Remote-SSH 接入
RUN if [ "${APT_MIRROR}" != "deb.debian.org" ]; then \
      sed -i "s|deb.debian.org|${APT_MIRROR}|g; s|security.debian.org|${APT_MIRROR}|g" \
        /etc/apt/sources.list.d/debian.sources /etc/apt/sources.list 2>/dev/null || true; \
    fi \
 && apt-get update \
 && apt-get install -y --no-install-recommends \
      build-essential pkg-config ca-certificates curl git jq ripgrep less vim unzip \
      openssh-server postgresql-client \
 && rm -rf /var/lib/apt/lists/* \
 && mkdir -p /var/run/sshd

# crates.io 镜像（与 .cnb.yml 里的配置逐字一致）
RUN if [ -n "${CRATES_MIRROR}" ]; then \
      printf '%s\n' \
        '[source.crates-io]' \
        'replace-with = "mirror"' \
        '[source.mirror]' \
        "registry = \"${CRATES_MIRROR}\"" \
        '[net]' \
        'git-fetch-with-cli = true' \
        > /usr/local/cargo/config.toml; \
    fi

# code-server：装上它 CNB 才走单容器模式，浏览器终端才拿得到本镜像的 Rust
RUN curl -fsSL https://code-server.dev/install.sh | sh

# 常用插件（Open VSX）。rust-analyzer 是硬需求，装失败应当让镜像构建失败
RUN set -eux; \
    for ext in \
      cnbcool.cnb-welcome \
      rust-lang.rust-analyzer \
      tamasfe.even-better-toml \
      redhat.vscode-yaml \
      mhutchie.git-graph \
      ms-azuretools.vscode-docker; do \
      code-server --install-extension "$ext"; \
    done

ENV LANG=C.UTF-8 \
    LANGUAGE=C.UTF-8 \
    LC_ALL=C.UTF-8 \
    CARGO_TERM_COLOR=always \
    RUST_BACKTRACE=1
