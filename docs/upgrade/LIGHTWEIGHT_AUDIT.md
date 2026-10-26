# Memory Hub 轻量化部署审计报告（LIGHTWEIGHT_AUDIT）

**审计日期：** 2026-10-21  
**审计对象：** 当前检出工作区 `/Volumes/WD Drive/项目/记忆中枢`  
**审计基线：** 分支 `feat/lightweight-deployment`，HEAD `3048efc75688c5c6cc1ceb0e06891587b2f3f1cf`  
**任务依据：** `docs/ Memory Hub 轻量化升级完整执行任务书.md` 的 P0.2（Workspace 与 Features）与 P0.3（数据库迁移）  
**审计性质：** 严格只读。仅使用 read / grep / glob 与只读 shell（`ls`、`sed -n`、`git log`、`git show`、`git status`）。未执行任何 `cargo build/test/clippy/fmt`（另有一个 release 构建占用 `target/` 锁），未修改任何 Rust/YAML/JSON/SQL 文件。本次唯一写入的文件就是本报告。

> **证据口径**：本报告每条结论都指向 `文件:行号` 或符号名。凡是标注「未实测」「未确认」的项，均未在本机执行或无法在无 PostgreSQL / 无 Docker 的环境下判定——**不根据编号、注释或旧文档推断结论**。

---

## 0. Git 基线、审计边界与环境

### 0.1 实取基线

| 项目 | 实取值 | 证据 |
|---|---|---|
| 当前分支 | `feat/lightweight-deployment` | `git rev-parse --abbrev-ref HEAD` |
| HEAD commit | `3048efc75688c5c6cc1ceb0e06891587b2f3f1cf` | `git rev-parse HEAD` |
| HEAD 时间/标题 | 2026-10-21 17:33:25 +0800 / `docs: 为全部面向用户的文档添加 KimiCode 徽章` | `git log -1 --format=…` |
| 与远程的关系 | `main`、`origin/main`、`origin/HEAD` 均指向同一 commit | `git log -10 --oneline --decorate` |
| 工作区未跟踪文件（审计起始时） | `docs/ Memory Hub 轻量化升级完整执行任务书.md` | 起始 `git status --porcelain` |

近 10 条提交（`git log -10 --oneline`）：

```text
3048efc docs: 为全部面向用户的文档添加 KimiCode 徽章
15a76f6 docs(readme): 整理 README 并新增全英文版本（默认显示中文）
4567387 fix(deps): 升级 rustls-webpki 与 webpki-roots 修复 RUSTSEC-2026-0381/0382
a3a7da6 fix(push): 重放自检比本地 cnb-main 而非远端跟踪引用 cnb/main
fcf76e8 docs(push): 双平台推送流程项目化适配与 patch-id 重放脚本
e3b5bd0 feat(consolidation): 记忆整合 A/B 双模式与关系感知自动合并门槛
9d7fae2 fix(security): 消除 GitHub secret scanning 误报的凭证字面量并加门禁
7111f8c feat(retrieval,core,mcp): 语义能力显式化 + 默认拒绝阈值 + 证据写入前脱敏
d22ce16 fix(ci): 修复 GitHub Actions 三处门禁失败
864a563 docs(readme): 全面重写 README 改用通俗语言并同步最新能力与工具集
```

### 0.2 审计期内工作区被并行改动（重要边界）

审计进行中（约 17:54 → 18:05），工作区出现了**另一条并行工作线（轻量化实现）的未提交改动**。这不是本次审计产生的，也不属于本报告的审计基线：

| 状态 | 路径 | 说明 |
|---|---|---|
| ` M` | `crates/common/src/config.rs` | 新增 `DeploymentProfile` 相关字段与 `defaults_for()` |
| ` M` | `crates/common/src/lib.rs` | 新增 `pub mod profile` 与再导出 |
| ` M` | `crates/embed/src/lib.rs` | 新增 `DisabledEmbedder` 与 `EmbedderKind::Off` |
| ` M` | `crates/mem-core/src/infra.rs` | `Embedder` trait 新增 `is_enabled()` |
| `??` | `crates/common/src/profile.rs` | 新文件（`DeploymentProfile`） |
| `??` | `scripts/lightweight_probe.py` | 新文件 |
| `??` | `docs/ Memory Hub 轻量化升级完整执行任务书.md` | 任务书本身 |

**因此，本报告所有行号均锚定在 HEAD `3048efc` 的 blob 上**，而不是「当时的磁盘文件」。复现方式：

```bash
git show HEAD:crates/common/src/config.rs | sed -n '278p'
#             consolidation_mode: "rules".to_string(),
```

报告引用的 4 个被改动文件中的关键锚点，已逐条用 `git show HEAD:<path> | sed -n '<n>p'` 复核（见 §2、§3、§4 的引用）。审计结论对**未提交的轻量化改动不生效**：那些改动不在本次基线内，也不在 `MIGRATIONS`、备份格式、MCP 工具集等既有契约的审计范围内。

> 说明：上表是审计当时（约 18:05）的脏文件快照。并行实现随后继续扩展（工作区到本报告交付时已触及 `crates/mcp/src/transport.rs`、`crates/mcp/src/tools.rs`、`crates/worker/src/*`、`crates/retrieval/src/hybrid.rs` 等更多文件，并新增 `crates/worker/src/lease.rs`、`docker-compose.lite.yml` 等）。报告不做增量跟踪；**所有结论仍以 HEAD `3048efc` 为准**，需要判断「改后是否已闭环」时应重跑本报告的 grep/git show 复核清单。

### 0.3 本机环境（实测）

| 项目 | 实测值 | 命令 |
|---|---|---|
| rustc | `1.98.1 (48a229cea 2026-09-01)` | `rustc --version` |
| cargo | `1.98.1 (797e8a9bc 2026-08-05)` | `cargo --version` |
| 架构 / 系统 | `arm64` / macOS `27.0.1`（Build 26A434） | `uname -m`、`sw_vers` |
| 工作目录 | `/Volumes/WD Drive/项目/记忆中枢`（**含空格**） | `pwd` |
| Docker | **不存在**（`command -v docker` 无输出） | `command -v docker` |
| psql / pg_isready | **不存在** | `command -v psql`、`command -v pg_isready` |
| 5432 监听 | **无监听**（`lsof -nP -iTCP:5432 -sTCP:LISTEN` 空；`nc -z 127.0.0.1 5432` 失败） | 实测 |
| 现有 release 产物 | `target/release/` 下 mcp-server 17,757,888 B、auth-server 11,418,368 B、backup-worker 11,168,336 B（17:55 生成） | `ls -l target/release` |

### 0.4 本次明确未执行的检查（及原因）

任务书 P0.2 要求执行 `cargo metadata` / `cargo tree` 系列命令。本次**未执行**，原因有二，均记录在案：

1. 另一条 release 构建正在占用 `target/`（`target/release` 下的 cargo 锁文件存在，产物时间戳 17:55），并发 cargo 会抢 target 锁并污染性能基线；
2. `GEMINI.md:102` 明确禁止在本机运行 `--all-features`：

```text
GEMINI.md:102  禁止运行 cargo test --all-features（路径包含空格会导致 tikv-jemalloc-sys C 编译报错）。
GEMINI.md:103  统一使用 cargo test --workspace 与 cargo clippy --workspace --all-targets -- -D warnings。
```

**本报告不含任何测试运行结果。** 本次没有执行 `cargo test` / `cargo metadata` / `cargo tree` / `cargo clippy` / `cargo fmt`，也没有启动任何二进制：target 目录由并行的 release 构建占用，且运行会把它正在修改的源码混入「基线」。需要运行结论的项，统一写为「未执行（由 Lead 在其代码修改完成后统一执行）」。文中的测试数量是源码静态计数（见 §13 口径说明），不是运行输出。

作为替代证据：feature 与依赖闭包一律从各 `Cargo.toml` 与 `Cargo.lock` 原文读取（§2），不依赖 `cargo tree` 输出。`cargo tree -i tikv-jemallocator` 的真实结果因此**未取得**，报告中以 `Cargo.lock` 的依赖边作为等价证据，并标注为「未执行命令的替代证据」。

---

## 1. Workspace：14 个成员与依赖方向

`Cargo.toml:1-18` 的 `[workspace].members` 实际列出 14 项，与磁盘目录一一对应（`ls crates` + `ls crates/bins`）：

| # | 成员（包名） | 路径 | 内部依赖（读自各自 `Cargo.toml` 的 `[dependencies]`） | 证据行 |
|---|---|---|---|---|
| 1 | `common` | `crates/common` | **无**（仅第三方：thiserror/serde/sha2/reqwest/tokio 等） | `crates/common/Cargo.toml:8-24` |
| 2 | `mem-core` | `crates/mem-core` | `common` | `crates/mem-core/Cargo.toml:9` |
| 3 | `embed` | `crates/embed` | `common`, `mem-core` | `crates/embed/Cargo.toml:10-11` |
| 4 | `store` | `crates/store` | `common`, `mem-core` | `crates/store/Cargo.toml:9-10` |
| 5 | `retrieval` | `crates/retrieval` | `common`, `mem-core`, `embed` | `crates/retrieval/Cargo.toml:9-11` |
| 6 | `authz` | `crates/authz` | `common`, `mem-core` | `crates/authz/Cargo.toml:9-10` |
| 7 | `auth-core` | `crates/auth-core` | `common`, `mem-core`, `authz` | `crates/auth-core/Cargo.toml:10-12` |
| 8 | `auth-web` | `crates/auth-web` | `common`, `mem-core`, `auth-core`, `authz`, `store` | `crates/auth-web/Cargo.toml:9-13` |
| 9 | `mcp` | `crates/mcp` | `common`, `mem-core`, `consolidation`, `authz` | `crates/mcp/Cargo.toml:10-13` |
| 10 | `consolidation` | `crates/consolidation` | `common`, `mem-core`, `store` | `crates/consolidation/Cargo.toml:9-11` |
| 11 | `worker` | `crates/worker` | `common`, `mem-core`, `consolidation`, `store`, `embed` | `crates/worker/Cargo.toml:9-13` |
| 12 | `mcp-server` | `crates/bins/mcp-server` | `common`, `mem-core`, `store`, `embed`, `retrieval`, `auth-core`, `authz`, `consolidation`, `mcp` | `crates/bins/mcp-server/Cargo.toml:14-23` |
| 13 | `auth-server` | `crates/bins/auth-server` | `common`, `mem-core`, `store`, `auth-core`, `auth-web`, `authz` | `crates/bins/auth-server/Cargo.toml:12-19` |
| 14 | `backup-worker` | `crates/bins/backup-worker` | `common`, `mem-core`, `consolidation`, `store`, `embed`, `worker` | `crates/bins/backup-worker/Cargo.toml:12-19` |

**依赖方向（由 `[dependencies]` 边还原，无环）**：

```text
第 1 层  common
第 2 层  mem-core
第 3 层  embed / store / authz
第 4 层  retrieval / consolidation / auth-core
第 5 层  worker / mcp / auth-web
第 6 层  mcp-server / auth-server / backup-worker
```

- **无循环依赖风险**：`common` 是唯一叶子；`mem-core` 只依赖 `common`（`crates/mem-core/Cargo.toml:9`，无 `store`/`consolidation`，即核心 crate 不反向依赖实现）；三个 bin 位于最后一层，未被任何库依赖。
- **一处层次倒挂（非环）**：`consolidation` 直接依赖具体存储 crate `store`（`crates/consolidation/Cargo.toml:11`），而它对外使用的抽象是 `mem_core::MemoryHubStore`（`crates/consolidation/src/engine.rs:59`）。这意味着 `consolidation` 无法脱离具体存储 crate 单独编译。是否要在轻量化中消除，属设计取舍，本报告只记录事实。
- `retrieval` 的 `store` 仅在 `[dev-dependencies]`（`crates/retrieval/Cargo.toml:23`），生产路径不绑定具体存储。
- 测试侧反向依赖：`mcp` 的 dev-deps 含 `store`、`retrieval`（`crates/mcp/Cargo.toml:30-31`）。

---

## 2. Features：实际内容、默认值、依赖闭包

### 2.1 每个 crate 的 `[features]` 原文（未声明者即无 `[features]` 段）

| crate | `[features]` 原文 | 默认 feature | 证据 |
|---|---|---|---|
| `store` | `default = []`；`pg-integration = []` | `[]`（空） | `crates/store/Cargo.toml:29-31` |
| `mcp` | `default = ["adapter-cjs"]`；`adapter-cjs = []` | `adapter-cjs` | `crates/mcp/Cargo.toml:24-26` |
| `mcp-server` | `default = ["adapter-cjs"]`；`adapter-cjs = ["mcp/adapter-cjs"]`；`jemalloc = ["dep:tikv-jemallocator"]` | `adapter-cjs` | `crates/bins/mcp-server/Cargo.toml:8-11` |
| `auth-server` | `default = []`；`jemalloc = ["dep:tikv-jemallocator"]` | `[]` | `crates/bins/auth-server/Cargo.toml:8-10` |
| `backup-worker` | `default = []`；`jemalloc = ["dep:tikv-jemallocator"]` | `[]` | `crates/bins/backup-worker/Cargo.toml:8-10` |
| 其余 9 个 crate | **没有 `[features]` 段** | 不适用 | `crates/common`、`mem-core`、`embed`、`retrieval`、`authz`、`auth-core`、`auth-web`、`consolidation`、`worker` 各自的 Cargo.toml |

结论：整个 workspace 只有 **3 个 feature 名字**——`jemalloc`、`adapter-cjs`、`pg-integration`。不存在部署形态类 feature。

### 2.2 `jemalloc`：哪三个 bin 有、依赖闭包是什么

- **三个 bin 都有，且都是可选依赖**：`mcp-server`（`Cargo.toml:11,29`）、`auth-server`（`:10,24`）、`backup-worker`（`:10,23`）。
- 使用点（`#[cfg]` 全局分配器）：

| 文件 | 行 | 内容 |
|---|---|---|
| `crates/bins/mcp-server/src/main.rs` | 24-29 | `#[cfg(all(feature = "jemalloc", not(target_env = "msvc")))]` + `#[global_allocator] static GLOBAL: Jemalloc` |
| `crates/bins/auth-server/src/main.rs` | 22-27 | 同上 |
| `crates/bins/backup-worker/src/main.rs` | 22-27 | 同上 |

- **依赖闭包**（`Cargo.lock` 实读，替代未能执行的 `cargo tree -i`）：

```text
Cargo.lock:2317  name = "tikv-jemallocator"  version = "0.6.1"
Cargo.lock:2323    dependencies = ["libc", "tikv-jemalloc-sys"]
Cargo.lock:2307  name = "tikv-jemalloc-sys" version = "0.6.1+5.3.0-1-ge13ca993e8ccb9ba…"
Cargo.lock        dependencies = ["cc", "libc"]     ← 需要 C 编译器构建 jemalloc 本体
```

- **本机不可编译的真实原因**：`tikv-jemalloc-sys` 的 autotools `configure` 拒绝含空格的前缀（`configure: error: Prefix should not contain spaces`），而工作区路径含空格；这正是 `GEMINI.md:102` 禁止 `--all-features` 的原因。默认 feature 集合不含 `jemalloc`，因此 `cargo test --workspace` 正常；而 CI 的 `--all-features` 只在 `ubuntu-latest`（路径无空格）上可行（`.github/workflows/ci.yml:36,39,42`）。
- 注意：`--all-features` 在**本机**不可行、在 CI 可行；因此「CI 通过」不能推出「本机可编译全 feature」。

### 2.3 `adapter-cjs`：默认开启的防腐中间件

| 位置 | 行 | 作用 |
|---|---|---|
| `crates/mcp/Cargo.toml` | 25 | `default = ["adapter-cjs"]`（**默认开启**） |
| `crates/mcp/src/lib.rs` | 17 | `#[cfg(feature = "adapter-cjs")] pub mod adapter;` |
| `crates/mcp/src/adapter/mod.rs` | 6 | `#[cfg(feature = "adapter-cjs")]` 门控 `cjs` 子模块 |
| `crates/bins/mcp-server/src/main.rs` | 229-240 | feature 开启且 `CjsAdapterConfig::from_env().enabled` 为真时注入 `CjsAdapter`，否则 `interceptor = None` |
| `crates/bins/mcp-server/Cargo.toml` | 10 | `adapter-cjs = ["mcp/adapter-cjs"]`（透传） |

`adapter-cjs` 的依赖闭包为空（`= []`），只做代码门控；关闭后 `crates/mcp/src/adapter/` 整层（约 900 行）不参与编译。

### 2.4 `pg-integration`：仅门控测试

| 位置 | 行 | 说明 |
|---|---|---|
| `crates/store/Cargo.toml` | 31 | `pg-integration = []`（无依赖） |
| `crates/store/src/postgres.rs` | 3173 | `#[cfg(all(test, feature = "pg-integration"))]` 包住真实 PG 集成测试模块 |
| `crates/store/src/idempotency.rs` | 147 | `#[cfg(all(test, feature = "pg-integration"))]` |
| `.github/workflows/ci.yml` | 88 | `cargo test -p store --features pg-integration -- --ignored --nocapture --test-threads=1` |

即：`pg-integration` **不改变任何生产代码**，只让「需要真实数据库」的测试参与编译与运行。

### 2.5 默认 feature 的最终形态

- `cargo test --workspace`（GEMINI.md 指定口径）实际编译的 feature 集合 = 每个包的 `default`：`mcp`/`mcp-server` 带 `adapter-cjs`，其余不带任何 feature；**不含 jemalloc、不含 pg-integration**。
- 生产镜像与 CI release 构建额外打开 3 个 bin 的 `jemalloc`（`Dockerfile:56`、`.github/workflows/ci.yml:151-153`）。当前没有任何 feature 级开关可以关掉它。

---

## 3. Embedding：trait、实现点、Provider 创建位置、缓存与失败行为

### 3.1 `Embedder` trait（`crates/mem-core/src/infra.rs:555-574`）

| 成员 | 行 | 语义 |
|---|---|---|
| `fn model_id(&self) -> &str` | 556 | 向量空间标识；跨模型向量不可比 |
| `fn dim(&self) -> usize` | 557 | 维度（PG 侧必须与 `vector(1024)` 一致） |
| `async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>` | 558 | 批量嵌入 |
| `fn provides_semantic_signal(&self) -> bool` | 571-573 | **默认 `true`**。伪向量实现返回 `false`，调用方据此把向量维度排除出打分、并声明「语义未就绪」 |

`provides_semantic_signal` 默认值 `true` 的后果：任何新实现若忘记覆盖，系统会把它当成真实语义源。目前显式返回 `false` 的有 3 处：`crates/embed/src/lib.rs:91`（`MockEmbedder`）、`crates/mem-core/src/testing.rs:68`（`TestEmbedder`）、`crates/retrieval/src/hybrid.rs:850`（测试内实现）；`testing.rs:138` 的 `SemanticTestEmbedder` 返回 `true`，其注释（`testing.rs:85`）明确写着「返回 true 只是为了让测试能进入向量模式分支」。

### 3.2 全部实现点（`impl Embedder for …`）

| 实现 | 位置 | 生产/测试 | 说明 |
|---|---|---|---|
| `MockEmbedder` | `crates/embed/src/lib.rs:23,70` | 生产可选项（`mock`） | SHA-256 字节 → 确定性伪向量 + L2 归一化（`generate_vector`，`:43-66`）；`provides_semantic_signal=false`（`:91`） |
| `HttpEmbedder` | `crates/embed/src/http.rs:37,86` | 生产（`http`） | OpenAI 兼容 `/embeddings`；reqwest 客户端 10s 超时（`:55-57`）；校验返回条数（`:131-137`）与维度（`:141-147`），不符即 `Internal` 错误 |
| `TestEmbedder` | `crates/mem-core/src/testing.rs:53` | 测试 | 伪向量，`false` |
| `SemanticTestEmbedder` | `crates/mem-core/src/testing.rs:124` | 测试 | `true`（仅测试） |
| `CountingEmbedder` | `crates/mem-core/src/testing.rs:192` | 测试 | 包装器，统计 `call_count()/text_count()`，用于证明「B 模式零调用」 |
| `SemanticTestEmbedder`（另一份） | `crates/retrieval/src/hybrid.rs:837` | 测试 | 检索单测内私有实现 |
| `FailingEmbedder` | `crates/retrieval/src/hybrid.rs:1305`、`crates/retrieval/tests/eval_suite.rs:146` | 测试 | 模拟嵌入失败 |
| `CachedEmbedder` | `crates/retrieval/tests/eval_bench.rs:1246` | **仅评测** | 磁盘缓存，用于重复跑评测；**不在生产路径** |

### 3.3 Provider 创建位置：两个 bin 调用，auth-server 不调用

| 二进制 | 是否构建 Embedder | 证据 | 条件 |
|---|---|---|---|
| `mcp-server` | **无条件** | `crates/bins/mcp-server/src/main.rs:50` 的 `embed::build_from_config(&cfg)?` | 无——无论 mock/http 都必须成功构建，失败即启动失败 |
| `backup-worker` | **仅 hybrid 模式** | `crates/bins/backup-worker/src/main.rs:73-86` 的 `if consolidation_mode == ConsolidationMode::Hybrid { match embed::build_from_config(&cfg) { … } }` | 默认 `rules` 下**一次都不构建**（这正是「B 模式零嵌入」的装配层保障） |
| `auth-server` | **否** | `crates/bins/auth-server/src/main.rs:42` 只有 `AppConfig::from_env()`；全文件无 `embed::` / `Embedder` 引用（grep 验证） | — |

> 与任务书参考事实的差异：任务描述为「三个二进制各自的调用」。实测**只有 2 个 bin 调用 Provider**，`auth-server` 完全不涉及嵌入。这符合其职责（OAuth/账户控制台），但审计必须按代码写。

`build_from_config`（`crates/embed/src/lib.rs:122-154`）只接受两个分支（`http` / `mock`），未知值返回「未知 HUB_EMBED_PROVIDER: {other}（支持 mock | http）」（`:150-152`）。`AppConfig` 侧再做同义校验：`crates/common/src/config.rs:542`、`:553`。**不支持第三值**（例如关闭嵌入的 `off`）。

`build_embedder(kind, …)`（`crates/embed/src/lib.rs:161-180`）是另一条构造入口，同样拒绝 http 无 endpoint；它在生产 bin 里未被调用（grep 命中仅 `embed` 自身及其单测）。

### 3.4 缓存：生产路径没有任何嵌入缓存

- 全仓 grep `cache`（`crates ` 下所有 .rs）的唯一非测试命中是 `crates/mcp/src/protocol.rs:93` 的 JSON 字段 `cacheScope`（协议声明，不是缓存实现）。
- `DefaultMemoryService::embed_content`（`crates/mem-core/src/memory_service.rs:243-252`）每次调用都直接 `self.embedder.embed(&[content.to_string()])`，无查询、无去重、无 TTL。
- 唯一的缓存实现是评测专用 `CachedEmbedder`（`crates/retrieval/tests/eval_bench.rs:1246`），不进生产二进制。

### 3.5 失败行为

| 场景 | 行为 | 证据 |
|---|---|---|
| 写入时嵌入失败 | **静默降级**：返回 `(None, None)`，条目照常落库但无向量；不告警、不计失败 | `memory_service.rs:243-252` |
| 写入路径调用点 | `put` 的 upsert 分支 `:285`、新建分支 `:304`、`update` 分支 `:437` —— 每次写入都算一次嵌入 | 同上 |
| 查询嵌入失败 | 记 `warn!`，向量路不可用，仅关键词召回（不失败整个查询） | `crates/retrieval/src/hybrid.rs:536-543` |
| 查询嵌入超时 | 记 `warn!`（带 `timeout_ms`），同上降级 | `hybrid.rs:544-551` |
| 查询嵌入返回空数组 | `warn!` + 视作无向量 | `hybrid.rs:526-535` |
| 融合侧缺向量 | 按「信号不可用」重新归一化权重，而不是按 0 压低全部分数 | `hybrid.rs:371-414` + `algorithm.rs:231-250` |
| 整合合并后重嵌入失败 | `warn!` 并**清空**该条目向量（宁可少一路召回，不留错向量） | `crates/consolidation/src/engine.rs:637-652` |

---

## 4. Retrieval：`search` 的真实执行顺序与打分细节

所有结论来自 `crates/retrieval/src/hybrid.rs`（1842 行）与 `crates/mem-core/src/algorithm.rs`（589 行）。

### 4.1 `Retriever::search` 步骤（`hybrid.rs:493-598`）

| # | 步骤 | 行号 | 关键事实 |
|---|---|---|---|
| 0 | `top_k = max(1)` | 494 | — |
| 1 | **空查询短路**：直接 `fallback_candidates` → 打分 → **只**应用调用方显式 `min_score` | 497-510 | 空查询不适用默认拒绝阈值（注释：否则「列出全部」会被误判不可靠） |
| 2 | 计算本次生效阈值 `effective_min_score` | 513 | 见 §4.4 |
| 3 | **建立统一 deadline** | 517 | `Instant::now() + soft_timeout`（默认 500ms，`config.rs:269`） |
| 4 | **查询嵌入（单次）**，受 deadline 约束 | 520-552 | 只嵌 `q.query_text` 一次，双路与打分共用；结果 `l2_normalize` |
| 5 | **FTS 与向量两路并行**，各自 `timeout_at(deadline)` | 555-558 | `tokio::join!`；超时只丢超时的那一路（`:559` `path_timed_out`） |
| 6 | **合并去重** | 560-561 → `merge_candidates` 299-317 | 先放向量路（score = 相似度），再补 FTS 路（`None`）；以 `item.id` 去重 |
| 7 | **软超时兜底**：两路都超时且合并为空 → `fallback_candidates`（纯列表召回） | 563-576 | 仅在 `merged.is_empty() && path_timed_out` 时触发；非空时保留已完成的一路 |
| 8 | **五维打分 + min_score + MMR**，整体移入 `spawn_blocking` | 585-595 | 打分是 CPU 密集（O(n·dim) + 池内 IDF） |
| 9 | `min_score` **在 MMR 之前**过滤 | 587-591 | 注释明写回归原因：先 MMR 再过滤会导致结果数少于高阈值下本可返回的数量 |
| 10 | **MMR 重排（无条件）** | 592 | 见 §4.5 |

候选池规模：FTS 路 `limit = fts_k.max(top_k)`（`hybrid.rs:201`，默认 `fts_k=100`，`config.rs:260`）；向量路 `k = vec_k.max(top_k * 2)`（`hybrid.rs:244`，默认 `vec_k=100`，`config.rs:261`）。

### 4.2 两路召回与过滤下推

| 函数 | 行 | 行为 |
|---|---|---|
| `fts_candidates` | 185-216 | 用 `store.list(MemoryFilter{ text_contains: Some(query_text), … })`；失败 `warn!` 后返回空（不静默） |
| `vec_candidates` | 219-265 | 用 `store.similar(filter, qv, k)`；**过滤条件下推到存储层**（`kind`/`tags`/`statuses`/`scene`/`layer`/`valid_at`），再做一次防御性复核（`:256-264`） |
| `statuses_for` | 268-274 | 默认只 `Active`；`include_archived` 或 `include_superseded` 才加 `Archived` |
| `fallback_candidates` | 277-296 | 纯列表，`limit = top_k.max(1)` |
| `scene_pushdown` | 149-154 | `Strict` 才把 scene 下推；`Prefer` 不下推（改由打分降权） |

### 4.3 `score_items`：五维打分与 `sim_available` 归一化（`hybrid.rs:320-444`）

| 维度 | 计算 | 行 |
|---|---|---|
| 池内 IDF | `extract_terms` + `document_tokens` 求 `doc_freq`，`inverse_document_frequency(pool_size, df)` | 333-354 |
| `kw` | `keyword_score_weighted(&tokens, &terms, &idf)`（词元集合命中，非裸子串，避免 api 命中 rapid） | 381 |
| `sim` | 有可用向量时：向量路得分 `vec_sim`，或现算 `cosine_similarity(qv, e)`；否则 `None` | 371-380 |
| `rec` | `recency_score(reference_time, item.occurred_at.unwrap_or(updated_at), tau)`；`tau` 按 `kind`（`recency_tau_days_for`） | 383-394 |
| `prior` | `layer_prior(item.layer)`（认知层级先验，取代旧的「版本新鲜度」） | 395 |
| 可用性向量 | `[sim_available, true, true, true, true]`，其中 `sim_available = self.semantic_ready && sim.is_some()` | 403 |
| 合成 | `calculate_item_score_with_availability(…, weights, available)` = Σwᵢsᵢ／Σ可用 wᵢ | 404-414；`algorithm.rs:231-250` |
| 场景加成 | Strict 命中 +0.05（`SCENE_PREFER_BONUS`）；Prefer 走 `scene_preference_score`（+/-0.05） | 417-428；`algorithm.rs:312-336` |
| 演化回溯折扣 | `is_superseded_at(item, reference_time)` 时 `score *= 0.5` | 431-433 |
| 权重 | `weights_for(q)`：分场景 profile 优先，否则全局 `[0.50,0.20,0.10,0.10,0.10]` | 162-166；`config.rs:265` |

**`sim_available` 的关键语义**（`hybrid.rs:396-403` 的注释即规范）：

1. 伪向量嵌入器（`provides_semantic_signal()==false`）时 `semantic_ready=false` → `sim_available=false` → **sim 维度权重被剔除、其余四维重新归一化**，同时配置权重本身不改（换真实模型后自动恢复）。
2. 嵌入失败 / 条目缺向量 / 模型不匹配（`embedding_usable`，`:176-182` 比对 `embedding_model == embedder.model_id()`）同样按「不可用」处理，避免全部分数被整体压低、`min_score` 阈值失真。
3. 归一化分母为 0 时返回 0.0（`algorithm.rs:245-249`）。

### 4.4 `effective_min_score` 优先级（`hybrid.rs:104-125`）

```text
请求显式 min_score  >  （valid_at 非空 → 直接 None，不叠加任何服务端阈值）
                     >  场景专属阈值 (scene_min_scores[scene])
                     >  全局默认阈值 (default_min_score)
```

- 全局默认值来自 `HUB_RETRIEVAL_MIN_SCORE`，默认 `0.65`（`config.rs:52,267`）；`mcp-server` 仅在 `> 0` 时注入（`main.rs:107-113`），0 表示关闭。
- 场景阈值来自 `HUB_RETRIEVAL_SCENE_MIN_SCORES`（`main.rs:114-128`）。
- 时间锚定查询（`valid_at` 有值）**不叠加**服务端阈值（`hybrid.rs:108-118` 注释与实现一致），因为历史时点事实天然词法更弱。

### 4.5 MMR：无条件执行、有界池、O(pool²·dim)

| 事实 | 证据 |
|---|---|
| `search` 里**无条件**调用 `mmr_rerank`，没有任何开关/配置项可关闭 | `hybrid.rs:592` |
| 唯一可调参数是 `mmr_lambda`（默认 0.7，`config.rs:266`），构造时 `clamp(0,1)` | `hybrid.rs:67` |
| 相关性候选先截断到 `k * MMR_POOL_FACTOR`（=4）再做相似度矩阵 | `hybrid.rs:457-461` |
| 相似度矩阵是 **n×n 余弦**，任一侧向量不可用（缺向量/模型不匹配）时记 0 | `hybrid.rs:466-483` |
| 复杂度 `O(pool² · dim)` + 每步 `O(k · n)`；注释承认它会成为主要延迟来源 | `hybrid.rs:448-451` |
| 贪心选择：`mmr = λ·rel − (1−λ)·max_sim`，返回至多 k 个索引 | `algorithm.rs:266-295` |

因此「MMR 无条件执行」属实；但 `λ=1.0` 时它在数学上退化为按相关度排序（`1−λ=0`），**计算量仍然发生**（相似度矩阵照样构建）。这是轻量化里可优化的点，不是可配置的点。

---

## 5. Consolidation：模式、候选、决策、自动合并门槛

### 5.1 共享类型（`crates/common/src/consolidation.rs`，207 行）

| 类型/函数 | 行 | 内容 |
|---|---|---|
| `enum ConsolidationMode` | 19-25 | `Rules`（默认，`#[default]`）/ `Hybrid` |
| `ConsolidationMode::parse` | 37-43 | 只接受 `rules` / `hybrid`，未知值报可读错误（不静默） |
| `enum ConsolidationFallback` | 57-63 | `Rules`（默认，降级并如实上报）/ `Error`（拒绝执行） |
| `CONSOLIDATION_STRATEGY_VERSION` | 87 | `consolidation-v1` |
| `struct ConsolidationModeStatus` | 95-117 | requested_mode / effective_mode / candidate_strategy / embedding_used / embedding_calls / embedded_entries / fallback_applied / fallback_reason / strategy_version / similarity_threshold_calibrated |
| `rules()/hybrid()/fallback_to_rules()` | 138-175 | 降级时 requested_mode 仍为 hybrid（`:160-161` 注释：降级必须显式可见） |

默认值来源：`config.rs:278-283`（`consolidation_mode="rules"`、`fallback="rules"`、`allow_mode_override=false`、`embed_top_k=20`、`embed_min_similarity=None`、`lexical_min_similarity=0.80`）。

### 5.2 `ConsolidationEngine`（`crates/consolidation/src/engine.rs`，731 行）

| 符号 | 行 | 行为 |
|---|---|---|
| `NAMESPACE_LOCKS` / `namespace_lock` | 38-47 | **进程内**按命名空间串行化（注释 `:36-37` 明确：跨进程并发无法覆盖） |
| `embedding_allowed()` | 160-167 | 仅 `mode==Hybrid && embedder.provides_semantic_signal()` 为真 |
| `resolve_mode()` | 170-189 | `Rules` → `RulesCandidateFinder` + `Status::rules()`；`Hybrid` → 无嵌入器则 `apply_fallback("embedding_provider_not_configured")`，伪向量则 `apply_fallback("embedding_provider_not_ready")`，否则 `HybridCandidateFinder` |
| `apply_fallback()` | 192-213 | `Rules` 策略：`warn!` + `fallback_to_rules(reason)`；`Error` 策略：`HubError::Internal`（整个扫描不执行） |
| `thresholds()` | 220-233 | 去重阈值强制 `max(semantic + 0.01)`，保证 0.82 < 0.92 < 0.98 单调 |
| `load_active()` | 236-244 | `all_in_namespace(ns, false)` + 过滤 `status==Active`（**无条数上限**，见 §7.4） |
| `analyze()` | 247-292 | 候选发现只算一次；embedding_calls / embedded_entries 取自 `CandidateOutcome`；effective_mode==hybrid 时置 `embedding_used=true` |
| `calculate_stats()` | 295-310 | 全量活跃条目 + `analyze` + `staleness.detect_in_entries` |
| `build_stats()` | 313-368 | 健康度 `1 − (0.45·冗余 + 0.40·冲突 + 0.15·陈旧占比)`（`:349-353`）；`stale_archive_candidates` 只做建议不自动归档（`:337-342`） |
| `consolidate_namespace()` | 371-524 | 见下 |
| `resolve_proposal()` | 527-586 | 仅 `Open` 可裁决（否则 `Conflict`）；`Merge` 会校验 `survivor_id ∈ member_ids` |
| `execute_merge()` | 589-708 | 见 §5.5 |
| `proposal_metadata()` | 714-731 | 只写 relation / candidate_source / reason_code / mode / requested_mode / strategy_version / fallback_applied / similarity，**不写原文/令牌/向量** |

`consolidate_namespace` 的顺序（`:379-521`）：取锁 → `load_active` → `analyze` → 陈旧检测 → 统计 → 冲突组先建提案（`:409-434`）→ 重复组剔除冲突成员（`:437-443`）→ 自动合并判定（`:450-470`）→ `new_proposals.truncate(max_proposals)`（`:495`）→ `dry_run` 直接返回（`:497-500`）→ `open_exists` 幂等查重后 `save_proposal`（`:503-521`）。

### 5.3 候选发现：两条召回通道与硬上限

**`RulesCandidateFinder`（`candidates/rules.rs`，519 行）**

| 通道 | 行 | 规则 |
|---|---|---|
| R1 精确重复 | 52-83 | `(kind, content)` 分桶；桶内两两配对 |
| R2 规范化指纹 | 85-122 | `mem_core::content_fingerprint` 相同且原文不同 → `CanonicalDuplicate` |
| R3/R4 业务键 | 124-172 | 同 key 不同内容：`valid_from` 可定序或 `is_fact_evolution` → `SupersedingVersion`，否则 `Conflict` |
| R5–R7 词法 | 174-180 | `lexical_candidates(entries, lexical_min_similarity, max_candidate_pairs)` |
| 硬上限 | 26-30 | `MAX_PAIRS_PER_BUCKET=1000`（×8 截断并 `warn!`，`:74,113`）、`MAX_TOKEN_DOC_FREQ=64`、`MAX_TOKENS_PER_ENTRY=24` |
| 产出 | 182-187 | `dedup_pairs(pairs)`，`embedding_calls: 0`、`embedded_entries: 0` |

**`HybridCandidateFinder`（`candidates/hybrid.rs`，511 行）**：先跑规则候选（`:62`），再叠加两条向量通道——通道 1 复用已存且模型匹配的向量（`:66-78`，零出站）；通道 2 为缺失条目按需嵌入（`:81-110`），硬上限 `max_embed_entries`（默认 20）与 `embed_batch_size`（默认 32，`candidates.rs:236-243`）。相似度比对用**穷举**（`n ≤ EXHAUSTIVE_MAX_ENTRIES=256`）或 **LSH**（8 表 × 6 bit，`hybrid.rs:28-34,311-388`）。

> **代码与注释不一致（须记录）**：`candidates/hybrid.rs:16-17` 的模块文档宣称第三条通道是「ANN 近邻召回：用 `MemoryStore::similar`」，`:80` 的注释也写「通道 2 + 3 … ANN 召回」。但该文件**没有任何 `store.` 调用**（grep 验证：`candidates/` 下无 `.similar(`、无 `store.`），实际实现是进程内穷举/LSH。即：不存在的「向量 ANN 通道」被写在文档里。

### 5.4 决策层：`policy.rs` 自动合并门槛与 `detectors/*`

| 组件 | 行 | 内容 |
|---|---|---|
| `PolicyMode` | `policy.rs:9-17` | `Manual`（默认）/ `SemiAuto` / `FullAuto` |
| `MergeEligibility` | `:21-26` | `AutoMergeable` / `ReviewRequired` |
| `merge_eligibility(relation)` | `:36-43` | **只有** `ExactDuplicate`、`CanonicalDuplicate` 是 `AutoMergeable`；其余全部 `ReviewRequired` |
| `AutoPolicy` 默认 | `:62-71` | mode=Manual、auto_merge_threshold=0.98、auto_merge_max_members=3、auto_archive_stale_score=0.85 |
| `should_auto_merge` | `:89-106` | 先判关系（非 AutoMergeable 直接 false）；Manual 恒 false；SemiAuto 需 sim ≥ 阈值且 members ≤ 上限；FullAuto 只判阈值 |
| `ConflictDetector` | `detectors/conflict.rs:24-44` | semantic_sim_threshold=0.82、divergence_threshold=0.45、duplicate_sim_threshold=0.92、exhaustive_max_entries=256；`with_duplicate_threshold` 强制 ≥ semantic+0.01（`:56-61`） |
| `DuplicateDetector` | `detectors/duplicate.rs:50-75` | probe_k=20；精确/规范化/语义近重复走并查集（传递闭包），词法近匹配/证据不足只生成两成员组（`:69-74`） |
| `UnionFind` | `detectors/duplicate.rs:14-45` | 并查集实现 |
| `StalenessDetector` | `detectors/staleness.rs:16-62` | half_life_days=60、staleness_threshold=0.65；`staleness = time_decay × (1+0.5·(1−conf)) + (version≤1 ? 0.05 : 0)` |

### 5.5 「B 模式 embedding 调用恒为 0」在代码里的落点（三层结构性保证）

| 层 | 落点 | 证据 |
|---|---|---|
| 1. 引擎层 | `resolve_mode()` 的 `Rules` 分支**只构造** `RulesCandidateFinder`——该结构体本身没有嵌入器字段 | `engine.rs:171-175`；`candidates/rules.rs:33-34`（`pub struct RulesCandidateFinder;` 单元结构） |
| 2. 候选层 | `RulesCandidateFinder::find_candidates` 返回 `embedding_calls: 0`，实现体内无 `embed` 调用 | `candidates/rules.rs:182-187` |
| 3. 装配层 | `backup-worker` 只在 `Hybrid` 模式才构建并注入嵌入器；`DefaultIntegrationService` 只有被调用 `.with_embedder()` 时才持有嵌入器 | `backup-worker/src/main.rs:73-86`；`consolidation/src/service.rs:25,66-70` |
| 附加 | 合并后重嵌入受 `embedding_allowed()` 把关：规则模式下**正文变化也不重嵌入，只清空向量** | `engine.rs:627-660`（`:653-659` 的日志即规则模式分支） |

守卫测试（两处，均实测存在）：

- `crates/consolidation/tests/consolidation_eval.rs:767` `test_rules_mode_makes_zero_embedding_calls` —— 用 `CountingEmbedder` 断言 `call_count()==0` 且 `text_count()==0`（`:787-797`）。
- `crates/consolidation/src/lib.rs:161` `test_rules_mode_never_calls_embedding` —— 即便注入嵌入器，`counter.call_count()==0`（`:182`）。
- 另有评测门禁 `consolidation_eval.rs:726`：`assert_eq!(b.embedding_calls, 0)`。

### 5.6 「语义候选永不自动合并」在代码里的落点

1. **关系判定**：`policy::merge_eligibility` 仅让 `ExactDuplicate`/`CanonicalDuplicate` 通过（`policy.rs:36-43`）。
2. **策略判定**：`should_auto_merge` 第一句即 `if eligibility != AutoMergeable { return false; }`（`policy.rs:95-97`），**先于**任何相似度比较。
3. **引擎调用点**：`engine.rs:450` 先算 `merge_eligibility(d.relation)` 再交给 `should_auto_merge`（`:451-456`）。
4. **候选标注**：向量通道产出的候选被标为 `SemanticNearDuplicate` 或 `Conflict`（`candidates/hybrid.rs:3-6` 模块文档 + `:204-310` 分类实现）。
5. **测试**：`policy.rs:140-143` `test_semantic_candidate_never_auto_merges_even_at_max_similarity`（FullAuto 下 ReviewRequired, 1.0, 2 仍不合并）；`consolidation/src/lib.rs:190` `test_semantic_candidate_is_not_auto_merged`；`candidates/hybrid.rs:474` `test_semantic_candidate_is_never_auto_mergeable`。

### 5.7 整合入口的两条路径不一致（重要）

| 入口 | 服务对象 | 是否注入嵌入器 | 后果 |
|---|---|---|---|
| `memory_consolidate` | 在 MCP 层**新造** `ConsolidationEngine`（`crates/mcp/src/tools.rs:651-666`），嵌入器来自 `ToolDispatcher::with_embedder`（`mcp-server/src/main.rs:205`） | **是** | 配 `HUB_CONSOLIDATION_MODE=hybrid` 时**可以**真的跑向量模式 |
| `memory_integrate_scan` | `DefaultIntegrationService::scan`（`tools.rs:563`；`service.rs:124-162`） | **否**——`mcp-server/src/main.rs:154-167` 构造 `DefaultIntegrationService` 时**没有** `.with_embedder(...)` | `hybrid` 模式必然走 `apply_fallback("embedding_provider_not_configured")`，effective_mode 恒为 rules |

即同一个部署里，两个整合工具对 hybrid 的实际能力不同；`memory_integrate_scan` 在 `HUB_CONSOLIDATION_FALLBACK=error` 时会直接报错。此为代码事实（`main.rs:154-167` 与 `:205` 的对比），非推测。

---

## 6. Cognitive：三个 service 的真实能力、重复逻辑、生成式 LLM 依赖

### 6.1 三个 service 的实际文件与能力

| Service | 文件（行数） | 主要能力（公开方法 + 行号） |
|---|---|---|
| `ChatCognitiveService` | `crates/mem-core/src/chat_cognitive_service.rs`（1815） | `compute_messages_hash`(66)、`build_excerpt`(80)、`build_excerpt_report`(89)、`process_chat_session`(121)、`review_proposal`(316)、`list_active_intentions`(365)、`scoped`(57) |
| `WorkingMemoryService` | `crates/mem-core/src/working_memory_service.rs`（931） | `start_task`(51)、`start_task_with_evidence`(64)、`checkpoint_task`(99)、`finish_task`(126)、`distill_experience`(166)、`record_feedback`(236)、`search_experiences`(318)、`search_experiences_with`(331) |
| `CodingMemoryService` | `crates/mem-core/src/coding_memory_service.rs`（479） | `start_thread`(38)、`get_thread`(62)、`list_threads`(75)、`record_revision`(91)、`get_revision`(134)、`list_thread_revisions`(147)、`list_file_revisions`(163) |

装配位置（三者均在 `mcp-server` 装配，不存在「未装配」的认知服务）：

```text
mcp-server/src/main.rs:169  WorkingMemoryService::new(store.clone())
mcp-server/src/main.rs:170  CodingMemoryService::new(store.clone())
mcp-server/src/main.rs:173-186  ChatCognitiveService::new(store, chat_extractor, CognitivePolicy{ mode: Manual, … })
mcp-server/src/main.rs:206-208  .with_working_memory(…) / .with_coding_memory(…) / .with_chat_cognitive(…)
```

治理默认值：`CognitiveGovernanceMode::Manual`（`domain.rs:343-344` `#[default]`）；`CognitivePolicy::default` 为 `auto_accept_threshold: 0.95`、`evidence_retention_days: 90`（`domain.rs:364-372`）。

### 6.2 重复逻辑点（如实列出）

| 重复项 | 位置 A | 位置 B / C | 说明 |
|---|---|---|---|
| 内联 `auth.has_scope` 校验 | `chat_cognitive_service.rs:126,323` | `working_memory_service.rs:73,107,136,172`；`coding_memory_service.rs:43,68,82,96` | `DefaultMemoryService` 反而有 `require_read/require_write` 助手（`memory_service.rs:177-196`），认知三服务各自内联，共 11 处重复 |
| 时间处理 | 三服务全部直接 `Utc::now()`，未使用 `Clock` trait | `chat_cognitive_service.rs:131,327,374`；`working_memory_service.rs:87,151,199,228`；`coding_memory_service.rs:234` | `Clock` 在 `infra.rs:577` 定义，`HybridRetriever` 与 `DefaultMemoryService` 都支持注入；认知三服务不支持，导致其时间不可在测试中固定 |
| 输入校验 | `validate_cognitive_candidate`（`domain.rs:375-412`） | `validate_experience_candidate`（`domain.rs:755`）；`validate_coding_revision`（`coding_memory_service.rs:101` 调用）；`start_thread` 内联仓库/标题校验（`:46-56`） | 三者都重复「trim 非空 / 长度上限 / `contains_sensitive_credentials`」骨架，但常量各写各的（如 chat 用 8192，`domain.rs:382`） |
| 证据转换 | `chat`：`NewEvidence{ id: None, source_kind: Session, source_ref: Some(session_id), content_hash, excerpt, occurred_at, expires_at }`（`chat_cognitive_service.rs:169-181`） | `coding`：直接把调用方给的 `source_evidence_id` 存进 `NewCodingRevision`（`coding_memory_service.rs:91-130`，**不校验证据是否存在**）；`working`：`source_evidence_id` 原样透传（`working_memory_service.rs:91,158-160`） | 只有 Chat 自己造证据行并脱敏；Coding/Working 只引用外部 UUID，没有存在性校验（对比 `record_revision` 对 `thread_id` 有存在性校验，`:104-113`） |
| 提案状态转换 | `CognitiveProposalStatus`（`domain.rs:178-184`：Candidate / PendingReview / Accepted / Rejected / Superseded） | 集成提案 `ProposalStatus`（Open / Merged / Dismissed / Expired，迁移 1 `:107` 的 CHECK） | 两套状态机并存（设计上刻意区分，注释 `domain.rs:175`）；但 MCP 侧把两者混用——见 §6.4 |
| 幂等去重 | Chat 按 `content_hash` 复用证据（`:134-145`）+ 内容完全相同则跳过（`:235-239`） | Working 按 `(task, solution)` 全量分页扫描去重（`:188-203`）；Coding 按 namespace+repository+diff_hash（`:120-127`） | 三种去重口径不同，均为各自实现 |

### 6.3 是否存在生成式 LLM 依赖/调用点：**不存在**（grep 证明）

| 检查 | 范围 | 结果 |
|---|---|---|
| 根依赖 | grep `openai|anthropic|llm|claude|gemini|gpt|prompt|completion|chat/completions` 于 `Cargo.toml` | **0 命中** |
| 源码内 prompt/completions | grep 同模式于 `crates` 下所有 .rs | 仅 2 处：`crates/mcp/src/protocol.rs:35,65`——MCP 协议的 `prompts` 能力声明字段，与 LLM 无关 |
| 抽取器实现 | grep `ChatCognitiveExtractor` | trait 定义 `infra.rs:622`；实现仅 `MockChatCognitiveExtractor`（`testing.rs:246`）与测试内 `FlakyExtractor`（`worker/src/cognitive_job.rs:172`） |
| 生产装配 | `mcp-server/src/main.rs:173` | `mem_core::testing::MockChatCognitiveExtractor::new()`，并在 `:174-177` 打印 `warn!` |

结论：**没有生成式 LLM SDK 依赖，也没有任何 prompt/completion 调用**；Chat 认知流的候选完全由注入的 `ChatCognitiveExtractor` 决定，生产当前注入的是**占位 Mock**（不会理解对话内容，只会返回构造时给定的候选）。

**代码与注释/配置不一致（重要）**：`mcp-server/src/main.rs:176` 的告警文案写「请通过 `HUB_CHAT_EXTRACTOR` 配置后再启用自动入库」，但全仓 grep `HUB_CHAT_EXTRACTOR` **只命中这一行字符串字面量**——`AppConfig` 里没有该字段，`from_env` 不读取它。即：告警引导用户去设置一个没有任何代码消费的环境变量。

### 6.4 认知提案的实际落点（缺口）

| 事实 | 证据 |
|---|---|
| `CognitiveProposalRecord` 是内存返回值，**没有对应存储 trait** | `infra.rs` 中只有 `IntegrationStore`（`:237-251`，作用于 `IntegrationProposal`）；全文无 `CognitiveProposalStore` |
| Postgres 侧无认知提案表 | `crates/store/src/postgres.rs` 中 `cognitive` 仅出现在迁移 6 的文件名（`:192-193`） |
| `review_proposal` 未被任何 MCP 工具调用 | grep：调用点只有 `chat_cognitive_service.rs:1276,1296`（自身单测） |
| `cognitive_proposal_resolve` 实际操作的是**整合提案** | `crates/mcp/src/tools.rs:975-986` 调 `self.integration.resolve(ResolveRequest{...})`；决策映射为 `ResolveAction::Dismiss/Merge`（`:965-974`） |

后果：`cognitive_chat_propose` 返回的 `proposals`（含 `id`）在响应后即丢失，无法按 id 评审；`cognitive_proposal_resolve` 的 `proposal_id` 必须来自 `memory_integration_list` 的整合提案。`CognitiveProposalStatus` 的 Accepted/Superseded 转换在 MCP 路径上不可达。

---

## 7. Storage：trait、账户作用域、两种实现的差异

### 7.1 `MemoryStore` 方法清单（`crates/mem-core/src/infra.rs:21-154`）

| 方法 | 行 | 默认实现 | 备注 |
|---|---|---|---|
| `insert` | 23 | — | id 由调用方分配 |
| `insert_initial` | 29-49 | **有**（insert + record_version，非原子） | 生产/内存实现应覆盖为原子 |
| `get` | 52 | — | — |
| `get_by_key` | 55 | — | — |
| `save` | 58 | — | 无条件 update |
| `save_if_version` | 68-75 | **有**（退化为 save） | 乐观锁；生产/内存必须覆盖 |
| `revise` | 85-110 | **有**（save_if_version + record_version） | 原子修订 |
| `delete` | 113 | — | 物理删除 |
| `list` | 117 | — | 默认 updated_at desc, id desc；带 text_contains 时按相关度 |
| `count` | 120 | — | 不含分页/limit |
| `similar` | 124-129 | — | 签名是 `(&MemoryFilter, &[f32], k)`——**过滤条件下推**，不是旧文档里的 `(namespace, embedding, k, kinds)` |
| `all_in_namespace` | 132 | — | 备份/整备用；merged 可排除 |
| `list_namespaces` | 139 | — | 全量命名空间 |
| `record_version` | 142 | — | — |
| `versions` | 145 | — | 按版本号升序 |
| `archive_expired` | 148 | — | worker 用 |
| `ping` | 151-153 | **有**（Ok(())） | 真探库由实现覆盖 |

`MemoryHubStore` 是 10 个 trait 的聚合（`:419-446`）：MemoryStore + BackupStore + IntegrationStore + GrantStore + AgentStore + EvidenceStore + RelationStore + TaskStore + ExperienceStore + CodingStore。

### 7.2 `AccountScopedStore` 的作用域包装方式

| 机制 | 行 | 说明 |
|---|---|---|
| 分隔符 | `account_scope.rs:34` | `SCOPE_SEPARATOR = '/'` |
| 组合/剥离/判定 | `:37-51` | `scope_name` / `unscope_name` / `in_scope` |
| 幂等加前缀 | `:54-60` | 已带本账户前缀则原样返回（备份恢复往返不会二次加前缀） |
| 包装器 | `:68-76` | `AccountScopedStore<S: ?Sized>{ inner: Arc<S>, account: AccountId }`，泛型可包 `dyn MemoryStore` 或 `dyn MemoryHubStore` |
| 输入/输出变换 | `:79-108` | `scoped` / `unscoped` / `scoped_filter` |
| **fail-closed 归属断言** | `:91-97` | `assert_owned`：键不带本账户前缀 → `HubError::NotFound`（不泄露存在性） |
| trait 覆盖 | `:119,256,300,348,423,448,521,579,651,775` | 10 个 trait 全部有包装实现 |
| 构造位置 | `memory_service.rs:172-174`；`backup_service.rs:94-96`；`service.rs:133,166,171`；`tools.rs:636,708` | 每个请求各自构造，非共享 |

### 7.3 `PostgresHubStore` 与 `InMemoryHubStore` 的差异

| 维度 | `PostgresHubStore` | `InMemoryHubStore` | 差异影响 |
|---|---|---|---|
| 文件/规模 | `crates/store/src/postgres.rs`（3391 行） | `crates/store/src/in_memory.rs`（2192 行） | — |
| `list` 排序 | 带 text_contains 且能生成 OR 查询时：`ts_rank_cd(to_tsvector('simple', tsv_tokens), to_tsquery(...)) desc, updated_at desc, id desc`；否则 `updated_at desc, id desc`（`postgres.rs:1060-1080`） | 带查询词时：**词元命中数**降序 → updated_at desc → id desc；否则 updated_at desc, id desc（`in_memory.rs:317-346`） | 相关度口径不同（ts_rank_cd 权重 vs 命中计数），同一查询可能给出不同顺序 |
| `list` 的 text 过滤 | SQL 层 tsvector 匹配（`push_memory_filters`，`postgres.rs:468` 起） | Rust 层集合交集，`hits==0` 直接剔除（`in_memory.rs:317-327`） | — |
| `similar` 距离 | pgvector `embedding <=> $1::vector`，`embedding is not null`，`order by dist limit k`，score = `1 − dist`（`postgres.rs:1100-1127`） | 内存里**点积**（假定已归一化），要求 `iv.len()==embedding.len()` 否则静默跳过，排序后 `truncate(k)`（`in_memory.rs:364-401`） | 未归一化向量在内存实现下分数不等价；维度不符时内存实现**静默丢弃** |
| `similar` 默认状态 | statuses 为空 → Active（`postgres.rs:1102-1105`） | 同样（`in_memory.rs:370-374`） | 一致 |
| `all_in_namespace` | `select … where namespace=$1 [and status<>'merged'] order by created_at, id`——**无 LIMIT**（`postgres.rs:1130-1150`） | 全量遍历 + `sort_by(created_at, id)`——**无上限**（`in_memory.rs:403-419`） | 两者都无界，见 §7.4 |
| `list_namespaces` | `select distinct namespace from memories order by namespace`（`postgres.rs:1152-1159`） | 去重 + 排序（`in_memory.rs:421-429`） | PG 侧取的是**存储键**（含 `{account}/` 前缀），内存侧同样；worker 直接用它，日志/报表里出现的是内部键 |
| 迁移 | `connect()` 内执行 `run_migrations`（`postgres.rs:416-462`） | 无 schema 概念 | — |
| 唯一键冲突映射 | `23505→Conflict`、`23503→InvalidArgument`、`RowNotFound→NotFound`（`postgres.rs:150-166`） | 手写检查 | 错误类型在不同实现下是否完全一致，需逐方法核对（本报告未逐一验证） |

### 7.4 `all_in_namespace` 无界：调用点与其后果

| 调用点 | 行 | 用途 |
|---|---|---|
| `ConsolidationEngine::load_active` | `engine.rs:236-244` | 整合扫描把整个命名空间的活跃条目装进内存，再做 O(n²) 候选发现 |
| `IntegrationScanJob::run_once` | `worker/src/integration_job.rs:59` | 先全量取一次（仅为统计 active 条数），随后 `consolidate_namespace` 内部**再取一次** |
| `BackupJob::run_once` | `worker/src/backup_job.rs:54` | `include_merged=true` 全量快照 |
| `DefaultBackupService::create` / `create_safety_backup` | `backup_service.rs:308,646-649` | 全量备份 / replace 前安全备份与清空 |
| `ChatCognitiveService::process_chat_session` / `review_proposal` | `chat_cognitive_service.rs:192-198,335-341` | 为冲突/去重比对全量加载 |
| `WorkingMemoryService::scan_experiences` | `working_memory_service.rs:188` | 全量分页扫描做去重 |

即：`IntegrationScanJob` 单轮对同一命名空间至少做 2 次全量装载（1 次为计数、1 次在引擎内），且**无条数上限**。轻量化若要控制内存峰值，这是首要落点（当前没有 LIMIT/分批参数）。

---

## 8. Worker：每个 job 的真实行为与上限、调度与互斥

### 8.1 五个 job 的实际行为

| Job | 文件 | 入口 | 真实行为 | 上限/节流 |
|---|---|---|---|---|
| `BackupJob` | `worker/src/backup_job.rs`（187 行） | `run_once`（47） | 遍历 `list_namespace_infos`（`:51`），对每个命名空间 `all_in_namespace(ns, true)`（`:54`）→ 自算 `checksum_of`（`:116-119`，SHA-256(serde_json::to_vec(entries))）→ 落 `BackupRecord`，**payload 是 `{"version":1,"namespace","entries"}`（`:63-67`）** → `prune`（`:88-112`：每命名空间保留最新 keep 份 + 超过 retention_days 的非最新份） | 无并发上限；`keep.max(1)`（`:40`） |
| `IntegrationScanJob` | `worker/src/integration_job.rs`（284 行） | `run_once`（52） | `list_namespaces`（`:56`）→ 对**每个**命名空间全量装载并计数（`:59-64`）→ active<2 跳过（`:66-68`）→ `ConsolidationEngine::consolidate_namespace(ns, false, Some(sim), worker)`（`:106-117`） | `max_proposals`；候选层 `MAX_PAIRS_PER_BUCKET×8` / `max_candidate_pairs=20000`；**无分页、无命名空间数上限** |
| `EvidenceRetentionJob` | `worker/src/evidence_job.rs`（108 行） | `run_once`（32） | `list_namespace_infos`（`:33`）→ **逐命名空间** `purge_expired_evidence(ns, now)`（`:36-40`） | 无批大小；每次一个 DELETE |
| `ChatCognitiveJob` | `worker/src/cognitive_job.rs`（331 行） | `run_session`(60) / `run_session_with_result`(70) | 重试循环 `while attempts < max_retries`（`:85`），仅对 `is_retryable()` 错误重试（`:108-120`），指数退避 `initial_backoff_ms * (1 << (attempts-1))`（`:124`） | **未被任何二进制装配**（§8.3） |
| `ExperienceDistillJob` | `worker/src/experience_job.rs`（284 行） | `run_distill`(63) / `run_distill_with_result`(76) | 同构重试/退避（`:90-156`） | **未被任何二进制装配**（§8.3） |

### 8.2 backup-worker 的手写 tick 循环（`crates/bins/backup-worker/src/main.rs`）

| 事实 | 行 | 说明 |
|---|---|---|
| 固定 30s tick | 116-117 | `tokio::time::interval(30s)` + `MissedTickBehavior::Delay` |
| 备份按 cron | 124-138 | `schedule.next_after(last_backup) <= now` 才跑；失败只 `error!` 不退出（下个 tick 重试） |
| 过期清理 | 141-155 | 间隔 `EXPIRY_SWEEP_INTERVAL_H`（`0` → `expiry_interval=None` 关闭，`:94-100`） |
| 证据清理 | 158-172 | 与过期清理**共用同一间隔**开关 |
| 整合扫描 | 175-186 | `if (now - last_scan) >= 24h` —— **硬编码每 24 小时，且没有任何配置开关**；进程不在跑才会停 |
| 心跳初值 | 110-113 | 四个 `last_*` 都设为「365 天前」→ 启动后首个 tick 就会跑一遍全部作业 |
| 存储约束 | 191-207 | 只接受 PostgreSQL；非 PG 连接串直接 `Err`（内存存储对备份无意义） |
| 调用 `JobScheduler` | — | **没有**：全文件无 `JobScheduler`/`ScheduledTask` 引用（grep 验证，见 §8.3） |

**无跨进程互斥（确认）**：唯一的串行化是 `ConsolidationEngine` 内的 `NAMESPACE_LOCKS`（`engine.rs:38-47`），注释自陈「跨进程并发（MCP 进程与 backup-worker 进程同时扫同一命名空间）无法用进程内锁覆盖，彻底方案是数据库侧的部分唯一索引，属于后续单独评估项」。多副本 `backup-worker` 之间同样没有分布式租约：`run_once` 无 advisory lock、无 `INSERT ... ON CONFLICT` 抢占。备份作业本身会因此产生重复快照（每个副本各建一份），整合扫描会重复计算（提案靠 `open_exists` 幂等，`:503-517`）。

### 8.3 未被任何二进制装配的组件（grep 证明）

| 组件 | 定义 | 全仓引用 | 结论 |
|---|---|---|---|
| `JobScheduler` | `worker/src/scheduler.rs:23` | `scheduler.rs` 自身 + `worker/src/lib.rs:23` 再导出 + 其单测（`:106,125,142`） | **无任何 bin 使用**；`backup-worker` 用自写 tick 循环替代 |
| `ScheduledTask` / `fn_task` | `scheduler.rs:14,79` | 同上 | 同 |
| `ChatCognitiveJob` | `worker/src/cognitive_job.rs:33` | 自身 + `lib.rs:19` 再导出 + 自身单测（`:216,267,303`） | **未被装配**：`mcp-server` 直接持有 `ChatCognitiveService`，`backup-worker` 只装配 Backup/Integration/Evidence 三个 job |
| `ExperienceDistillJob` | `worker/src/experience_job.rs:36` | 自身 + `lib.rs:21` + 自身单测（`:184,248`） | **未被装配** |

对比：`backup-worker/src/main.rs:20` 的 `use worker::{BackupJob, EvidenceRetentionJob, IntegrationScanJob};` 只引入这三个。因此 `worker` crate 的 5 个 job 里有 2 个、以及整个 `scheduler` 模块（144 行）是**死代码路径**（会被编译、有单测，但生产不执行）。

---

## 9. MCP：28 个工具的真实定义与鉴权路径

### 9.1 工具总数与构成

`crates/mcp/src/schema.rs:42` 的 `get_tool_definitions()` 返回 **28 个** `ToolDefinition`（结构体定义 `schema.rs:20-29`：name / description / read_only / destructive / idempotent / required_scope / input_schema）：

- 基础 18 个：`memory_put` … `memory_revoke`；
- 认知 10 个：`cognitive_*`（3）+ `working_*`（5）+ `coding_*`（2）。

### 9.2 完整工具表（元数据均为 schema 原文；行号为该 ToolDefinition 起始行）

| # | 名称 | required_scope | read_only | destructive | idempotent | 必填参数 | 关键可选参数 | 行 |
|---|---|---|---|---|---|---|---|---|
| 1 | `memory_put` | memory:write | false | false | true | namespace, kind, content | key, tags, metadata, confidence, ttl_seconds | 46 |
| 2 | `memory_get` | memory:read | true | false | true | namespace | id, key | 96 |
| 3 | `memory_update` | memory:write | false | false | true | namespace, id | content, kind, tags, metadata, confidence, expires_in_seconds | 122 |
| 4 | `memory_delete` | memory:write | false | **true** | true | namespace, id | — | 153 |
| 5 | `memory_search` | memory:read | true | false | true | namespace | query_text, kinds, tags, top_k, include_archived, min_score, scene, layer, valid_at, include_superseded | 170 |
| 6 | `memory_list` | memory:read | true | false | true | namespace | kinds, statuses, tags, source_agent, text_contains, cursor, limit | 239 |
| 7 | `memory_stats` | memory:read | true | false | true | namespace | — | 291 |
| 8 | `memory_history` | memory:read | true | false | true | namespace, id | — | 307 |
| 9 | `memory_backup_create` | memory:admin | false | false | true | namespace | note | 325 |
| 10 | `memory_backup_list` | memory:admin | true | false | true | （无） | namespace | 352 |
| 11 | `memory_restore` | memory:admin | false | **true** | **false** | backup_id | mode（dry_run / merge / replace，默认 merge） | 371 |
| 12 | `memory_integrate_scan` | memory:write | false | false | true | namespace | dry_run, sim_threshold, mode | 392 |
| 13 | `memory_integration_list` | memory:read | true | false | true | namespace | — | 423 |
| 14 | `memory_integration_resolve` | memory:write | false | false | true | proposal_id, action | survivor_id, note | 439 |
| 15 | `memory_consolidate` | memory:write | false | false | true | namespace | dry_run, sim_threshold, auto_merge, mode | 466 |
| 16 | `memory_consolidation_stats` | memory:read | true | false | true | namespace | — | 502 |
| 17 | `memory_grant` | memory:admin | false | false | true | namespace, agent_id | can_read, can_write | 519 |
| 18 | `memory_revoke` | memory:admin | false | **true** | true | namespace, agent_id | — | 543 |
| 19 | `cognitive_chat_propose` | memory:write | false | false | **false** | namespace, turns | — | 561 |
| 20 | `cognitive_proposal_resolve` | memory:write | false | false | true | proposal_id, decision | reason, custom_content | 604 |
| 21 | `cognitive_intentions_list` | memory:read | true | false | true | namespace | status, limit | 638 |
| 22 | `working_task_start` | memory:write | false | false | **false** | namespace, task | scene, source_evidence_id, metadata | 666 |
| 23 | `working_task_finish` | memory:write | false | false | true | namespace, task_id, status | result_summary, failure_reason | 701 |
| 24 | `working_experience_distill` | memory:write | false | false | **false** | namespace, task, solution | task_id, search_keys, preconditions, boundary_envs, failure_modes, validation, confidence | 736 |
| 25 | `working_experience_feedback` | memory:write | false | false | true | namespace, experience_id, score | status | 801 |
| 26 | `working_experience_search` | memory:read | true | false | true | namespace, query | env, strict_env, top_k | 832 |
| 27 | `coding_thread_create` | memory:write | false | false | **false** | namespace, repository, title | branch, description, task_id | 871 |
| 28 | `coding_revision_record` | memory:write | false | false | true | namespace, repository, file_path, diff_content, change_kind | thread_id, branch, commit_sha, parent_sha, symbol_fingerprint, is_committed, author, source_evidence_id, source_task_id | 914 |

分发实现：`crates/mcp/src/tools.rs:203` 起的 `dispatch` 用 `match name` 覆盖全部 28 个（`:205-236`）。

### 9.3 鉴权路径：HTTP 与 stdio 的真实差异

**HTTP（`POST /mcp`，`transport.rs:621-764`）**

| 步骤 | 行 | 行为 |
|---|---|---|
| Origin 校验 | 629-641 | `allowed_origins` 非空时逐项比对，不匹配 → 403 |
| 体积上限 | 643-651 | 超 `max_request_bytes` → 413（另有 axum `DefaultBodyLimit`，`:516`） |
| Bearer 解析 + 内省 | 653-663 | `extract_bearer_token`（`:124-134`）→ `introspector.introspect`；失败 → 401 |
| 限流 | 665-673 | 仅对已认证请求按 `agent_id` 判流（未认证不占配额） |
| JSON-RPC 分发 | 675-739 | 支持批量数组；通知帧返回 204 |
| 状态码映射 | 741-753 | `32001→401`（附 `WWW-Authenticate: Bearer error="invalid_token"`）、`32002→403`、`32003→429`、`32004→424` |

**JSON-RPC 内核（`handle_jsonrpc_ext`，`transport.rs:302-438`）**

| 步骤 | 行 | 行为 |
|---|---|---|
| 参数校验 | 310-315 | jsonrpc 必须 "2.0"、method 非空 |
| `initialize` / `server/discover` | 319-336 | 协议版本协商（客户端版本在白名单内则回显） |
| `ping` / `tools/list` | 338-340 | `tools/list` **不做作用域过滤**，任何已连上传输层的调用方都能拿到 28 个工具名与 schema |
| `tools/call` 鉴权 | 361-376 | auth 为 None → **立即返回 `HubError::AuthRequired`（code 32001）**，不再往下走 |
| 作用域强制 | 379-393 | `required_scope_for_tool(name)`（`:111-121`：memory:read/write/admin → Scope），不符 → 32002 |
| 拦截器 | 396-423 | 可短路返回；CJS 适配层挂这里 |
| 分发 | 426-428 | `dispatcher.dispatch(auth, name, arguments)` |

**stdio（`run_stdio`，`transport.rs:467-497`）—— 恒无认证的本地桥接**

```rust
// transport.rs:441-464  handle_raw_frame
let parsed: Result<JsonRpcRequest, _> = serde_json::from_str(trimmed);
match parsed {
    Ok(req) => {
        if req.id.is_none() && req.method.starts_with("notifications/") { return None; }
        // 454  stdio 场景无 HTTP 头，鉴权由调用方通过 params 或环境注入的
        // 455  服务主体承担；此处视为"已认证的本地桥接"。
        Some(handle_jsonrpc(state, None, req).await)     // 456  ← auth 恒为 None
    }
    …
}
```

由此产生的**真实缺陷（三处叠加）**：

1. `handle_jsonrpc(state, None, req)`（`:456`）→ `handle_jsonrpc_ext(…, None, …)`（`:298`）→ `tools/call` 分支第一道校验即 `auth: None → 32001 AuthRequired`（`:361-376`）。**stdio 模式下所有 `tools/call` 必然失败**，只有 initialize / ping / tools/list / 通知可用。
2. 注释（`:454-455`）宣称「视为"已认证的本地桥接"」，与实现相反：没有任何代码把本地主体注入成 `AuthCtx`。
3. `mcp-server/src/main.rs:276-279`：

```rust
// 276  stdio 模式：不带 Bearer 的本地桥接（仅限回环地址）
let local_auth = std::env::var("HUB_STDIO_LOCAL_AGENT")
    .unwrap_or_else(|_| "local-agent".to_string());
let _ = local_auth;                                   // 279  ← 读取后立即丢弃
run_stdio(Arc::new(state)).await?;
```

`HUB_STDIO_LOCAL_AGENT` 被读取、随后 `let _ = local_auth;` 丢弃，从未进入 `McpTransportState`（该结构体字段见 `transport.rs:137-150`，无本地主体字段）。因此「本地桥接已认证」是**注释层面的宣称**，代码层面不成立；注释说的「仅限回环地址」也没有任何代码校验（stdio 不绑定端口，该措辞无从生效）。

补充：`McpTransportState.capabilities`（`transport.rs:149`）经 `GET /capabilities`（`:590-618`）暴露 embed / semantic / retrieval / consolidation 四组能力，其中 `semantic.ready` 与 `consolidation.mode/vector_ready` 分开上报——这是把「检索语义能力」与「整合向量能力」解耦的落点。

---

## 10. Backup：v1–v4 负载、校验和、恢复与保留策略

### 10.1 负载版本与校验和函数（`crates/mem-core/src/backup_service.rs`）

| 项目 | 符号 | 行 | 覆盖字段 |
|---|---|---|---|
| 当前版本 | `BACKUP_PAYLOAD_VERSION: u32 = 4` | 29 | — |
| v1 校验和 | `checksum_v1(entries)` | 208-211 | entries |
| v2 校验和 | `checksum_v2(entries, evidences, relations)` | 214-224 | + evidences、relations |
| v3 校验和 | `checksum_v3(…, tasks, experiences, task_experiences)` | 227-243 | + 任务/经验/关联 |
| v4 校验和 | `checksum_v4(…, coding_threads, coding_revisions)` | 247-267 | + 代码演进线程/修订 |
| 负载构造 | `payload_of(…)` | 271-294 | version 4 + namespace + entries + evidences + relations + tasks + experiences + task_experiences + coding_threads + coding_revisions |

算法实现细节：各 `checksum_vN` 都是对 `serde_json::to_vec(…)` 的字节流做 SHA-256（v2+ 为连续 `hasher.update` 后取一次 digest）。注释称「规范校验和」，实际是**按结构体字段序的确定性 JSON 序列化**——同一版本内稳定，但**不是跨语言/跨字段重排的规范化 JSON**；若未来重排字段顺序，旧快照的 v1 校验和将无法重算。

四个校验和函数均为**私有**（`fn`，非 `pub fn`），外部只能通过 `restore` 间接触发校验。

### 10.2 `restore` 的三种模式与安全备份（`backup_service.rs:505` 起）

| 步骤 | 行 | 行为 |
|---|---|---|
| 管理权限 | 506 | `require_admin`（Scope::Admin） |
| 取快照 | 508 | `store.get_backup(id)`（账户作用域内） |
| 解析 8 类实体 | 511-518 | entries / evidences / relations / tasks / experiences / task_experiences / coding_threads / coding_revisions |
| **校验和兼容校验** | 520-549 | 依次重算 v4/v3/v2/v1，命中任一即通过；全不命中 → `Internal("备份校验和不匹配（内容可能已损坏）")` |
| `dry_run` | 562-571 | 只对 entries 逐条比对：`cur == e` → unchanged、存在但不同 → updated、不存在 → created，**不写库** |
| `replace` 前安全备份 | 589-596 | 对每个受影响命名空间 `create_safety_backup`（`:297` 起：全量 8 类实体 + v4 校验和 + `note="restore-safety:{ns}"`）；`namespace == "*"` 时**跳过并 warn**（`:302-306`） |
| `replace` 清空 | 597-650 | 按外键依赖**逆序**删除：代码修订 → 线程 → 经验 → 任务 → 关系 → 证据 → 记忆条目（含历史版本） |
| `merge` | 651-655 | 只做 `require_write` 校验，不清空 |
| 恢复证据 | 657-671 | 逐条 `append_evidence`（**忽略错误** `let _ = …`） |
| 恢复条目 | 673-697 | 存在且相同 → unchanged；存在不同 → `revise`（`e.version = cur.version + 1`）；不存在 → `insert_initial` |
| 恢复关系/任务/经验/代码 | 699 起 | 逐类重建；关系缺 namespace 时按 backup.namespace 或成员条目反推（`:700-711`） |

**失败回滚路径（必须如实说明）**：

- **没有自动回滚/事务**：`restore` 全程不在数据库事务里，删除与重建都是逐条操作；`replace` 清空阶段大量使用 `let _ = store.delete_*(…).await`（`:605,609,617,621,629,644,648`）——单条删除失败被**静默吞掉**。
- **唯一的「回滚」是人工的**：先 `create_safety_backup`，失败后由操作者用 `restore(safety_id, Replace)` 反向恢复。代码里没有任何自动触发该动作的分支（grep `rollback` 与中文「回滚」在 `backup_service.rs` 仅命中测试函数 `test_backup_replace_purge_safety_backup_and_rollback`，`:2383,2494`）。
- 因此「replace 前自动创建安全备份」属实；「失败自动回滚」**不属实**，不应在验收材料里这样写。

### 10.3 快照保留策略

| 实现 | 行 | 规则 |
|---|---|---|
| `DefaultBackupService::prune(keep, retention_days)` | 825 起（`beyond_count = idx >= keep`，`:843`；`beyond_ttl = retention_days>0 && idx>=1 && created_at+days <= now`，`:844-847`） | 每命名空间按 created_at 降序，超出 keep 或超 TTL 的**非最新**份删除 |
| `BackupJob::prune` | `worker/src/backup_job.rs:88-112` | 同一规则的独立实现（`idx >= keep` 或 `idx>=1 && 超期`） |
| 默认参数 | `config.rs:270-271` | backup_keep_count=10、backup_retention_days=90 |

两处 prune 逻辑重复实现（服务层 vs 作业层），语义一致但代码不共享。

### 10.4 与 `BackupJob` 的格式不一致（代码与注释矛盾）

`worker/src/backup_job.rs:1-4` 的模块文档写：「与 MCP 暴露的 `memory_backup_create` 走同一套快照格式与校验和算法」。实测**不一致**：

| 维度 | `DefaultBackupService::create` | `BackupJob::run_once` |
|---|---|---|
| payload version | `4`（`backup_service.rs:283`） | `1`（`backup_job.rs:64`） |
| payload 字段 | 8 类实体 | 仅 entries（`backup_job.rs:63-67`） |
| checksum | `checksum_v4`（8 类实体串联） | SHA-256(serde_json::to_vec(entries))（`backup_job.rs:116-119`） |

兼容性后果（可从代码推出，无需数据库）：定时备份产生的快照能被 `restore` 接受（校验和会命中 v1 分支，`backup_service.rs:539,543`），但**只恢复记忆条目，不含证据/关系/任务/经验/代码演进**；而 replace 恢复会先按「快照涉及的命名空间」清空这些实体（`:597-650`）——即：用 `backup-worker` 的定时快照做 replace 恢复，会**先清空认知实体、再用空快照重建**，造成数据丢失。这是当前代码的真实行为，属缺口（§14 G4）。

### 10.5 本报告未验证的备份行为

- 「历史数据库从最早受支持版本升级后仍可 restore 旧快照」：需要真实 PG + 旧快照数据，本机无 PG → **未实测**。
- v2/v3 旧快照的实际恢复往返：仅在单测里以构造的 `BackupRecord` 覆盖（`backup_service.rs:2273,2337` 等），生产真实旧快照**未验证**。

---

## 11. CI：四个 job 的实际命令与触发条件

### 11.1 GitHub Actions（`.github/workflows/ci.yml`，159 行）

触发条件（`:3-8`）：push / pull_request 到 main、master，以及 `workflow_dispatch`。**当前分支 `feat/lightweight-deployment` 的 push 不会触发**（除非开 PR）。

全局环境（`:10-12`）：`CARGO_TERM_COLOR=always`、`RUSTFLAGS=-D warnings`。

| # | job | runs-on | 实际命令 | 行 |
|---|---|---|---|---|
| 1 | `quality`（质量门禁） | ubuntu-latest | `cargo fmt --all -- --check` | 31 |
| | | | `cargo clippy --all-targets --all-features -- -D warnings` | 36 |
| | | | `cargo test --workspace --all-features` | 39 |
| | | | `cargo doc --no-deps --workspace --all-features` | 42 |
| 2 | `postgres`（PG 集成测试） | ubuntu-latest + `pgvector/pgvector:pg15` service | 循环 `psql -v ON_ERROR_STOP=1 -f` 逐个应用 `crates/store/migrations/*.sql` | 73-81 |
| | | | `cargo test -p store --features pg-integration -- --ignored --nocapture --test-threads=1` | 88 |
| 3 | `supply-chain`（依赖准入） | ubuntu-latest | `cargo-deny check all --all-features` | 111-115 |
| | | | `rustsec/audit-check@v1.4.1`，忽略 RUSTSEC-2023-0071 | 117-130 |
| 4 | `build`（Release 构建，needs: [quality]） | ubuntu-latest | `cargo build --release -p mcp-server -p auth-server -p backup-worker --features mcp-server/jemalloc,auth-server/jemalloc,backup-worker/jemalloc` | 149-153 |
| | | | 逐个 `test -x target/release/<bin>` | 155-159 |

feature 组合要点：

- job 1/3 用 `--all-features`（含 jemalloc 与 pg-integration）→ 在 Ubuntu 可行，在**本机不可行**（路径空格）。
- job 2 只开 `pg-integration`，且必须 `--test-threads=1`（`:86-87` 注释：并发会触发「并发迁移」与外键冲突）。
- job 4 用**包级限定**的 feature 语法，注释（`:146-148`）明确写「feature 按包限定，避免虚拟 workspace 下 --features 的解析歧义」。这与 `Dockerfile:56` 的 `cargo build --release --bins --features jemalloc` 写法**不一致**（后者是工作区根上一把梭），而 Dockerfile 口径本机无法验证（无 docker）。

### 11.2 CNB 流水线（`.cnb.yml`，74 行）

| 项目 | 实际内容 | 行 |
|---|---|---|
| 触发 | main 分支 push | 11-14 |
| 运行环境 | 8 CPU + `services: [docker]` | 14-16 |
| 密钥 | `imports: https://cnb.cool/toroyxxx/ci-secrets/-/blob/main/deploy-key.yml` | 17 |
| 阶段 1 | `docker build`（APT_MIRROR=mirrors.tencentyun.com、CRATES_MIRROR=sparse+https://rsproxy.cn/index/），打 CNB_COMMIT 前 8 位与 latest 两个 tag | 19-28 |
| 阶段 2 | `docker push` 两个 tag；取 RepoDigests 写入 `/tmp/release.env` | 29-38 |
| 阶段 3 | ssh 到部署主机（${DEPLOY_HOST}）：轮换 current/previous_release.env → `docker compose pull` → `up -d` → 15×2s 探活 8080/readyz 与 3000/readyz → 失败则回滚到 previous_release.env 并 exit 1 | 39-74 |

部署行为要点：**只在 main push 时**触发；部署机是 ubuntu 用户下的 `~/memory-hub`；回滚是「换回上一份 current_release.env 再 docker compose up -d」，不是镜像层回滚。当前分支 `feat/lightweight-deployment` 不会触发 CNB 部署。

> 注：本节是审计当时的快照（74 行）。此后 `.cnb.yml` 已扩为：main 部署加流水线锁、新增 `$` 兜底键（其它分支 push 只跑质量门禁）、以及 `$` 下的 `tag_push`（打 tag 构建 `:<tag 名>` 定版镜像入库，不部署、不动 `latest`）。表格中的行号对应旧版，查当前配置请直接读文件。

### 11.3 本机无法执行的 CI 项

| 项 | 原因（实测） |
|---|---|
| `quality` 的 `--all-features` 三条 | 路径含空格 → tikv-jemalloc-sys C 编译失败（`GEMINI.md:102`）；且本机禁跑 cargo（另有 release 构建占锁） |
| `postgres` job | 无 psql、无 docker、5432 无监听 |
| `supply-chain` job | 无 cargo-deny / cargo-audit（未安装），且需联网 RustSec 数据库 |
| `build` job | 需 jemalloc feature → 本机不可编译 |
| `.cnb.yml` 全流程 | 无 docker；无 DEPLOY_HOST / SSH 私钥 |

---

## 12. 数据库迁移（P0.3）

### 12.1 11 个迁移文件与注册表的一一对应（实读）

`crates/store/migrations/` 实际 11 个 `.sql`，`MIGRATIONS`（`crates/store/src/postgres.rs:170-215`）11 项，顺序与文件名完全一致：

| # | 文件（`crates/store/migrations/`） | 该迁移实际做什么（读 SQL 原文，不按编号推断） | 注册行 |
|---|---|---|---|
| 1 | `20261011000001_initial_schema.sql` | 建 extension；agents / namespaces / namespace_grants / memories / memory_versions / memory_backups / integration_proposals / refresh_tokens / confirm_tokens / idempotency_keys / rate_limit_buckets / events / audit_log；`do ` 块建 `audit_writer` 角色并只授予 insert | 171-174 |
| 2 | `20261012000002_accounts_sandbox.sql` | `accounts` / `account_sessions` 表；引导账户固定 UUID `00000000-0000-0000-0000-000000000001`（`:35-37`）；`agents.owner_account_id`；命名空间键改 `{account}/{namespace}` | 175-178 |
| 3 | `20261013000003_conditional_key_unique.sql` | 条件唯一键（9 行） | 179-182 |
| 4 | `20261013000004_query_indexes.sql` | 查询索引（27 行） | 183-186 |
| 5 | `20261013000005_cjk_bigram_fts.sql` | CJK bigram FTS（11 行） | 187-190 |
| 6 | `20261018000006_cognitive_evidence_and_relations.sql` | 认知证据与关系表（56 行） | 191-194 |
| 7 | `20261018000007_working_memory_tasks_and_experiences.sql` | 任务与经验（54 行） | 195-198 |
| 8 | `20261018000008_coding_memory_revisions.sql` | 代码演进线程与修订（62 行） | 199-202 |
| 9 | `20261019000009_composite_namespace_fk_hardening.sql` | 复合唯一键 + 复合外键（126 行，见 §12.4） | 203-206 |
| 10 | `20261019000010_evidence_retention.sql` | `memory_evidence.expires_at` + 部分索引（16 行） | 207-210 |
| 11 | `20261021000011_consolidation_proposal_metadata.sql` | `integration_proposals.metadata jsonb not null default '{}'`（20 行） | 211-214 |

**结论：11 个文件全部登记，无遗漏、无多登记。** 每项都用 `include_str!("../migrations/<同名>.sql")` 内嵌（`:173,177,…`），因此二进制不需要运行期读取 `migrations/` 目录。

### 12.2 迁移执行器与顺序、幂等性

`run_migrations()`（`crates/store/src/postgres.rs:416-462`）：

1. 建 `_hub_migrations(version text primary key, applied_at)` 账本表（`:417-427`）。
2. 按 `MIGRATIONS` 数组顺序逐条：查账本已应用则跳过（`:429-438`）。
3. 未应用则开事务 → `set local statement_timeout = 0`（`:440-445`，索引构建不受常规语句超时限制）→ 按 `split_sql_statements` 逐语句执行（`:447-453`）→ 写账本（`:454-458`）→ commit（`:459`）。
4. `split_sql_statements`（`:228-285`）感知单引号、$$ 块与 `tag` 形式的 dollar-quote、`--` 与 `/* */` 注释，这些结构内的分号不切分。

顺序保证：注册表按版本号字典序排列，且由测试强制递增（`:3040-3048`）。

**幂等性（逐文件核对 DDL 写法）**：

- 10 个文件的建表/建索引/加列全部是 `if not exists`（脚本扫描 0 处未加保护的 DDL）。
- `0009` 的 5 处 `alter table … add constraint` 裸写在 `do ` 块内的 `if not exists (select 1 from pg_constraint where conname='…')` 保护里（`:11-28,42-59,73-85,88-95,98-115,118-125`），因此也可重放。
- `0001` 的 `create role audit_writer` 由 `if not exists (select from pg_roles …)` 保护（`:174-179`），grant/revoke 本身幂等。

### 12.3 Docker 初始化路径与 Rust runner 的先后关系（关键）

- `docker-compose.yml` 把迁移目录挂进 initdb：

```yaml
- ./crates/store/migrations:/docker-entrypoint-initdb.d:ro   # 注释：容器首次初始化时按文件名顺序自动执行全部迁移
```

- 时序：`postgres` 健康检查通过后（`depends_on: condition: service_healthy`），auth-server / mcp-server / backup-worker 才启动；它们 `connect()` 时执行 `run_migrations`。
- **但 initdb 路径不写 `_hub_migrations` 账本**（`docker-entrypoint-initdb.d` 只执行 .sql，没有任何文件插入账本行）。因此 Docker 首次部署的实际时序是：

```text
第 1 步  initdb 按文件名顺序执行 11 个 .sql（schema 建立，账本为空）
第 2 步  第一个服务 connect() → run_migrations 认为 11 条都「未应用」
第 3 步  在事务里把 11 条全部重放一遍（依赖 §12.2 的幂等性才不报错）
第 4 步  写入账本，之后各进程都跳过
```

即：**所有迁移在 Docker 部署里会被执行两次**，正确性完全依赖「每条语句幂等」。`0002` 文件头也自陈（`:10-11`）：「本文件在 runtime 迁移账本与 docker initdb 两条路径下都可能执行，每条语句均可安全重放」。
风险点：若将来有人给某条迁移写入非幂等 DDL（如裸 `create table`、裸 `alter table add column`），Docker 首次启动会在第 3 步失败，而 CI（psql 跑一次 + 另有 runner 测试）**不会暴露该问题**。

### 12.4 Migration 9 的复合外键/账户+namespace 一致性约束（原文要点）

`20261019000009_composite_namespace_fk_hardening.sql`：

| 类别 | 内容 | 行 |
|---|---|---|
| 父表复合唯一 | `uq_memories_ns_id`、`uq_memory_evidence_ns_id`、`uq_memory_tasks_ns_id`、`uq_memory_experiences_ns_id`、`uq_coding_threads_ns_id`，均为 `unique (namespace, id)` | 11-28 |
| 子表补 namespace 列 | `memory_relations.namespace`、`memory_task_experiences.namespace`（`add column if not exists … references namespaces(namespace) on delete cascade`） | 31,62 |
| 存量回填 | 从父表 `update … set namespace = m.namespace …`，随后 `set not null` | 33-38,64-69 |
| 复合外键（子 → 父） | `fk_mem_rel_from_ns` / `fk_mem_rel_to_ns` → `memories(namespace,id)` cascade；`fk_mem_rel_evidence_ns` → `memory_evidence(namespace,id)` set null；`fk_mte_task_ns` / `fk_mte_exp_ns` → `memory_tasks` / `memory_experiences` cascade；`fk_coding_threads_task_ns` set null；`fk_coding_rev_thread_ns` cascade；`fk_coding_rev_task_ns` / `fk_coding_rev_evidence_ns` set null；`fk_memory_tasks_evidence_ns` set null | 42-59,73-85,88-95,98-115,118-125 |
| 索引 | `idx_memory_relations_ns`、`idx_memory_task_experiences_ns` | 40,71 |

**语义**：由于外键是 `(namespace, id)` 复合键，跨 namespace 的引用（例如 A 命名空间的关系指向 B 命名空间的记忆）在数据库层**无法插入**——这是「账户+namespace 一致性」在 schema 层的硬约束（账户隔离由存储键 `{account}/{namespace}` 承担，见 §7.2）。约束仍然存在，未被后续迁移删除（迁移 10、11 只做加列）。

### 12.5 Migration 11：`integration_proposals.metadata` 是否已登记 —— 已登记

- 注册项：`crates/store/src/postgres.rs:211-214`（`20261021000011_consolidation_proposal_metadata`）。
- SQL 实体：`add column if not exists metadata jsonb not null default '{}'::jsonb`（`:15-16`）+ 列注释（`:18-19`）。
- 文件头声明的兼容性（`:8-13`）：旧行读作空对象；不动 `kind` 的 CHECK；`integration_proposals` 不在备份载荷内，**不需要**升快照版本；可重复执行。
- 与代码的对应：`engine.rs:714-731` `proposal_metadata()` 写入的正是 relation / candidate_source / reason_code / mode / requested_mode / strategy_version / fallback_applied / similarity，与列注释一致。

### 12.6 注册守卫测试在哪里

| 测试 | 位置 | 断言内容 |
|---|---|---|
| `test_migrations_registered` | `crates/store/src/postgres.rs:3037-3060` | `MIGRATIONS.len() == 11`（`:3038`）；首项以 `20261011` 开头（`:3039`）；版本号严格递增（`:3041-3048`）；每条迁移都能被切分器切开且每段以分号结尾（`:3051-3059`） |
| `test_every_migration_file_is_registered` | `crates/store/src/postgres.rs:3065-3084` | 遍历 `migrations/` 目录的每个 .sql，断言 `MIGRATIONS` 中存在同名项（`:3077-3081`）；并断言 **文件数 == 登记数**（`:3083`） |
| `test_split_sql_statements_dollar_quote` / `test_split_sql_statements_comments_and_strings` / `test_split_full_migration_keeps_do_block_intact` | `:3087-3122` | 切分器对 dollar-quote / 注释 / 字符串的行为 |

另外 `crates/store/src/postgres.rs:3173` 起的 `#[cfg(all(test, feature = "pg-integration"))]` 模块含 3 个 `#[ignore]` 用例（`:3214,3267,3355`，均标注「需要 DATABASE_URL」），用于真实库回归；本机无 PG → **未执行**。

### 12.7 P0.3 未验证项

| 任务书要求 | 状态 |
|---|---|
| 「历史数据库从最早受支持版本升级到当前版本是否可连续完成」 | **未实测**（需要真实 PG 与旧版 schema 数据）。可确认的静态事实：注册表按版本递增且测试强制顺序、所有 DDL 幂等、每条迁移单独事务。 |
| 「新增的认知记忆字段和关系是否有真实 schema 支撑」 | 静态可确认：迁移 6/7/8 建了 memory_evidence / memory_relations / memory_tasks / memory_experiences / memory_task_experiences / coding_threads / coding_revisions，迁移 9 补齐复合外键；但**字段级逐一比对（Rust 结构体字段 vs 表列）未逐列完成**，本报告不声称已核对全部列。 |
| 「migration 的真实顺序和幂等性」 | 已按 SQL 原文核对（§12.2），结论成立。 |

---

## 13. 测试资产清单

> **口径说明**：本节的数量是**静态计数**——对源码 grep `#[test]` / `#[tokio::test]` 属性得出，**不是运行结果**。本次未执行任何测试（本机禁止运行 cargo，另有一个 release 构建占用 target 目录），运行结论由 Lead 在其代码修改完成后统一执行。

### 13.1 测试数量级（静态计数，含 `#[ignore]`）

| crate / bin | .rs 文件数 | 测试函数数（静态计数） | 备注 |
|---|---|---|---|
| `crates/mem-core` | 17 | **92** | 含 `tests/credential_pattern_guard.rs`（凭证字面量门禁） |
| `crates/mcp` | 13 | **79** | 含 `tests/cjs_adapter_test.rs`（464 行） |
| `crates/store` | 8 | **53** | 含 `account_scope_tests.rs`（641 行）；3 个 PG 用例 `#[ignore]` |
| `crates/common` | 11 | **52** | 配置校验、cron、幂等、限流 |
| `crates/consolidation` | 14 | **50** | 含 `tests/consolidation_eval.rs`（955 行，8 个评测/门禁用例） |
| `crates/auth-web` | 4 | **42** | OAuth 流程与账户控制台 |
| `crates/retrieval` | 4 | **38** | 含 `tests/eval_bench.rs`（2788 行）+ `tests/eval_suite.rs`（763 行） |
| `crates/worker` | 7 | **17** | 5 个 job + scheduler 单测 |
| `crates/embed` | 2 | **12** | wiremock 驱动的 HTTP 契约测试 |
| `crates/auth-core` | 5 | **11** | PKCE / 令牌族 |
| `crates/authz` | 3 | **5** | TokenClaims 校验 |
| `crates/bins/mcp-server` | 1 | **3** | 装配/内省单测 |
| `crates/bins/backup-worker` | 1 | **2** | tick 算术 |
| `crates/bins/auth-server` | 1 | **1** | — |
| **合计** | **97** | **约 457** | 全仓 Rust 源码合计 48,196 行 |

`#[ignore]` 共 **9** 处：`crates/retrieval/tests/eval_bench.rs:2128,2611,2668`、`crates/retrieval/tests/eval_suite.rs:684`、`crates/store/src/idempotency.rs:178,204`、`crates/store/src/postgres.rs:3214,3267,3355`。

### 13.2 `eval_bench`：测试函数与门禁口径

| 测试 | 行 | 门禁口径 |
|---|---|---|
| `test_retrieval_bench_gate` | 2036 | 逐查询配对比较；**绝对下限 1pt + 配对 z 检验 z≥2.0**（`PAIRED_ABS_FLOOR=0.01`、`PAIRED_Z_CRITICAL=2.0`，`:1946-1947`）；配对样本匹配率必须 ≥95%（`:1978-1983`）；基线缺 per_query 直接失败（`:1984-1989`） |
| `test_retrieval_bench_ablations` | 2129 | 消融对比（`#[ignore]`，`:2128`——耗时长，默认不跑） |
| `test_ood_rejection_gate` | 2309 | 域外拒绝：forbidden_rate / first_hit_forbidden_rate / negative_hit_rate |
| `test_business_constraint_gate` | 2384 | 业务约束（场景/层级/时间有效性） |
| `test_min_score_calibration` | 2612 | 阈值标定（`#[ignore]`，`:2611`） |
| `test_retrieval_bench_postgres` | 2669 | 真实 PG 上的同一套评测（`#[ignore]`，`:2668`） |

分类别门禁：样本量 ≥ `PAIRED_MIN_CATEGORY_N = 30` 才判定（`:1949,2017`），否则只打印。

### 13.3 `eval_suite` 与 `consolidation_eval`

| 套件 | 测试 | 行 | 说明 |
|---|---|---|---|
| `crates/retrieval/tests/eval_suite.rs` | `test_full_retrieval_eval_suite` | 163 | 内存存储全链路评测 |
| | `test_postgres_retrieval_eval_suite` | 686 | 真实 PG 版（`#[ignore]`，`:684`） |
| `crates/consolidation/tests/consolidation_eval.rs` | `test_consolidation_modes_quality_and_safety_gates` | 671 | A/B 双模式质量与安全门禁（见下） |
| | `test_rules_mode_makes_zero_embedding_calls` | 767 | B 模式嵌入调用 = 0（call_count / text_count 双断言） |
| | `test_hybrid_mode_falls_back_when_provider_not_ready` | 801 | 伪向量 → 降级并如实上报 |
| | `test_hybrid_mode_errors_when_fallback_is_error` | 825 | fallback=error 时拒绝执行 |
| | `test_account_isolation_is_enforced_in_both_modes` | 846 | 两种模式下的账户隔离 |
| | `test_repeated_scan_does_not_recreate_open_proposals` | 890 | 幂等 |
| | `test_dry_run_does_not_change_business_data` | 913 | dry_run 无副作用 |
| | `test_fixed_clock_is_stable` / `test_scope_is_unchanged_for_consolidation_tools` | 941 / 952 | 时钟稳定 / 作用域未变 |

`test_consolidation_modes_quality_and_safety_gates` 的强制门槛（`:715-763`）：

1. `wrong_merges == 0`（两种模式，`:718-719`）；
2. `hard_violations == 0`（`:722-723`）；
3. B 模式 `embedding_calls == 0`（`:726`）；
4. `b.dup_recall() >= 0.5`（`:729-733`）且 `a.dup_recall() >= b.dup_recall()`（`:734-737`）；
5. **有真实嵌入端点时**才要求 `a.paraphrase_recall() > b.paraphrase_recall()`；无端点时只打印「未验收」（`:740-753`）——与任务书「不得用伪向量作为语义验收」一致；
6. 重复扫描不得重建 open 提案（`:756-763`）。

### 13.4 `eval_baseline.json` 元数据（`crates/retrieval/tests/data/eval_baseline.json`，159,856 B）

| 字段 | 值 |
|---|---|
| `embedder` | `test-embedder` |
| `semantic_ready` | `false` |
| `dataset_fingerprint` | `9f5b3b075a558371` |
| `dataset_docs` / `dataset_queries` | 480 / 600 |
| `config` | `default(strict, w=0.5/0.2/0.1/0.1/0.1, λ=0.7, min_score=0.65)` |
| `overall`（冻结值） | recall@1=0.6815、recall@5=0.9537、recall@10=0.9722、mrr@10=0.7989、ndcg@10=0.6649、forbidden_rate=0.1963、first_hit_forbidden_rate=0.0056、negative_hit_rate=0、p50=10.12ms、p95=28.45ms |
| 结构 | dataset_fingerprint / dataset_docs / dataset_queries / embedder / semantic_ready / config / overall / by_scene / by_category / per_query（per_query 600 条逐查询记录，配对门禁依赖它） |

**关键解读**：基线是在 `semantic_ready=false`（伪向量）下冻结的。因此它只能作为**词法路径（含「向量维度被剔除」的打分口径）**的回归基线，**不能**作为「语义检索质量」的验收证据；这与任务书约束 8「不得将 Mock/Pseudo embedding 的结果作为真实语义能力的验收结果」一致。基线写入开关为 `HUB_EVAL_WRITE_BASELINE=1`（`eval_bench.rs:1986` 的错误文案提及）。

### 13.5 测试资产的缺口

- 没有针对 stdio 传输的集成测试（§9.3 的缺陷因此未被测试捕获；`transport.rs` 的测试都在 `#[cfg(test)]` 内直接构造 state 并传 `Some(auth)`）。
- 没有针对 `BackupJob` 与 `DefaultBackupService` 快照格式一致性的测试（§10.4 的不一致因此未被捕获）。
- 没有针对「配置项真的被代码读取」的测试（§6.3 的 `HUB_CHAT_EXTRACTOR` 死变量因此未被捕获）。
- `JobScheduler`、`ChatCognitiveJob`、`ExperienceDistillJob` 只有自身单测，没有「是否被装配」的断言（§8.3）。

---

## 14. 审计结论

### 14.1 已满足

| 结论 | 证据 |
|---|---|
| Workspace 14 成员与 Cargo.toml 声明完全一致，无循环依赖 | `Cargo.toml:1-18` + 各 crate Cargo.toml |
| feature 面收敛：仅 jemalloc / adapter-cjs / pg-integration 三个，pg-integration 纯测试门控 | §2.1、§2.4 |
| 嵌入器有单一构造入口 `build_from_config`，provider 只支持 mock / http，http 缺 endpoint 即失败（不静默退回 mock） | `embed/src/lib.rs:122-154`；`config.rs:542,553` |
| 伪向量被显式识别：`provides_semantic_signal=false` → 打分剔除 sim 维度、健康检查声明未就绪 | `infra.rs:571`；`hybrid.rs:59,403`；`config.rs:403-416`；`mcp-server/src/main.rs:55-61` |
| 检索执行顺序确定且可审计：嵌入 → 双路并行 → 合并 → 五维 → min_score → MMR | `hybrid.rs:493-598` |
| min_score 在 MMR 之前应用（有回归测试） | `hybrid.rs:587-591`；`hybrid.rs:1433` |
| 软超时只丢超时的一路，两路皆空才降级列表 | `hybrid.rs:554-576` |
| 整合 A/B 共用同一决策层（关系分类、冲突检查、提案、自动合并门槛只有一份实现） | `candidates.rs:1-11`；`engine.rs:280-291` |
| B 模式嵌入调用恒为 0（三层结构保证 + 两处守卫测试 + 评测门禁） | §5.5 |
| 语义候选永不自动合并（关系门槛先于分数门槛） | §5.6 |
| 默认 `HUB_CONSOLIDATION_MODE=rules`，且配 `HUB_EMBED_PROVIDER=http` 不会自动切 hybrid | `config.rs:278`；`common/consolidation.rs:21`；`mcp-server/src/main.rs:137-145` |
| 无生成式 LLM 依赖与调用点 | §6.3 |
| MemoryStore 抽象不反向依赖具体存储；AccountScopedStore 覆盖 10 个 trait 且 fail-closed | `infra.rs:21-446`；`account_scope.rs:119-775` |
| 11 个迁移与注册表一一对应，守卫测试断言数量与文件名一致 | §12.1、§12.6 |
| Migration 9 的复合唯一键/复合外键仍完整存在 | §12.4 |
| Migration 11（integration_proposals.metadata）已登记且与代码写入字段一致 | §12.5 |
| 备份格式最高 v4，restore 兼容 v1–v4 校验和，replace 前自动建安全备份 | `backup_service.rs:29,208-267,520-549,589-596` |
| 关键安全能力：save_if_version 乐观锁、revise 原子修订、证据写入前脱敏 + 保留期限、默认拒绝阈值 | `infra.rs:68-110`；`chat_cognitive_service.rs:89-104`；`config.rs:52,267` |

### 14.2 存在缺口（可对应到具体文件）

| # | 缺口 | 证据 | 影响 |
|---|---|---|---|
| G1 | **stdio 模式下 tools/call 恒失败**：handle_raw_frame 传 `auth=None`，注释却称「已认证的本地桥接」；HUB_STDIO_LOCAL_AGENT 读取即丢弃 | `mcp/src/transport.rs:454-456,361-376`；`bins/mcp-server/src/main.rs:276-279` | 本地/CLI 集成不可用；注释误导 |
| G2 | **memory_integrate_scan 的 hybrid 恒降级**：DefaultIntegrationService 从未被注入嵌入器（而 memory_consolidate 注入了） | `bins/mcp-server/src/main.rs:154-167` vs `:205`；`consolidation/src/service.rs:66-70` | 两个整合入口能力不一致 |
| G3 | **认知提案无持久化**：CognitiveProposalRecord 无 store trait、无表；cognitive_proposal_resolve 实际操作整合提案 | `infra.rs:237-251`；`chat_cognitive_service.rs:33,204-306`；`mcp/src/tools.rs:975-986` | cognitive_chat_propose 返回的提案无法事后评审；两套状态机混用 |
| G4 | **定时备份与 MCP 备份格式不一致**（payload v1 仅 entries vs v4 含 8 类实体），模块注释却称「同一套格式与校验和算法」 | `worker/src/backup_job.rs:1-4,63-67,116-119` vs `backup_service.rs:29,271-294` | 用定时快照做 replace 恢复会丢掉证据/关系/任务/经验/代码演进 |
| G5 | **restore 无自动回滚、清空阶段吞错误**：`let _ =` 忽略删除失败；无事务 | `backup_service.rs:598-650` | 部分失败留下半恢复状态，只能人工用安全备份回滚 |
| G6 | **all_in_namespace 无界**，且 IntegrationScanJob 对同一命名空间重复全量装载两次 | `postgres.rs:1130-1150`；`in_memory.rs:403-419`；`worker/src/integration_job.rs:56-64` + `engine.rs:236-244` | 大命名空间内存峰值不可控 |
| G7 | **后台整合扫描无配置开关**（硬编码 24h、启动即跑），且无跨进程互斥/租约 | `bins/backup-worker/src/main.rs:110-113,175-186`；`engine.rs:36-47` | 个人轻量部署无法「改为按需执行」 |
| G8 | **JobScheduler / ChatCognitiveJob / ExperienceDistillJob 未被装配** | §8.3 | worker 约 760 行死路径；调度逻辑两套（一套未用） |
| G9 | **HUB_CHAT_EXTRACTOR 是死变量**（只出现在告警字符串里） | `bins/mcp-server/src/main.rs:176`；全仓 grep 仅此一处 | 运维被引导去配置一个无效开关 |
| G10 | **candidates/hybrid.rs 文档宣称的 MemoryStore::similar ANN 通道不存在**（实现是穷举/LSH） | `consolidation/src/candidates/hybrid.rs:16-17,80` vs 该目录无 store 调用 | 架构描述与实现不符 |
| G11 | **认知三服务不使用可注入 Clock**，直接用 Utc::now()（共 8 处） | §6.2 | 时间相关行为不可在测试中固定；与 HybridRetriever / MemoryService 风格不一致 |
| G12 | **Coding/Working 的 source_evidence_id 无存在性校验**（对比 thread_id 有校验） | `coding_memory_service.rs:91-130`；`working_memory_service.rs:158-160` | 可写入悬空证据引用 |
| G13 | **Docker initdb 与 runtime runner 会执行两遍迁移**，账本不共享 | `docker-compose.yml`（initdb 挂载）vs `postgres.rs:416-462` | 依赖「全部幂等」；CI 无法覆盖该路径 |
| G14 | **Dockerfile 的 feature 写法与 CI 不一致**（根级 --features vs 包级限定） | `Dockerfile:56` vs `.github/workflows/ci.yml:146-153` | 镜像构建口径未在本机验证 |
| G15 | **tools/list 不做作用域过滤** | `transport.rs:340` | 已连接调用方可见全部 28 个工具与 schema（不含数据） |
| G16 | 本机无法运行 CI 全部门禁（--all-features / docker / psql） | §11.3 | 本机「绿」不等于 CI「绿」 |

### 14.3 阻断项（会阻断「轻量化个人模式」验收，或使既有承诺不成立）

| # | 阻断项 | 为什么阻断 | 证据 |
|---|---|---|---|
| B1 | 没有部署形态开关（当前 HEAD 无 `HUB_DEPLOYMENT_PROFILE`），也没有任何 feature/环境变量能关闭「写入必算嵌入」「检索必调嵌入」「整合扫描固定 24h」 | 任务书 A 形态（本地规则、按需执行、零外部调用）无法在**当前 HEAD** 上由配置实现 | `config.rs:236-289`（无该字段）；`memory_service.rs:285,304,437`；`hybrid.rs:520-522`；`backup-worker/src/main.rs:175` |
| B2 | mcp-server 无条件构造嵌入器：即使只想跑纯关键词检索，也必须让 HUB_EMBED_PROVIDER 合法（mock 或 http），没有「关闭嵌入」的取值 | 个人轻量部署要么带 mock（伪向量噪声，已被剔除打分）要么必须配 http 端点 | `bins/mcp-server/src/main.rs:50`；`config.rs:542`；`embed/src/lib.rs:150-152` |
| B3 | stdio 传输不可用（G1） | 任何依赖 stdio 的本地 Agent 集成都无法调用工具 | `transport.rs:456,361-376` |
| B4 | 无真实 PG / 无 docker 的环境下，P0.3 的「升级连续性」与全部 PG 集成测试无法验证 | 迁移与备份恢复的最终正确性只能在真实库上确认 | §12.7、§11.3 |
| B5 | 评测基线是伪向量冻结的（semantic_ready=false） | 不能用它证明任何「语义能力」，也不能用它衡量向量模式收益 | `eval_baseline.json` 元数据 |

### 14.4 对后续轻量化实施的最小落点建议（仅基于本次审计事实）

1. **装配层**：把「是否产生/使用向量」从 `embed_provider` 字符串里拆出来（当前 mock / http 二选一，且 mcp-server 无条件构建），否则 B2 无解。
2. **写入/检索路径**：`embed_content`（`memory_service.rs:243`）与查询嵌入（`hybrid.rs:520`）是仅有的两处嵌入调用点，加「关闭」分支即可覆盖全部写入/检索影响面。
3. **后台作业**：`backup-worker` 的整合扫描（`main.rs:175-186`）需要开关 + 启动不立即跑；`all_in_namespace`（`postgres.rs:1130`）需要上限或分批，否则 G6/G7 无解。
4. **不要在文档里先写后做**：G1、G4、G9、G10 都是「注释/告警/文档承诺了代码没有的行为」，本轮改造应同步修正文案，避免下一轮审计重复记录。

---

## 附录 A：与任务书参考事实列表的逐条核对

| # | 参考事实 | 核对结果 | 差异/补充 |
|---|---|---|---|
| 1 | 无 `HUB_DEPLOYMENT_PROFILE`（全仓 grep 仅任务书） | **成立（HEAD 基线）** | 但审计期内工作区已出现未提交实现（`crates/common/src/profile.rs` + `config.rs` 改动，见 §0.2）。结论只对 HEAD `3048efc` 成立 |
| 2 | `HUB_EMBED_PROVIDER` 仅支持 mock / http | **成立** | 两处校验：`config.rs:542`、`embed/src/lib.rs:150-152` |
| 3 | mcp-server 无条件 build_from_config；检索每次必调 embed；写入每次必算嵌入 | **全部成立** | 补充：backup-worker 仅 hybrid 时构建；auth-server 完全不构建（与「三个二进制各自调用」的表述不符） |
| 4 | MMR 无条件执行 | **成立** | 补充：λ 可配（默认 0.7），但相似度矩阵与贪心始终计算 |
| 5 | 11 个 migration 全部登记，守卫测试断言 MIGRATIONS.len()==11 | **成立** | 守卫测试有两处：`postgres.rs:3037`（数量+顺序+切分）与 `:3065`（文件↔登记一一对应） |
| 6 | ChatCognitiveJob / ExperienceDistillJob / JobScheduler 无二进制装配 | **成立** | 证据见 §8.3（仅自身单测 + lib.rs 再导出） |
| 7 | 28 个 MCP 工具 = 18 基础 + 10 认知 | **成立** | 完整表见 §9.2 |
| 8 | 备份格式最高 v4，v1–v4 校验和兼容 | **成立** | 但 BackupJob 的定时快照写的是 version 1（§10.4）——格式能力有 v4，不代表生产所有快照都是 v4 |
| 9 | 默认 HUB_CONSOLIDATION_MODE=rules；consolidation_eval.rs 中 test_rules_mode_makes_zero_embedding_calls 是守卫 | **全部成立** | 另有第二处守卫 `consolidation/src/lib.rs:161` 与评测门禁 `consolidation_eval.rs:726` |
| 10 | 本机无 docker、无 psql、5432 无监听 | **全部成立** | 实测见 §0.3 |
| 11 | 本机 cargo 1.98.1 / macOS 27 arm64 / 路径含空格 | **全部成立** | 实测见 §0.3 |

## 附录 B：代码与注释/文档不一致清单（汇总）

| # | 位置 | 声明的行为 | 实际行为 |
|---|---|---|---|
| 1 | `crates/mcp/src/transport.rs:454-455` | 「此处视为"已认证的本地桥接"」 | auth=None，tools/call 恒 32001（`:456` → `:361-376`） |
| 2 | `crates/bins/mcp-server/src/main.rs:276` | 「stdio 模式：不带 Bearer 的本地桥接（仅限回环地址）」 | 既无认证注入，也无回环校验（HUB_STDIO_LOCAL_AGENT 被丢弃，`:279`） |
| 3 | `crates/bins/mcp-server/src/main.rs:176` | 「请通过 HUB_CHAT_EXTRACTOR 配置」 | 全仓无代码读取该变量 |
| 4 | `crates/consolidation/src/candidates/hybrid.rs:16-17,80` | 第三通道是 MemoryStore::similar 的 ANN 召回 | 无任何 store 调用；实际为穷举/LSH |
| 5 | `crates/worker/src/backup_job.rs:1-4` | 与 DefaultBackupService 同一套快照格式与校验和算法 | payload v1 仅 entries；校验和为 v1 变体 |
| 6 | `crates/mem-core/src/backup_service.rs:207,213,226,245` | 「规范校验和」 | 实为结构体字段序的 serde_json::to_vec + SHA-256（非规范化 JSON） |
| 7 | `crates/mcp/src/transport.rs:797-798` | — | 重复的 `#[cfg(test)]` 属性（无害但属笔误） |
| 8 | `docs/upgrade/REPOSITORY_AUDIT.md` §4.1 | `similar(&self, namespace, embedding, k, kinds)` | 现签名是 `similar(&self, filter: &MemoryFilter, embedding, k)`（`infra.rs:124-129`），旧文档已过期 |
| 9 | 本报告 §13 的口径 | — | 测试数量为**静态 grep 计数**，非运行结果；运行结论由 Lead 统一执行 |

---

*本报告仅记录本次检出（HEAD `3048efc`，2026-10-21 17:33 +0800）的实际源码事实；所有行号均可用 `git show HEAD:<path> | sed -n '<n>p'` 复核。凡标注「未实测 / 未验证」的项，均未在本机执行。*

[![powered by KimiCode](../../.github/assets/kimi-badge.svg)](https://www.kimi.com/code)
