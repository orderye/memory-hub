#!/usr/bin/env bash
# ==============================================================================
# CNB「仅预览模式」的 launch 脚本（.cnb.yml 里 `vscode.options.launch` 调用）
#
# 平台约定：
#   1. 业务必须监听 8686；
#   2. launch 要在 5 分钟内自己退出，平台随后每 10 秒探一次端口（最多约 200 秒）；
# 所以这里做的是：后台起 auth-server → 等 /healthz 就绪 → 打印地址 → 退出。
#
# 预览只起 auth-server（账户控制台）。mcp-server 不在这里起：内存存储是进程内的，
# 两个进程各拿一份数据，硬起一个「能连但数据对不上」的 MCP 端点比不起更误导。
# ==============================================================================
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT}"

PORT="${PREVIEW_PORT:-8686}"
BIN="${PREVIEW_BIN:-${ROOT}/target/debug/auth-server}"
LOG="${PREVIEW_LOG:-/tmp/memory-hub-preview.log}"
BASE="http://127.0.0.1:${PORT}"

# 同一环境里再次打开预览时，直接复用已在跑的实例
if curl -fsS "${BASE}/healthz" >/dev/null 2>&1; then
  echo "预览服务已在 ${PORT} 端口运行：${BASE}"
  exit 0
fi

# 正常路径由 .cnb.yml 的 stages 预先 cargo build；这里只是缺失时的兜底
if [ ! -x "${BIN}" ]; then
  echo "未找到 ${BIN}，现场编译（首次点开会慢一些）"
  cargo build -p auth-server
fi

# 预览不接真库：HUB_DB_URL 不是 postgres:// 时 auth-server 用内存存储，进程退出即清空
export HUB_DB_URL="memory://preview"
export HUB_DEPLOYMENT_PROFILE="lite"
export HUB_EMBED_PROVIDER="off"
export AUTH_BIND="0.0.0.0:${PORT}"
export AUTH_EXTERNAL_BASE_URL="http://localhost:${PORT}"
export RUST_LOG="${RUST_LOG:-info}"
# 只在本进程生命周期内签发/校验令牌；每次启动随机生成，不落仓库
export HUB_TOKEN_SECRET="${HUB_TOKEN_SECRET:-$(head -c 64 /dev/urandom | od -An -tx1 | tr -d ' \n')}"

# nohup + 重定向：脚本退出后进程仍归环境容器管，不会被 shell 退出带走
nohup "${BIN}" >"${LOG}" 2>&1 </dev/null &
PID=$!

for _ in $(seq 1 60); do
  if curl -fsS "${BASE}/healthz" >/dev/null 2>&1; then
    echo "预览服务已就绪：${BASE}"
    echo "控制台：${BASE}/console（未登录会跳登录页，可自助注册）"
    exit 0
  fi
  if ! kill -0 "${PID}" 2>/dev/null; then
    echo "预览服务启动失败，日志尾部："
    tail -n 50 "${LOG}" || true
    exit 1
  fi
  sleep 1
done

echo "60 秒内未就绪，日志尾部："
tail -n 50 "${LOG}" || true
exit 1
