# Memory Hub Workspace Rules

## 1. 双远程提交与推送规范

本仓库同时推送两个远程，两端**提交身份严格隔离**（GitHub 在主目录提交，CNB 在 `.cnb-worktree` 提交）：

| 位置 | 分支 | 提交身份 | 推送方式 |
|---|---|---|---|
| 主目录 | `main` | `orderye <orderye@qq.com>` | `git push origin main`（GitHub 主仓） |
| `.cnb-worktree` | `cnb-main` | CNB 专用身份（noreply 邮箱，确定性校验值见 `scripts/push-cnb.sh`） | `git -C .cnb-worktree push cnb cnb-main:main`（upstream = `cnb/main`，**触发生产部署**） |

> 完整操作手册（含凭证自检、故障排查、禁忌清单、SHA 对照留档）见
> [`docs/upgrade/PUSH_PROCESS.md`](docs/upgrade/PUSH_PROCESS.md)。本节是可执行的最小版本。

### 1.1 标准流程

```bash
# Step 1 主目录提交（GitHub 身份）
git add -A && git commit -F - <<'EOF'
feat(范围): 一句话标题

- 路径/模块: 改了什么、为什么、怎么验证的
EOF

# Step 2 推 GitHub
git push origin main

# Step 3 以 CNB 身份重放（用仓库自带脚本，内建 patch-id 口径与身份校验）
./scripts/push-cnb.sh            # 只重放并自检，不推送
./scripts/push-cnb.sh --push     # 重放 + 自检 + 推送

# Step 4 核对两端（比树，不比 SHA：两端 SHA 必然不同，内容必须一致）
git -C .cnb-worktree diff --stat cnb/main origin/main   # 必须无输出
```

**为什么必须用 `scripts/push-cnb.sh`**：两端是**两条不相交的提交历史**（作者邮箱不同 → SHA 没有任何一个相同），
判断"哪些提交还没同步"必须按 **patch-id**（改动内容指纹）判断。用 SHA 区间
（`git rev-list cnb/main..origin/main`）会把已同步过的提交再重放一遍，
结果是空提交并报 `nothing to commit`。脚本已内建正确口径：

```bash
git -C .cnb-worktree log --right-only --cherry-pick --no-merges --reverse \
  --format='%h %s' cnb/main...origin/main
```

重放的等价命令（`commit -C "$sha"` 复用原提交信息、`--reset-author` 把作者改写为 CNB 身份）：

```bash
git -C .cnb-worktree cherry-pick -n "$sha"
git -C .cnb-worktree commit -C "$sha" --reset-author
```

### 1.2 硬性约束

- **禁止**在主目录直接 `git push cnb main`，也禁止用 `merge --ff-only` 同步——两者都会把 GitHub 身份带到 CNB，失去隔离意义。
- **禁止**在主目录做 CNB 侧操作（切身份、rebase、cherry-pick）——会污染当前分支；一律在 `.cnb-worktree` 里做。
- **禁止**用 `git push --mirror`（会把实验分支等所有 ref 一并推上去，且不做身份改写）；
  **禁止**用 `git push -f` 覆盖 `cnb/main`（确需重写时用 `--force-with-lease`）。
- **禁止**把功能分支推到 CNB 镜像——CNB 侧只保留 `main` 一条分支。
- **禁止**在 GitHub 侧质量门禁未绿时推 `cnb/main`——那会直接触发生产部署。
- 身份写在各自的 `.git/config.worktree` 与 `.git/worktrees/-cnb-worktree/config.worktree`，修改必须用 `git config --worktree`；`git -C .cnb-worktree config` 会写进共享配置并同时影响主目录。
- 改身份前先确认现状：`git config --show-origin --get extensions.worktreeConfig` 应为 `true`；
  `git config user.email` 应为 `orderye@qq.com`，`git -C .cnb-worktree config user.email` 应为 CNB 邮箱。
- `~/.gitconfig` 的 `includeIf ... cnb.cool` 只在涉及该远端时生效，**不能**当切换提交身份的手段。

### 1.3 Commit 信息规范（标题 + 详实内容要点）

Git commit message 严禁仅写一行空洞的短标题，必须包含：

1. 简明符合规范的首行标题（如 `feat(...)`、`fix(...)`、`docs(...)`、`ci(...)`）；
2. 空行分隔；
3. 详实的正文要点列表（`- ...`），**以文件或模块路径开头**，逐项说明改了什么、为什么这么改、怎么校验的；
4. 涉及行为变更的要写清迁移与回滚方式。

正文里不得出现绝对路径、密钥、token 与内部账号。本仓库**不做版本号晋级**
（版本由 tag 触发 CI 构建镜像），因此提交正文**不写** `版本 A → B`。

### 1.4 推送说明

每次向 CNB 远程推送时，必须清晰详细地写明更新内容：

1. 本次更新涉及的文件列表与变更类型；
2. 新增或修复的核心功能与架构影响；
3. 数据库迁移（Migration）变更与兼容性说明；
4. 质量验收与测试运行结果（`cargo test`、`cargo clippy`、`cargo fmt`）；
5. 包含的 Commit SHA 与提交说明。

### 1.5 推前自检（30 秒）

```bash
git remote -v                                    # origin=GitHub / cnb=CNB
git log -1 --format='%ae'                        # 署名属于目标平台
git push --dry-run origin main                   # 确认将要推的对象
git log --format='%ae' -20 | sort -u             # 只有一种邮箱
```

可选安装 `pre-push` 身份拦截钩子（**不随仓库分发**，安装步骤见
[`docs/upgrade/PUSH_PROCESS.md`](docs/upgrade/PUSH_PROCESS.md) §六）——
它只挡手滑，真正的边界是"推送前在 CNB worktree 完成身份重放"。

## 2. 构建与测试纪律
- 禁止运行 `cargo test --all-features`（路径包含空格会导致 `tikv-jemalloc-sys` C 编译报错）。
- 统一使用 `cargo test --workspace` 与 `cargo clippy --workspace --all-targets -- -D warnings`。
- `.cnb-worktree/` 由 `.git/info/exclude` 排除，不要提交它；部署主机等敏感信息统一走密钥仓 `<CNB 组织>/ci-secrets` 导入（如 `.cnb.yml` 的 `${DEPLOY_HOST}`），禁止在仓库内硬编码。
