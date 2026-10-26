#!/usr/bin/env bash
# 质量门禁唯一事实来源：GitHub Actions 与 CNB 流水线共同调用。
# 两边 CI 配置只保留各自的环境准备（工具链 / 镜像源 / 缓存），命令本身不再双维护，
# 避免历史遗留的双份定义悄悄漂移。
#
# 用法：
#   scripts/ci/quality-gate.sh          # 依次跑 fmt / clippy / test
#   scripts/ci/quality-gate.sh fmt      # 仅格式检查
#   scripts/ci/quality-gate.sh clippy   # 仅静态检查
#   scripts/ci/quality-gate.sh test     # 仅测试
set -euo pipefail
cd "$(dirname "$0")/../.."

# CI 平台在各自 env 里也能注入；本脚本兜底保证单独运行时与 CI 同口径
export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-always}"
export RUSTFLAGS="${RUSTFLAGS:--D warnings}"

run_fmt() {
  cargo fmt --all -- --check
}

run_clippy() {
  # 必须带 --all-features：否则 adapter-cjs 适配层与 pg-integration 里的代码
  # 从头到尾不参与编译，编译错误要到发布构建甚至线上才暴露。
  cargo clippy --all-targets --all-features -- -D warnings
}

run_test() {
  cargo test --workspace --all-features
}

case "${1:-all}" in
  fmt) run_fmt ;;
  clippy) run_clippy ;;
  test) run_test ;;
  all) run_fmt && run_clippy && run_test ;;
  *)
    echo "用法: $0 [fmt|clippy|test|all]" >&2
    exit 2
    ;;
esac
