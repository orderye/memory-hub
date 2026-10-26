#!/usr/bin/env bash
#
# 生产部署阶段（main.push 与 tag_deploy.production 共用，避免两份部署脚本漂移）。
#
# 输入：
#   /tmp/release.env 中的 DEPLOY_IMAGE（镜像 digest）、RELEASE_TAG（发布标识）
#   DEPLOY_HOST、SSH_KEY_B64 来自密钥仓 toroyxxx/ci-secrets 的 deploy-key.yml
#
# 行为：远端切换镜像 → docker compose pull/up → /readyz 探活（最多约 30 秒）
#       → 失败时回滚到上一稳定版本并退出 1。

set -euo pipefail

source /tmp/release.env
: "${DEPLOY_IMAGE:?需要 DEPLOY_IMAGE（由上一层写入 /tmp/release.env）}"
: "${DEPLOY_HOST:?需要 DEPLOY_HOST（由 ci-secrets 的 deploy-key.yml 导入）}"
: "${SSH_KEY_B64:?需要 SSH_KEY_B64（由 ci-secrets 的 deploy-key.yml 导入）}"

command -v ssh >/dev/null 2>&1 || { apt-get update -qq && apt-get install -y -qq openssh-client; }
echo "$SSH_KEY_B64" | base64 -d > /tmp/deploy_key && chmod 600 /tmp/deploy_key

echo "部署 ${RELEASE_TAG:-unknown}：${DEPLOY_IMAGE}"
ssh -i /tmp/deploy_key -o StrictHostKeyChecking=accept-new ubuntu@${DEPLOY_HOST} "
  cd ~/memory-hub &&
  if [ -f current_release.env ]; then
    cp current_release.env previous_release.env
  fi
  echo \"MEMORY_HUB_IMAGE=${DEPLOY_IMAGE}\" > current_release.env &&
  set -a && source current_release.env && set +a &&
  docker compose pull mcp-server auth-server backup-worker &&
  docker compose up -d &&
  SUCCESS=0
  for i in \$(seq 1 15); do
    if curl -fsS http://127.0.0.1:8080/readyz >/dev/null 2>&1 && curl -fsS http://127.0.0.1:3000/readyz >/dev/null 2>&1; then
      echo '服务就绪探活通过 (readyz 200)'
      SUCCESS=1
      break
    fi
    sleep 2
  done
  if [ \"\$SUCCESS\" -ne 1 ]; then
    echo '探针就绪检测失败，触发自动回滚...'
    if [ -f previous_release.env ]; then
      cp previous_release.env current_release.env
      set -a && source current_release.env && set +a
      docker compose pull mcp-server auth-server backup-worker
      docker compose up -d
      echo '已回滚至上一稳定版本'
    fi
    exit 1
  fi
  docker compose ps
"
