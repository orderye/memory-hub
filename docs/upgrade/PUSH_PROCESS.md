# 双平台推送流程（GitHub + CNB）· Memory Hub

本文是《双平台推送流程》通用手册在 **Memory Hub** 上的**项目化适配版**：命令、路径、身份、CI
全部换成本仓库的真实值，并补上本仓库特有的约束（Rust 工具链、镜像化发布、`.cnb.yml` 触发生产部署）。

> **与通用手册的三处关键差异**（照通用手册抄会出错）：
>
> 1. **GitHub 提交身份是 `orderye <orderye@qq.com>`**，不是通用手册假设的
>    `orderye@users.noreply.github.com`。本仓库历史一贯用前者。
> 2. **发布物是容器镜像，不是 `dist/*.zip` 安装包**。本仓库不发 GitHub Release 附件，
>    发布由 CI 构建镜像 + 打 tag 触发，因此通用手册的 §5.4 / §5.5 在本仓库**不适用**（见 §五）。
> 3. **构建与测试命令是 `cargo`**，不是 `pnpm`；且本仓库路径常含空格，
>    **禁止 `--all-features`**（见 `GEMINI.md` §2）。

---

## 〇、十秒速查

```bash
cd "/Volumes/WD Drive/项目/记忆中枢"

# 1) 远端是谁
git remote -v

# 2) 两端主线现在各是什么提交、谁署名
git log --format='%h | %ae | %s' -3 main
git log --format='%h | %ae | %s' -3 cnb/main

# 3) 身份对不对
git config user.email                     # 主工作区：orderye@qq.com
git -C .cnb-worktree config user.email    # CNB worktree：...@noreply.cnb.cool

# 4) 凭证是谁
ssh -T git@github.com                     # 期望：Hi orderye! You've successfully authenticated
gh auth status                            # 期望：github.com / account orderye
security find-internet-password -s cnb.cool | grep -E 'acct|srvr'   # cnb.cool 独立条目

# 5) 两端内容是否一致（应无输出）
git -C .cnb-worktree diff --stat cnb/main origin/main

# 6) 最近的提交有没有「标题 + 正文」
git log -1 --format='%s%n---%n%b'

# 7) 有没有待同步到 CNB 的提交（应无输出）
git -C .cnb-worktree log --right-only --cherry-pick --no-merges --reverse --format='%h %s' \
  cnb/main...origin/main
```

---

## 一、双平台架构快照

| 维度 | GitHub 侧 | CNB 侧 |
| :--- | :--- | :--- |
| 远端名 | `origin` | `cnb` |
| 远端地址 | `ssh://git@ssh.github.com:443/orderye/memory-hub.git` | `https://cnb.cool/toroyxxx/memory-hub.git` |
| 平台账号 | `orderye` | `toroyxxx` |
| 凭证来源 | SSH 私钥 `~/.ssh/id_ed25519`；`gh` 的 token 在 keychain 的 `github.com/orderye` 条目 | keychain 里 `srvr=cnb.cool` 的独立条目（密码位是 CNB 访问令牌） |
| 主线分支 | `main`（本地 `main` 跟踪 `origin/main`） | 远端只保留 `main`；本地 `cnb-main` 跟踪 `cnb/main`，实际检出在 `.cnb-worktree/` |
| 提交身份 | `orderye <orderye@qq.com>` | `orderye <3WJRP74JzJLZlOYQ8b7VcB+cnb.dgyOUWMigHA@noreply.cnb.cool>` |
| CI 入口 | `.github/workflows/ci.yml` | `.cnb.yml` |
| CI 触发 | `push` / `pull_request` / 手动 `workflow_dispatch` | `main` 分支 `push` |
| CI 覆盖 | 质量门禁 / PG 集成 / 依赖审计 / Release 构建（4 个 job） | 构建镜像 → 推制品库 → SSH 部署 → 探活回滚 |
| 发布物 | 容器镜像（`docker build`） | 同左（CNB 侧推 `docker.cnb.cool` 制品库） |
| 额外影响 | — | **推 `main` 会触发生产部署**（`tx-cloud` 主机） |

**为什么两端 SHA 不一样**：同一个提交在两个平台署名邮箱不同，commit 对象哈希就不同。
判断两端内容是否真的一致，要**比树而不是比 SHA**：

```bash
git -C .cnb-worktree diff --stat cnb/main origin/main   # 无输出 = 内容一致
```

> 两端都要推：GitHub 流水线覆盖更广（含真实 PostgreSQL 集成测试与依赖审计），
> CNB 流水线负责实际部署到生产。只推一端 = 少一层验证，或线上拿不到这批改动。

---

## 二、账户隔离模型

### 2.1 凭证层：两个平台用两套凭证

| | GitHub | CNB |
| :--- | :--- | :--- |
| 协议 | SSH（本仓库走 443 端口变体 `ssh.github.com:443`） | HTTPS + keychain |
| 凭证 | `~/.ssh/id_ed25519` | keychain 条目 `srvr=cnb.cool` |
| 生效范围 | `github.com` | 仅 `cnb.cool` |

硬性规则：

1. **不得**把 GitHub PAT 写进 `cnb.cool` 的 remote URL（反之亦然）。remote URL 里只放主机 + 路径。
2. **不得**在 URL 里内嵌 `https://user:token@host/...`。token 一旦进了 `.git/config` 就会随备份、日志、截图外泄。
3. **不得**把两个平台的 token 塞进同一个 keychain 条目或同一个 CI secret 变量名。
4. CNB 侧 token 过期只更新 keychain 的 `cnb.cool` 条目，不要"临时借用"GitHub token。

CNB 令牌也可以直接从 git credential helper 读出，供脚本使用（不落盘、不进仓库）：

```bash
CNB_TOKEN=$(git credential fill <<EOF
protocol=https
host=cnb.cool
EOF
| grep '^password=' | cut -d= -f2)
```

### 2.2 身份层：两个平台用两个提交身份

提交身份由 `user.name` / `user.email` 决定，写进 commit 对象，**平台账号与提交署名是两件事**：
推送时用的是凭证身份，commit 里留的是署名。本方案要求署名也按平台分开，
这样在任一平台上看历史，都能立刻分辨这段提交是为哪个平台写的。

本仓库用 **worktree 级配置**（一个仓库、两个工作区、各自身份），**已配好**：

```bash
git config --show-origin --get extensions.worktreeConfig   # → true
git -C .cnb-worktree config --show-origin --get user.email
```

换机器、重建 worktree 或重新 clone 时，用下面这组命令恢复（幂等）：

```bash
# 一次性设置：开启 worktree 级配置
git config extensions.worktreeConfig true

# 主工作区：GitHub 身份
git config user.name  "orderye"
git config user.email "orderye@qq.com"

# CNB worktree：CNB 身份
git -C .cnb-worktree config --worktree user.name  "orderye"
git -C .cnb-worktree config --worktree user.email "3WJRP74JzJLZlOYQ8b7VcB+cnb.dgyOUWMigHA@noreply.cnb.cool"
```

> 若 `.cnb-worktree/` 不存在，先建出来：
> `git worktree add .cnb-worktree cnb-main`（该目录已被 `.git/info/exclude` 排除）。
>
> **注意**：修改身份必须用 `git config --worktree`；`git -C .cnb-worktree config` 会写进**共享配置**
> 并同时影响主工作区。

**关于 `includeIf` 的边界**：`~/.gitconfig` 里有

```
includeIf.hasconfig:remote.*.url:https://cnb.cool/**.path=~/.gitconfig-cnb
```

它只在「命令涉及该远端」时生效（push/pull/fetch 到 `cnb.cool`），**普通 `git commit` 不会读取它**。
所以它可以兜底，但不能当作"切换提交身份"的手段——切身份请用 worktree 级配置。

### 2.3 发布物层：发布物只在受控渠道出现

- 本仓库的发布物是**容器镜像**，由 CI 构建（`Dockerfile`）并推送到各自平台的制品库
  （GitHub 侧由 Release job 构建；CNB 侧推 `docker.cnb.cool`）。**不提交进仓库**。
- `target/`、`dist/` 一类产物目录已被忽略，**不要**把构建产物提交进仓库，也不要推成 git 分支。
- 部署所需密钥（`DEPLOY_HOST`、部署私钥）只存在于 CNB 流水线从密钥仓
  `toroyxxx/ci-secrets` 导入的变量里，**禁止在仓库内硬编码**。
- GitHub 侧 `cargo-audit` 需要 `GITHUB_TOKEN`（仓库内置，无需额外配置）；
  CNB 流水线保持无密钥状态（除部署密钥外）。

---

## 三、仓库拓扑

```
主工作区（GitHub 身份）                     .cnb-worktree/（CNB 身份，已排除）
  main ──── origin/main ────► GitHub          cnb-main ──── cnb/main ────► CNB
    │                                            ▲
    │  Step 3：以 CNB 身份重放同一批提交           │
    └────────────────────────────────────────────┘
                 （提交信息不变，作者邮箱改写 → SHA 不同，内容一致）
```

- `.cnb-worktree/` 是一个 git worktree（`git worktree list` 可见），专门给 CNB 侧用，
  已被 `.git/info/exclude` 排除，不进仓库、也不影响构建产物。
- 在主工作区做 CNB 侧操作（切身份、rebase、cherry-pick）容易污染当前分支，
  这是把它单独拆成一个 worktree 的唯一原因。

---

## 四、日常推送 SOP

### Step 0 · 预检

```bash
cd "/Volumes/WD Drive/项目/记忆中枢"
git status -sb                 # 确认在预期分支、工作区干净
git fetch origin --prune
git fetch cnb --prune
```

本仓库没有 `tools/package.py` 之类的版本一致性脚本，等价的"发版前自检"是：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

> **不要**加 `--all-features`：本仓库路径常含空格，`tikv-jemallocator` 的 C 构建脚本
> 不支持带空格的路径前缀。CI 侧路径无空格，仍跑 `--all-features`（见 `GEMINI.md` §2）。

### Step 1 · 在主工作区提交（GitHub 身份）

提交信息一律 **标题 + 正文** 两段式：标题一行说清"做了什么"，正文按文件/模块逐条展开。

#### 格式模板

```
<type>(<范围>): <一句话标题>

- <文件或模块路径>: <改了什么，为什么这么改 / 怎么校验的>
- <文件或模块路径>: <改了什么>
```

| type | 用途 |
| :--- | :--- |
| `feat` | 新功能 |
| `fix` | 缺陷修复 |
| `chore` | 版本号、构建配置等杂项 |
| `docs` | 文档 |
| `refactor` | 不改变外部行为的重构 |
| `test` | 测试补充或修正 |
| `perf` | 性能优化 |
| `ci` | 流水线配置 |
| `build` | 打包与依赖 |

#### 示例

多行提交信息用 heredoc 传入（`-F -` 从标准输入读取），避免逐行 `-m` 拼接：

```bash
git add -A
git commit -F - <<'EOF'
feat(consolidation): 记忆整合 A/B 双模式与关系感知合并门槛

- crates/consolidation/src/candidates.rs: 候选抽象（关系分类 8 类、来源、稳定原因码）
- crates/consolidation/src/candidates/rules.rs: 纯本地规则候选（精确/指纹/结构/词法/环境）
- crates/consolidation/src/policy.rs: 自动合并门槛改为关系类型 + 相似度双重判定
- crates/common/src/config.rs: HUB_CONSOLIDATION_* 六项配置，启动期 fail-fast 校验
- crates/store/migrations/20261021000011_consolidation_proposal_metadata.sql: 提案溯源列（可空带默认值）
- crates/consolidation/tests/consolidation_eval.rs: 整合专属评测（错误合并数与硬约束违规数必须为 0）
EOF
```

#### 写作要点

- **标题**写"动词 + 对象"，一句话说清这一批改动做了什么；范围用模块名（如 `consolidation`、`CI`、`打包`）。
  不要写"更新代码""修复若干问题"这类无信息量的标题。
- **正文**条目以**文件或模块路径开头**（`- crates/...: ...`），逐条说明"改了什么 + 为什么 + 怎么验证"；
  涉及行为变更的要写清迁移或回滚方式。
- 条目顺序：CI → 脚本 → 配置 → 文档；一般 4–8 条，同类合并，不要逐行流水账。
- 本仓库**不做版本号晋级**：版本号是 `Cargo.toml` 的 `workspace.package.version`，
  发版由打 tag 触发 CI，不在提交里改版本号，因此提交正文**不写** `版本 A → B`。
- 正文里不要出现绝对路径、密钥、token 和内部账号。

提交后校验：

```bash
git log -1 --format='%h | %an <%ae> | %s'   # 标题与身份
git log -1 --format='%b'                      # 正文是否成段、条目是否分条
```

期望看到 `orderye <orderye@qq.com>`。**如果这里出现 CNB 邮箱，说明身份配错了**，
先 `git config user.email` 修好再继续，不要"先推了再说"。

### Step 2 · 推 GitHub

```bash
git push origin main
```

验证：

```bash
git ls-remote origin refs/heads/main
```

> 功能分支推 GitHub 时**配套开 PR**；CNB 侧不接受功能分支（见 §四 Step 4 的说明）。

### Step 3 · 以 CNB 身份重放到 CNB 侧

**用仓库自带的脚本**（它把下面的流程固化，并强制 patch-id 口径）：

```bash
./scripts/push-cnb.sh            # 只重放并自检，不推送（默认，安全）
./scripts/push-cnb.sh --push     # 重放 + 自检 + 推送
```

脚本等价于下面这套命令，**在 `.cnb-worktree` 里做，不要在主工作区做**：

```bash
git -C .cnb-worktree fetch origin main
```

**先看清哪些提交还没同步**（这一步必须用 patch-id 比对，不能用 SHA 比对）：

```bash
git -C .cnb-worktree log --right-only --cherry-pick --no-merges --reverse \
  --format='%h %s' cnb/main...origin/main
```

> 为什么不能写 `git rev-list cnb/main..origin/main`？两端是**两条不相交的提交历史**（作者邮箱不同，
> SHA 没有一个相同），SHA 区间会把它当成"CNB 侧完全没有这些提交"，从而把已同步过的提交
> 再重放一遍——重放结果是空提交，直接报 `nothing to commit`。
> `--cherry-pick` 按 **patch-id**（改动内容）判断等价，才是双身份双历史下的正确口径。
> 等价写法：`git cherry cnb/main origin/main`（`-` 表示已镜像，`+` 表示待同步）。

逐个重放（保留原提交信息与作者日期，署名换成 CNB 身份）：

```bash
git -C .cnb-worktree checkout cnb-main
git -C .cnb-worktree reset --hard cnb/main

for sha in $(git -C .cnb-worktree log --right-only --cherry-pick --no-merges --reverse --format=%H cnb/main...origin/main); do
  git -C .cnb-worktree cherry-pick -n "$sha"
  git -C .cnb-worktree commit -C "$sha" --reset-author
done
```

说明：

- `cherry-pick -n` 只把改动落到暂存区，不自动提交；`commit -C "$sha"` 复用原提交信息
  （标题 + 正文逐字保持两端一致），`--reset-author` 把作者改写为当前身份（CNB worktree 的 `user.email`）。
- 若某个提交有冲突：解决冲突 → `git add <文件>` → `git -C .cnb-worktree commit -C "$sha" --reset-author`，
  然后继续循环下一个提交。
- 循环是幂等的：没有待同步提交时列表为空，什么都不做。

重放完核对内容一致：

```bash
git -C .cnb-worktree diff --stat cnb-main origin/main     # 必须无输出
git -C .cnb-worktree log --format='%h | %ae | %s' -3
```

> 比的是**本地重放后的 `cnb-main`**，不是远端跟踪引用 `cnb/main`：重放还没推送时
> `cnb/main` 仍是旧状态，拿它比会误报"内容不一致"。

### Step 4 · 推 CNB

```bash
git -C .cnb-worktree push cnb cnb-main:main
```

验证：

```bash
git ls-remote cnb refs/heads/main
```

> CNB 侧是**镜像**，只保留 `main` 一条分支：功能分支、实验分支一律只推 GitHub，
> 不要往 CNB 推分支（推了也不会触发流水线，只会在镜像上留垃圾 ref）。
>
> ⚠️ **推 `cnb/main` 会触发 `.cnb.yml`：构建镜像 → 推制品库 → SSH 部署到生产主机 → 探活回滚。**
> 推之前确认 GitHub 侧的质量门禁已经绿灯。

推完对照两端：

| GitHub | CNB | 内容 |
| :--- | :--- | :--- |
| `a1b2c3d` | `e4f5a6b` | 一致 |

### Step 5 · 看两端 CI

| 平台 | 触发条件 | 查看方式 |
| :--- | :--- | :--- |
| GitHub | 任意分支 push、PR、手动触发 | `gh run list --workflow ci.yml --limit 5` |
| CNB | 仅 `main` 分支 push | CNB 仓库页面的流水线列表 |

推分支到 CNB 不会触发流水线——这是正常现象，CNB 侧只在 Step 4 合入 `main` 时验证。

### Step 6 · 记录 SHA 对照

两端 SHA 不同是常态，发版时按下表留档（模板见 §九）：

| 版本 | GitHub `main` SHA | CNB `main` SHA | 内容一致 |
| :--- | :--- | :--- | :--- |
| 0.1.0 | `9d7fae2` | `f367c78` | ✅ |

---

## 五、发版 SOP

本仓库的发布**由 tag 触发 CI 构建镜像**，不产出 `dist/*.zip` 这类安装包，
因此通用手册的「GitHub Release 挂分发包」与「CNB Release 附件挂载」两节**不适用**。
本仓库对应的发版动作是：

### 5.1 发版前确认

```bash
# 两端内容一致
git -C .cnb-worktree diff --stat cnb/main origin/main

# GitHub 侧质量门禁全绿（质量门禁 / PG 集成 / 依赖审计 / Release 构建 四个 job）
gh run list --workflow ci.yml --limit 5
```

### 5.2 双端提交与打 tag

先按 §四 的 Step 1–4 完成双端提交与推送，然后**分端打 tag**
（不要用一条命令把同一个 tag 推给两端，否则 CNB 侧的 tag 会带 GitHub 署名，与身份隔离原则冲突）：

```bash
# GitHub：tag 指向 GitHub 主线
git tag -a v0.1.0 main -m "v0.1.0"
git push origin v0.1.0

# CNB：同名 tag，指向 CNB 主线（同名不同 SHA，两个平台互不干扰）
git -C .cnb-worktree tag -a v0.1.0 cnb-main -m "v0.1.0"
git -C .cnb-worktree push cnb v0.1.0
```

### 5.3 发布完成核对清单

```bash
# 1. 校验双远程 commit 状态与作者标识
printf '%-16s %s\n' 'origin/main:' "$(git ls-remote origin main | cut -c1-7)"
printf '%-16s %s\n' 'cnb/main:'    "$(git ls-remote cnb main | cut -c1-7)"
git log -1 --format='%an <%ae> - %s' origin/main
git log -1 --format='%an <%ae> - %s' cnb/main

# 2. 校验双端 tag 状态
git ls-remote --tags origin "v0.1.0"
git ls-remote --tags cnb "v0.1.0"
```

**期望验收标准**：

- [ ] GitHub 与 CNB 两端分支最新 commit 内容一致，作者身份各自独立且自足；
- [ ] 双端均存在对应的 `vX.Y.Z` 标签；
- [ ] GitHub 侧 `ci.yml` 四个 job 全绿；
- [ ] CNB 侧流水线完成"部署到 tx-cloud 与探活回滚"，`/readyz` 探活通过。

---

## 六、防「推错账号」

推送前自检（30 秒）：

1. `git remote -v` —— 确认目标远端名字对得上（`origin`=GitHub / `cnb`=CNB）。
2. `git log -1 --format='%ae'` —— 确认这次要推的提交署名属于**目标平台**。
3. `git push --dry-run <remote> <branch>` —— 先看将要推的对象，确认没有意外的分支。
4. `git log --format='%ae' -20 | sort -u` —— 只有一种邮箱，且属于目标平台。

仓库自带 `scripts/push-cnb.sh` 已把第 1、2、4 步做成硬校验（身份不对直接拒绝重放）。
可选：再装一个 `pre-push` 钩子做最后一道拦截（**不随仓库分发，按需手动安装**）：

```bash
cat > .git/hooks/pre-push <<'EOF'
#!/bin/sh
# pre-push: check author emails against the target platform identity.
set -eu
remote="$1"
url="$(git remote get-url --push "$remote")"
case "${url}" in
  *github.com*) expect_mail="orderye@qq.com" ;;
  *cnb.cool*)    expect_mail="3WJRP74JzJLZlOYQ8b7VcB+cnb.dgyOUWMigHA@noreply.cnb.cool" ;;
  *) echo "pre-push: unconfigured remote ${url}, skip identity check" >&2; exit 0 ;;
esac
bad=""
while read -r local_ref local_sha remote_ref remote_sha; do
  [ "${local_sha}" = "0000000000000000000000000000000000000000" ] && continue
  mails="$(git log --format='%ae' -20 "${local_sha}" | sort -u | grep -v -x "${expect_mail}" || true)"
  [ -n "${mails}" ] && bad="${bad}${mails}
"
done
if [ -n "${bad}" ]; then
  echo "pre-push: author emails do not match target platform identity:" >&2
  echo "${bad}" >&2
  exit 1
fi
echo "pre-push: ${remote} -> ${expect_mail} identity check passed" >&2
EOF
chmod +x .git/hooks/pre-push
```

> 两个实现细节：变量一律写 `${url}` 而不是 `$url`——macOS 自带 bash 3.2 在 `$var` 紧跟中文标点时
> 会把多字节字符算进变量名，报 `unbound variable`；钩子提示语也保持英文，避免同样的坑。
> 钩子是启发式检查（看待推提交最近 20 个提交的邮箱），不是安全边界；真正的边界是"推送前在 CNB
> worktree 完成身份重放"。钩子只用来挡手滑。
>
> ⚠️ 本仓库的 CNB 历史上有一个提交（`8b7ed78`）带 GitHub 邮箱，是早期用 `cherry-pick` 未加
> `--reset-author` 留下的历史痕迹。钩子检查的是**最近 20 个提交**，不会误伤；
> 但也**不要**为此去改写 CNB 侧历史（见 §八 禁忌 5）。

---

## 七、故障排查

| 现象 | 原因 | 处理 |
| :--- | :--- | :--- |
| 推 CNB 报 `Authentication failed` | keychain 里 `cnb.cool` 条目过期或丢失 | `security find-internet-password -s cnb.cool` 确认条目在；重新登录 CNB 拿到新令牌后更新该条目。**不要**改用 GitHub token |
| 推 GitHub 报 `Permission denied (publickey)` | SSH key 没加到 `orderye` 账号 | `ssh -T git@github.com` 自检；`gh auth status` 看 token；SSH 失效时可临时用 `gh auth setup-git` 走 HTTPS + gh helper |
| `scripts/push-cnb.sh` 报 `CNB worktree identity is wrong` | worktree 级身份被改坏 | `git -C .cnb-worktree config --worktree user.email 3WJRP74JzJLZlOYQ8b7VcB+cnb.dgyOUWMigHA@noreply.cnb.cool` |
| `scripts/push-cnb.sh` 报 `has uncommitted changes` | 上一轮重放有残留 | 提交/丢弃那些改动，或加 `--force-reset` 丢弃（该 worktree 应当只是镜像） |
| `pre-push` 拦下推送 | 待推提交署名与目标平台不符 | 主工作区推 GitHub 前应是 `orderye@qq.com`；推 CNB 前应先在 `.cnb-worktree` 用 §四 Step 3 重放 |
| 重放时报 `nothing to commit` | 用 SHA 区间比对导致已同步提交被重复重放 | 改用 `--right-only --cherry-pick --no-merges`（patch-id 口径）筛选待同步提交；`scripts/push-cnb.sh` 已内建该口径 |
| 两端 `diff --stat` 有输出 | CNB 侧漏同步或同步时改了内容 | 重跑 §四 Step 3 的重放循环；必要时 `git -C .cnb-worktree reset --hard cnb/main` 后重来 |
| CNB 流水线没跑 | CNB 只在 `main` push 时触发 | 推分支不触发是正常的；合入 `main` 才会跑 |
| GitHub 流水线没跑 | 触发的是 `push` / `pull_request` / 手动 | `gh run list --workflow ci.yml` 确认；`workflow_dispatch` 的 job 需手动选后端并配好 secret |
| tag 冲突 `already exists` | 同名 tag 已在远端 | 确认没发错版本后 `git push --delete origin v0.3.6` 再打；不要用 `git push -f` 覆盖已发布 tag |
| CNB 部署后没起来 | 探活失败会自动回滚到上一稳定版本 | 看 CNB 流水线日志；`curl -fsS http://127.0.0.1:3000/readyz` 在主机上复核 |
| 本地 `cargo build` 报 tikv-jemallocator 编译错 | 路径含空格 | 不要加 `--all-features`；用 `cargo build --workspace` 或 `cargo build -p mcp-server` |

---

## 八、禁忌清单

1. 不把 GitHub PAT 写进 `cnb.cool` 的 remote URL，反之亦然。
2. 不在 remote URL 里内嵌 `https://user:token@host/...`。
3. 不让两个平台共用同一个 keychain 条目、同一个 CI secret 变量名、同一把 SSH key。
4. 不用 `git push --mirror`：它会把本地所有 ref（含实验分支、远端跟踪分支镜像）一并推上去，
   且不做身份改写。
5. 不在 CNB 侧改写 GitHub 侧的历史（会破坏 SHA 对照表）；反之亦然。
6. 不用 `git push -f` 覆盖 `cnb/main`（会丢掉 CNB 侧身份历史）；确需重写时用 `--force-with-lease`。
7. 不把 `target/`、`dist/` 等产物提交进仓库（已被忽略）。
8. 不在主工作区切身份去做 CNB 侧操作——用 `.cnb-worktree`。
9. 不在两端 SHA 不一致时"顺手"把一边强推覆盖另一边。
10. 不把功能分支推到 CNB 镜像——CNB 侧只保留 `main`。
11. 不把 GitHub 主线的 tag 对象直接推到 CNB——两端分别在自己的主线打同名 tag。
12. 不在主工作区执行 `cherry-pick` 重放——会污染当前分支。
13. 不提交只有标题、没有正文的提交——正文按文件/模块逐条写清改了什么。
14. 不在正文里写绝对路径、密钥、token 与内部账号。
15. 不在 GitHub 侧质量门禁未绿时推 `cnb/main`——那会直接触发生产部署。

---

## 九、附录

### 9.1 相关文件

| 文件 | 作用 |
| :--- | :--- |
| `GEMINI.md` | 工作区规则（含本流程的最小可执行版本与构建纪律） |
| `scripts/push-cnb.sh` | 以 CNB 身份重放并推送镜像（patch-id 口径，幂等） |
| `.cnb.yml` | CNB 流水线（构建 → 推制品库 → 部署 → 探活回滚） |
| `.github/workflows/ci.yml` | GitHub 流水线（质量门禁 / PG 集成 / 依赖审计 / Release 构建） |
| `deny.toml` | 依赖准入与安全审计策略（cargo-deny / cargo-audit 共用） |
| `docs/upgrade/` | 各专题文档（本目录） |

### 9.2 SHA 对照留档模板

| 版本 | GitHub `main` SHA | CNB `main` SHA | 内容一致 |
| :--- | :--- | :--- | :--- |
| 0.1.0 | `9d7fae2` | `f367c78` | ✅ |

### 9.3 术语

| 术语 | 含义 |
| :--- | :--- |
| patch-id | 由提交 diff 内容算出的指纹；作者邮箱不同但改动相同则 patch-id 相同，用于跨身份历史比对 |
| worktree | 同一个 git 仓库的多个独立工作区，各自持有独立分支与配置 |
| 镜像 | CNB 侧只保留 `main`、内容与 GitHub 主线一致的只读副本 |
| 重放 | 把 GitHub 主线上的提交按 CNB 身份重新提交到 CNB 侧（内容不变，署名与 SHA 变） |

[![powered by KimiCode](../../.github/assets/kimi-badge.svg)](https://www.kimi.com/code)
