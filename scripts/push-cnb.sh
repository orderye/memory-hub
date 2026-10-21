#!/usr/bin/env bash
#
# 以 CNB 身份把 GitHub 主线（origin/main）上的提交重放到 CNB 镜像（cnb/main）。
#
# 为什么需要这个脚本：两端是**两条不相交的提交历史**（作者邮箱不同 → SHA 必然不同），
# 所以「哪些提交还没同步」必须按 **patch-id**（改动内容指纹）判断，不能用 SHA 区间。
# 用 `git rev-list cnb/main..origin/main` 会把已同步过的提交再重放一遍，
# 结果是空提交并报 `nothing to commit`。
#
# 用法：
#   scripts/push-cnb.sh            # 只重放并自检，不推送（默认，安全）
#   scripts/push-cnb.sh --push     # 重放 + 自检 + 推送到 cnb/main
#   scripts/push-cnb.sh --push --force-reset   # 允许丢弃 .cnb-worktree 里的本地改动
#
# 规范细节见 docs/upgrade/PUSH_PROCESS.md。

set -euo pipefail

# ===== 参数 =====
DO_PUSH=0
FORCE_RESET=0
for arg in "$@"; do
  case "${arg}" in
    --push) DO_PUSH=1 ;;
    --force-reset) FORCE_RESET=1 ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "push-cnb: unknown argument ${arg}" >&2; exit 2 ;;
  esac
done

# ===== 常量 =====
CNB_WORKTREE=".cnb-worktree"
CNB_LOCAL_BRANCH="cnb-main"      # 本地 CNB 侧工作分支
CNB_REMOTE_BRANCH="main"         # CNB 镜像远端只保留 main 一条分支
CNB_REMOTE="cnb"
GITHUB_BRANCH="main"
EXPECT_CNB_MAIL="3WJRP74JzJLZlOYQ8b7VcB+cnb.dgyOUWMigHA@noreply.cnb.cool"

# ===== 定位仓库根 =====
REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || true)"
if [ -z "${REPO_ROOT}" ]; then
  echo "push-cnb: not inside a git repository" >&2
  exit 1
fi
cd "${REPO_ROOT}"

# ===== 前置校验 =====
if [ ! -d "${CNB_WORKTREE}" ]; then
  echo "push-cnb: ${CNB_WORKTREE} not found; create it with:" >&2
  echo "  git worktree add ${CNB_WORKTREE} ${CNB_LOCAL_BRANCH}" >&2
  exit 1
fi

if [ "$(git config --get extensions.worktreeConfig || true)" != "true" ]; then
  echo "push-cnb: extensions.worktreeConfig is not enabled" >&2
  echo "  git config extensions.worktreeConfig true" >&2
  exit 1
fi

ACTUAL_CNB_MAIL="$(git -C "${CNB_WORKTREE}" config user.email || true)"
if [ "${ACTUAL_CNB_MAIL}" != "${EXPECT_CNB_MAIL}" ]; then
  echo "push-cnb: CNB worktree identity is wrong" >&2
  echo "  expected: ${EXPECT_CNB_MAIL}" >&2
  echo "  actual:   ${ACTUAL_CNB_MAIL}" >&2
  echo "  fix with:" >&2
  echo "  git -C ${CNB_WORKTREE} config --worktree user.email ${EXPECT_CNB_MAIL}" >&2
  exit 1
fi

# 工作区必须干净：脏的工作区会让重放结果不可预期
if [ -n "$(git -C "${CNB_WORKTREE}" status --porcelain)" ]; then
  if [ "${FORCE_RESET}" -eq 1 ]; then
    echo "push-cnb: --force-reset given, discarding local changes in ${CNB_WORKTREE}"
  else
    echo "push-cnb: ${CNB_WORKTREE} has uncommitted changes" >&2
    echo "  commit them, or re-run with --force-reset to discard them" >&2
    exit 1
  fi
fi

echo "== push-cnb: 同步远端 =="
git fetch origin --prune
git -C "${CNB_WORKTREE}" fetch "${CNB_REMOTE}" --prune

# ===== 计算待同步提交（patch-id 口径） =====
PENDING="$(git -C "${CNB_WORKTREE}" log \
  --right-only --cherry-pick --no-merges --reverse \
  --format='%H' "${CNB_REMOTE}/${CNB_REMOTE_BRANCH}...origin/${GITHUB_BRANCH}")"

if [ -z "${PENDING}" ]; then
  echo "== push-cnb: 没有待同步提交（两端内容已一致） =="
else
  echo "== push-cnb: 待同步提交 =="
  git -C "${CNB_WORKTREE}" log \
    --right-only --cherry-pick --no-merges --reverse \
    --format='  %h | %s' "${CNB_REMOTE}/${CNB_REMOTE_BRANCH}...origin/${GITHUB_BRANCH}"
fi

# ===== 重放 =====
if [ -n "${PENDING}" ]; then
  git -C "${CNB_WORKTREE}" checkout "${CNB_LOCAL_BRANCH}"
  git -C "${CNB_WORKTREE}" reset --hard "${CNB_REMOTE}/${CNB_REMOTE_BRANCH}"

  for sha in ${PENDING}; do
    echo "== push-cnb: replay ${sha} =="
    if ! git -C "${CNB_WORKTREE}" cherry-pick -n "${sha}"; then
      echo "push-cnb: conflict while replaying ${sha}" >&2
      echo "  resolve in ${CNB_WORKTREE}, then: git add <files> && git -C ${CNB_WORKTREE} commit -C ${sha} --reset-author" >&2
      exit 1
    fi
    # -C 复用原提交信息（标题 + 正文逐字保持两端一致）；--reset-author 把作者改写为 CNB 身份
    git -C "${CNB_WORKTREE}" commit -C "${sha}" --reset-author
  done
fi

# ===== 自检：两端内容必须一致（比树，不比 SHA） =====
# 注意比的是**本地重放后的分支**（cnb-main）而不是远端跟踪引用（cnb/main）：
# 重放还没推送时，cnb/main 仍是旧状态，拿它比会误报"内容不一致"。
echo "== push-cnb: 内容一致性自检 =="
DIFF_STAT="$(git -C "${CNB_WORKTREE}" diff --stat "${CNB_LOCAL_BRANCH}" origin/${GITHUB_BRANCH})"
if [ -n "${DIFF_STAT}" ]; then
  echo "push-cnb: 两端内容不一致，请检查：" >&2
  echo "${DIFF_STAT}" >&2
  exit 1
fi
echo "  内容一致（${CNB_LOCAL_BRANCH} 与 origin/${GITHUB_BRANCH} 的 diff --stat 无输出）"

echo "== push-cnb: CNB 侧最近提交 =="
git -C "${CNB_WORKTREE}" log --format='  %h | %ae | %s' -3

# ===== 推送 =====
if [ "${DO_PUSH}" -eq 1 ]; then
  echo "== push-cnb: 推送 ${CNB_LOCAL_BRANCH} -> ${CNB_REMOTE}/${CNB_REMOTE_BRANCH} =="
  git -C "${CNB_WORKTREE}" push "${CNB_REMOTE}" "${CNB_LOCAL_BRANCH}:${CNB_REMOTE_BRANCH}"
  echo "== push-cnb: 完成 =="
  printf '%-16s %s\n' "origin/${GITHUB_BRANCH}:" "$(git ls-remote origin "refs/heads/${GITHUB_BRANCH}" | cut -c1-7)"
  printf '%-16s %s\n' "${CNB_REMOTE}/${CNB_REMOTE_BRANCH}:" "$(git ls-remote "${CNB_REMOTE}" "refs/heads/${CNB_REMOTE_BRANCH}" | cut -c1-7)"
else
  echo "== push-cnb: 重放完成，未推送（加 --push 可推送） =="
fi
