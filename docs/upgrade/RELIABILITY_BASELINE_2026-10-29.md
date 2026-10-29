# Memory Hub 可靠性升级基线审计

**审计阶段：** Stage 0（只读基线）
**审计时间：** 2026-10-29 10:16–10:30 +08:00
**目标工作树：** `/tmp/memory-hub-reliability`，分支 `reliability/productionization`
**基线源码：** GitHub `main` / commit `d35f033230c46d989de527e554d6e40491be2dd3`（与原工作区 HEAD 相同）
**环境：** macOS 27.0 arm64；rustc 1.98.1；cargo 1.98.1；未发现本机 PostgreSQL、Docker、Podman 或 `psql` 工具。

## 1. 仓库、分支与镜像

| 项目 | 结果 |
|---|---|
| GitHub 默认分支 / HEAD | `main` / `d35f033230c46d989de527e554d6e40491be2dd3` |
| CNB 默认分支 / HEAD | `main` / `4b140e038d81cac27b50da2f3b07b20590a68fa3` |
| 当前分支共同祖先 | `9b6d6f38749b867fa852c57580574ee0b1915149` |
| 两端提交关系 | CNB 与 GitHub 各有 55 个非 merge commit 的不同 SHA；55 个 patch-id 全部一一对应，没有发现仅一端存在的 patch。分支顶端同主题 commit 的 parent / commit object 不同，属于重新生成的分叉历史，不是直接 fast-forward 镜像关系。 |
| `v0.1.0` peeled commit | 两端均为 `ee507e94b0781095d22e4cb7df725ee022bd1a23` |
| 关键文件 SHA-256 | `Cargo.lock`、migration 015、`backup_service.rs`、GitHub CI 与 `.cnb.yml` 在两个 HEAD 的哈希一致。 |
| 远端访问 | `origin`（GitHub）与 `cnb` 均可用只读 `ls-remote` / fetch 查询；本次未 push、部署或改写远端。 |

**发布源结论：** 当前头部内容的关键文件完全一致且提交补丁集合等价，但 55+55 的不同 commit SHA 与非共同祖先不适合直接宣称某端是机械镜像。本阶段未据此单方面定权威发布源；Task 6 应根据构建、合并保护和生产部署的实际证据确认唯一源，并避免 merge 两端历史造成重复 patch。

## 2. Workspace、features 与 CI

- 根 `Cargo.toml` 有 15 个 workspace member；edition 2021，MSRV 1.80。主要 feature 包括 MCP `adapter-cjs`、store `pg-integration`、三个服务端二进制的 `jemalloc`。
- CI 事实来源为 `scripts/ci/quality-gate.sh`：`cargo fmt --all -- --check`；`cargo clippy --all-targets --all-features -- -D warnings`；`cargo test --workspace --all-features`。GitHub CI 另外运行文档构建、PG15 + pgvector 集成 job、cargo-deny/audit 与带 jemalloc 的 release 构建。
- `.cnb.yml` 的 PR / 分支质量门禁复用同一脚本；CNB main push 还构建、推镜像并部署，另有 tag 定版制品与人工部署入口。质量命令一致不代表生产部署权威源已经治理完成。

## 3. Migration 清单与执行器

- 当前有 17 个 migration，登记顺序从 `20261011000001_initial_schema` 到 `20261028000017_memory_collections`；文件与 `MIGRATIONS` 静态清单有守卫测试。
- `crates/store/src/postgres/mod.rs::run_migrations` 使用 `_hub_migrations` 账本；每个 migration 在独立事务中先取得 `pg_advisory_xact_lock`，锁内检查版本、执行 SQL、写账本、提交。失败时当前 migration 的事务回滚；以前已提交的 migration 保留。该执行器不能直接容纳 `CREATE INDEX CONCURRENTLY`。
- GitHub CI 另在 PostgreSQL service 中按文件名用 `psql -f` 逐个执行 migration，然后串行跑 `store --features pg-integration -- --ignored`。这是非事务逐文件路径，与运行时执行器行为不完全相同；需要保留两种路径的兼容验证。
- Migration 015 `20261027000015_vector_index_halfvec_partial.sql` 使用 `SET LOCAL maintenance_work_mem`，创建 `embedding::halfvec(1024)` + `halfvec_cosine_ops` 且状态为 active/archived 的 HNSW 部分索引，并移除旧索引，调整 autovacuum。注释明确假设空表/小表；大表应在维护窗口采取 concurrent build。此处尚未用真实 PG 验证计划、锁、失败中断或有效索引状态。

## 4. 备份与恢复真实路径

| 路径 | 实现 | 载荷 / 校验 |
|---|---|---|
| MCP 手动创建 | `crates/mcp/src/tools/backup.rs` → `DefaultBackupService::create` (`crates/mem-core/src/backup_service.rs`) | 当前 `BACKUP_PAYLOAD_VERSION=5`、`payload_of` 与 `checksum_v5`。覆盖 entries、evidences、relations、tasks、experiences、experience_versions、task_experiences、coding_threads、coding_revisions。 |
| 定时创建 | `BackupJob::run_once` (`crates/worker/src/backup_job.rs`) → `DefaultBackupService::system_snapshot` | 两类 namespace 路径都调用相同 v5 payload/checksum，恢复服务可复用；按 namespace 列出并保存，随后执行保留数量/TTL 清理。 |
| 恢复 | `DefaultBackupService::restore` | 先提取各类 payload 并用 v5 到 v1 checksum 比对，再按 DryRun/Merge/Replace 写入。兼容测试名为 `test_v1_to_v4_historical_sample_restore_and_checksum_verification`，实现已有 v5 校验逻辑；测试是否覆盖 v5 的独立历史 fixture 需在 P0-A 阶段核实。 |
| Replace 保护 | `create_safety_backup`，随后清理并重建 | 先保存受影响 namespace 的 v5 安全备份；但是当前清理路径对读取错误使用 `unwrap_or_default`、忽略删除错误，实体恢复按设计容许逐条失败并返回计数。没有整体事务/原子替换保证，必须验证失败时状态与成功报告语义。全量 `*` 安全备份会跳过并告警，要求操作者事先手动全量备份。 |
| 保留 / 删除 | worker 的 `BackupJob::prune` 与备份服务 `prune` | 按 namespace 保留最新数量和 TTL；应继续检查系统 worker 与人工服务的 ACL / 全量范围语义。 |

现有代码已不存在“定时仍是 v1 payload”的已知差异：scheduled test 名称与若干注释仍写 v4，但测试断言读取 v5 version 且两入口实际都用 v5。旧文档 `docs/upgrade/REPOSITORY_AUDIT.md` 仍称 v1，属于过期审计记录，不代表当前实现。任务书所称手动 v4 / scheduled v1 不适用于当前 HEAD。

## 5. Proposal 并发与整合

- `crates/consolidation/src/engine.rs` 默认 `Rules`；策略只允许 exact/canonical duplicate 自动合并，semantic candidate 不会仅因高相似度自动合并。
- `PostgresHubStore::open_exists` 在 `crates/store/src/postgres/integration.rs` 按 namespace、open 状态、kind 与 member UUID 集合查询；随后 `save_proposal` 仅 `ON CONFLICT (id)` 更新，没有以 namespace/kind/member identity 为业务唯一键的原子约束。初始 schema `integration_proposals` 只有 PK(id) 与 `(namespace,status)` 索引。
- 进程内 namespace mutex 与 consolidation 测试可阻止同进程/同扫描重复，但不能单凭这些证明多个 Postgres 连接或进程竞争安全。CI 现有 PG 集成测试未发现 proposal contention 覆盖。Task 2 必须按实际 member_ids 顺序规范化和 open 状态定义设计原子键，并在独立数据库上并发复现。

## 6. 后台任务和部署形态

- `backup-worker` 默认 Production 常驻周期；Lite 模式拒绝不带 `--once` 的常驻进程。Lite compose 默认 `HUB_EMBEDDED_WORKER=1`，在 `mcp-server` 进程内运行 `EmbeddedJobs`，与独立 worker 共用备份/整合/证据作业，但部署时必须避免两处同时启用或依赖租约正确互斥。
- `EmbeddedJobs` 实际注册备份、整合扫描、过期/证据清理；没有注册 `ChatCognitiveJob` 与 `ExperienceDistillJob`。两者目前是可直接调用的执行器，有有限瞬态重试、幂等服务调用和日志，不是生产周期调度队列；没有发现可领取任务表、持久租约或状态查询接口。
- Chat 抽取器的生产真实性/配置要进一步看 service/provider 实现；本轮不据 job 名称推断真实 LLM 已接入。默认规则整合与 embedding 分开。

## 7. Embedding 与检索评测

- `crates/embed/src` 有 Mock、HTTP OpenAI-compatible、Disabled embedder。MCP server 启动会基于 `semantic_ready()` 明确告警伪向量不等于语义能力。
- `crates/retrieval/tests/eval_bench.rs` 的程序化数据集指纹为 `9f5b3b075a558371`，包含配对指标、Recall@1/5/10、MRR@10、nDCG@10、硬负例/域外与业务约束门禁；默认 eval embedder 可以是 TestEmbedder，报告写 `semantic_ready=false`。真实 HTTP embedding 由 `HUB_EVAL_EMBED_ENDPOINT/MODEL/DIM/API_KEY` 配置，设 `HUB_EVAL_REQUIRE_SEMANTIC` 才硬性要求真实语义信号。
- CI 质量门禁实际执行了 eval bench 默认门禁、OOD 与 business-constraint gate。当前 `.env` 环境变量名检查没有发现 `HUB_EVAL_*`，故真实模型评测未运行；默认 eval 分数只代表词法/伪向量管线行为，不作为语义质量报告。

## 8. OAuth、MCP、ACL 与敏感数据的审计入口

- OAuth claims 验证位于 `crates/authz/src/validator.rs`，检查 `exp`、issuer、audience 与 account claim，并由 scopes/grants 生成 `AuthCtx`。
- MCP schemas 位于 `crates/mcp/src/schema.rs`；HTTP MCP dispatch 在 `crates/mcp/src/transport/dispatch.rs` 按 `required_scope` 强制校验；具体工具还在工具服务层检查 scope / namespace grant。MCP 当前真实工具数量测试断言 38。
- 证据脱敏在 `crates/mem-core/src/redaction.rs`，凭证输入拒绝与编码/路径校验在 `domain.rs` 和各 memory service；已有 token/连接串/个人信息测试。仅凭这些入口尚未证明 backup/export、结构化错误日志和恢复边界都满足全任务书要求，Task 7 需沿实际 dispatch 与存储继续验收。

## 9. 基线命令结果

| 命令 | 环境 / 退出码 | 结果 | 备注 |
|---|---|---|---|
| `scripts/ci/quality-gate.sh` | macOS 27 arm64、rustc 1.98.1、cargo 1.98.1；exit 0 | fmt、全 targets/all-features clippy、workspace/all-features tests 均通过；日志合计 548 passed、0 failed、13 ignored | ignored 测试未计作通过；测试中包含 `eval_bench` 3 个门禁和 `eval_suite` 冒烟，均非真实 embedding 语义验证。 |
| PG 集成 `cargo test -p store --features pg-integration -- --ignored --nocapture --test-threads=1` | 未执行 | 未运行 | 本机无 Docker、Podman、PostgreSQL 工具；localhost:5432 关闭；无 `DATABASE_URL`。CI 使用 PG15 + pgvector 的测试服务。 |
| 真实 embedding 专项 | 未执行 | 未运行 | 没有配置 `HUB_EVAL_EMBED_ENDPOINT/MODEL/DIM/API_KEY`；不得将默认伪向量门禁当作真实语义结果。 |

工作树起始时干净（隔离 worktree），原主工作区已有的 `.gitignore` 修改和未跟踪 `web/` 保持未动。

## 10. Stage 0 结论与后续优先核验

1. P0-A 的备份入口已共用 v5 全实体定义；风险集中在 Replace 清理错误被吞、恢复部分成功、旧版兼容语义及 coverage fixture，而非版本 v1/v4 差异。
2. P0-B 的 PG 写入有查询后插入竞争窗口，业务唯一键尚无数据库约束；需要两独立连接/进程的真实 PG 复现。
3. cognitive/experience jobs 存在但未进周期调度；当前周期作业与 Lite/Production 模式需按真实部署配置验收。
4. Migration 015 在事务型运行时执行器中普通建 HNSW 并先删旧索引；真实生产大表升级不能仅凭本机静态测试放行，必须有隔离 PG 证据和维护窗口回滚/前滚方案。
5. 本轮没有访问生产数据库、触发远端 CI、运行真实模型评测、部署或改动 CNB/GitHub。

Stage 0 产品代码零改动。后续每个 P0/P1/P2 阶段仍需独立测试、独立提交和更新最终报告；当前基线通过不代表整个升级任务完成。

<!-- kimi-badge -->
[![powered by KimiCode](../../.github/assets/kimi-badge.svg)](https://www.kimi.com/code)
