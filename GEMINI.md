# Memory Hub Workspace Rules

## 1. 双远程提交与推送规范
本仓库同时推送两个远程，两端提交身份严格隔离（GitHub 在主目录提交，CNB 在 `.cnb-worktree` 提交）：

| 位置 | 分支 | 提交身份 | 推送方式 |
|---|---|---|---|
| 主目录 | `main` | `orderye <orderye@qq.com>` | `git push origin main`（GitHub 主仓） |
| `.cnb-worktree` | `cnb-main` | `orderye <3WJRP74JzJLZlOYQ8b7VcB+cnb.dgyOUWMigHA@noreply.cnb.cool>` | 裸 `git push`（upstream = `cnb/main`，触发生产部署） |

- **标准流程**：主目录提交并推 GitHub → 在 `.cnb-worktree` 执行 `git cherry-pick --no-commit <主目录 commit>` 取改动 → 重新 `git commit`（作者此时为 CNB 身份）→ `git push`。两端 hash 必然分叉、内容保持一致。
- **禁止**在主目录直接 `git push cnb main`，也禁止用 `merge --ff-only` 同步——两者都会把 GitHub 身份带到 CNB，失去隔离意义。
- 身份写在各自的 `.git/config.worktree` 与 `.git/worktrees/-cnb-worktree/config.worktree`，修改必须用 `git config --worktree`；`git -C .cnb-worktree config` 会写进共享配置并同时影响主目录。
- **Commit 信息规范（标题 + 详实内容要点）**：
  Git commit message 严禁仅写一行空洞的短标题，必须包含：
  1. 简明符合规范的首行标题（如 `feat(...)`、`fix(...)`、`docs(...)`、`ci(...)`）；
  2. 空行分隔；
  3. 详实的正文要点列表（`- ...`），逐项清楚说明涉及的核心文件、调整逻辑、契约变动或流水线动作。
- **推送说明**：每次向 CNB 远程推送时，必须清晰详细地写明更新内容：
  1. 本次更新涉及的文件列表与变更类型；
  2. 新增或修复的核心功能与架构影响；
  3. 数据库迁移（Migration）变更与兼容性说明；
  4. 质量验收与测试运行结果（`cargo test`, `cargo clippy`, `cargo fmt`）；
  5. 包含的 Commit SHA 与提交说明。

## 2. 构建与测试纪律
- 禁止运行 `cargo test --all-features`（路径包含空格会导致 `tikv-jemalloc-sys` C 编译报错）。
- 统一使用 `cargo test --workspace` 与 `cargo clippy --workspace --all-targets -- -D warnings`。
- `.cnb-worktree/` 由 `.git/info/exclude` 排除，不要提交它；部署主机等敏感信息统一走密钥仓 `toroyxxx/ci-secrets` 导入（如 `.cnb.yml` 的 `${DEPLOY_HOST}`），禁止在仓库内硬编码。
