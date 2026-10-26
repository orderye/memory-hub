#!/usr/bin/env bash
#
# 生产回滚阶段（main 分支详情页「回滚上一版本」按钮触发）。
#
# 与部署脚本的分工：部署把 current_release.env 切到新镜像、旧值先存进
# previous_release.env；回滚就是把上一稳定版本搬回 current_release.env 再拉起重启。
# 回滚不碰 previous_release.env，因此可以重复执行（每次回到同一个稳定版本）。
#
# 输入：
#   DEPLOY_HOST、SSH_KEY_B64 来自密钥仓 toroyxxx/ci-secrets 的 deploy-key.yml
#
# 行为：远端校验 previous_release.env 存在 → 切回该镜像 → docker compose pull/up
#       → /readyz 探活（最多约 30 秒）→ 失败退出 1，留给人工处理。

set -euo pipefail

: "${DEPLOY_HOST:?需要 DEPLOY_HOST（由 ci-secrets 的 deploy-key.yml 导入）}"
: "${SSH_KEY_B64:?需要 SSH_KEY_B64（由 ci-secrets 的 deploy-key.yml 导入）}"

command -v ssh >/dev/null 2>&1 || { apt-get update -qq && apt-get install -y -qq openssh-client; }
echo "$SSH_KEY_B64" | base64 -d > /tmp/deploy_key && chmod 600 /tmp/deploy_key

ssh -i /tmp/deploy_key -o StrictHostKeyChecking=accept-new ubuntu@${DEPLOY_HOST} "
  cd ~/memory-hub &&
  if [ ! -f previous_release.env ]; then
    echo '没有 previous_release.env：这台机器还没成功部署过，无可回滚版本'
    exit 1
  fi
  echo '回滚目标：'
  cat previous_release.env
  cp previous_release.env current_release.env &&
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
    echo '回滚后探活失败，请人工介入：docker compose ps / docker compose logs'
    exit 1
  fi
  docker compose ps
"
