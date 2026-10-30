# 可靠性修复：完成状态与本次补完（2026-10-29）

本文回答一个问题：**任务书里那些 P0/P1 项，现在到底哪些有证据、哪些还缺。**

- 主体实现已在 `reliability/productionization` 分支完成并进入 `main`
  （GitHub/CNB 均已包含，CI run `40828521543` 四个 job 全绿）；
- 本文记录在此基础上**本轮补完的缺口**，以及仍未验证的部分。

> 证据口径：`源码确认` / `单测复现` / `CI 实测` / `未验证`，四者不混用。
> 文档里写的“已完成”，必须能对应到一条命令或一个 commit。

---

## 1. 基线

| 项目 | 结果 |
|---|---|
| 任务书参考基线 | `cae9e87f`（CNB 公开页面） |
| 实际 GitHub `main` | `8bdccfe`（含 21 个可靠性提交） |
| 本地工作区起点 | `d35f033`，落后 `origin/main` 21 个提交 |
| 本地分支 | `main`（未动）+ `reliability/completion`（本轮工作分支，基于 `origin/main`） |
| 工作区杂项 | `.gitignore` 本地改动（`web/node_modules`、`web/dist`）+ 未跟踪 `web/`，本轮未触碰 |
| 本机环境 | macOS arm64，rustc/cargo 1.98.1，**无 docker / psql / DATABASE_URL** |
| 已知本地限制 | 仓库路径含空格，`--all-features` 会因 `tikv-jemalloc-sys` 的 `configure` 拒绝带空格 prefix 而失败；CI/无空格路径不受影响 |

---

## 2. 源码映射（真实路径）

| 关注点 | 位置 |
|---|---|
| 整合扫描 / 提案创建 | `crates/consolidation/src/engine.rs`（`consolidate_namespace`，原子入口 `create_open_proposal_once`） |
| 提案存储（PG） | `crates/store/src/postgres/integration.rs` |
| 提案存储（内存） | `crates/store/src/in_memory/integration.rs` |
| 账户作用域转发 | `crates/mem-core/src/account_scope.rs`（`IntegrationStore` 段） |
| 提案唯一性迁移 | `crates/store/migrations/20261028000018_proposal_open_uniqueness.sql` |
| 迁移执行器 | `crates/store/src/postgres/mod.rs::run_migrations`（每迁移一事务 + advisory lock + 账本） |
| 备份创建 / 恢复 | `crates/mem-core/src/backup_service.rs`（`restore_inner`、`compensate_merge`、`validate_target_namespaces`） |
| 原子 replace | `crates/store/src/postgres/backup.rs::replace_namespaces_from_snapshots` |
| 定时快照 | `crates/worker/src/backup_job.rs` → `DefaultBackupService::system_snapshot` |
| 作业装配 | `crates/worker/src/embedded.rs`；`crates/bins/backup-worker/src/main.rs` |
| 认知抽取器装配 | `crates/bins/mcp-server/src/main.rs`；模式解析 `crates/common/src/cognitive.rs` |
| 迁移 015 | `crates/store/migrations/20261027000015_vector_index_halfvec_partial.sql` |
| 查询侧索引表达式 | `crates/store/src/postgres/memory.rs`（`embedding::halfvec(1024) <=>`） |

---

## 3. P0-A 跨进程 proposal 原子去重

**状态：已实现 + CI 实测。**

- 唯一性业务键：`(namespace, kind, member_ids)`，仅约束 `status = 'open'`（部分唯一索引）。
  关闭后可再次新建；`suggested_survivor` / `similarity` / `metadata` 不参与判定。
- 写入走 `insert ... on conflict (namespace, kind, member_ids) where status='open' do nothing`，
  冲突 = `AlreadyExists` = `skipped_existing`（幂等，不报错）。
- 迁移 018 先清理历史重复（保留每组最早一条），再建索引；DDL 在事务内，失败即整体回滚且不记账。
- 证据：`crates/store/src/postgres/pg_integration.rs` 的并发/清理用例（CI PG15 实测），
  以及 `crates/consolidation` 的引擎用例。

**未验证 / 风险：** 迁移 018 用 `delete` 清理重复提案（不是改状态），重复组的非最早记录会消失；
这些记录本身是同一逻辑候选的冗余副本，信息等价，但**不可回溯**。
降级窗口内旧二进制插入同键会得到 23505 → `Conflict`，不会写坏数据。

---

## 4. P0-B 恢复失败自动回滚

**状态：replace 原子性已实现（CI 实测）；merge 补偿本轮补完（单测实测）。**

| 模式 | 机制 | 证据 |
|---|---|---|
| `dry_run` | 校验版本 + SHA-256 + 目标范围 + 每个命名空间写权限；零写入 | 本轮补：`test_dry_run_checks_write_acl_for_target_namespaces` |
| `merge` | 致命失败 → 补偿账本（删本轮新建 + 前像回写）；次要实体失败只计数 `partial=true` | 本轮补：3 条注入测试 |
| `replace` | 每个命名空间安全备份 → 单事务清空+重建 | 前序提交 + `test_replace_*` 系列 |

### 本轮修掉的三个真实缺陷

1. **`Err(_) => insert_initial`**：任何存储错误都被当成“库里没有”，会掩盖真实故障。
   现在只有 `HubError::NotFound` 触发新建，其它错误立即致命并触发补偿。
   测试：`test_merge_does_not_treat_storage_error_as_missing_entry`。
2. **merge 无补偿**：中途致命失败会留下“改了一半 + 建了一半”。
   现在按补偿账本回滚；补偿自身失败时报**灾难恢复状态**（含受影响命名空间与原错误），
   不把失败说成成功。测试：`test_merge_fatal_failure_rolls_back_entries_to_pre_restore_state`。
3. **`dry_run` 不校验目标范围与写权限**：预演会给出执行时必然失败的结果。
   现在命名空间集合与 ACL 检查前移到 dry-run 之前；并新增非法命名空间拒绝
   （空、`*`、含 `/` 的作用域键一律拒绝，防伪造内部键跨账户写入）。

### 补偿的已知代价
- 被回滚的条目版本号继续递增并留下一条历史（内容与关联回到操作前，版本号不等于原值）；
- 经验版本 / 任务-经验关联没有独立删除接口，依赖 `on delete cascade` 清理。

**未验证：** 补偿路径只在内存存储上做过注入测试；PostgreSQL 上的等价路径由
`replace` 的单事务测试间接覆盖，**merge 补偿没有 PG 实测**（本机无 PG）。

---

## 5. P1 认知任务调度与生产安全

**状态：调度现状已核实；抽取器默认关闭本轮补完。**

| 作业 | 实现 | 生产调度 | 默认副作用 |
|---|---|---|---|
| 备份 / 证据清理 / 整合扫描 / 过期归档 | 有 | 有（`embedded.rs`、`backup-worker`） | 写快照/提案/归档 |
| `ChatCognitiveJob` | 有（重试/幂等/证据先行） | **无** | 无 |
| `ExperienceDistillJob` | 有 | **无**（同步入口是 `working_experience_distill` 工具） | 无 |

本轮新增 `HUB_COGNITIVE_EXTRACTOR`：

- 默认 `disabled`：**不构造** `ChatCognitiveService`，`cognitive_chat_propose` 返回
  `E_METHOD_NOT_FOUND`，不写证据、不产出提案；`cognitive_intentions_list` 走只读存储分支；
- `mock`：仅开发使用，装配占位抽取器，会写证据与认知提案（Manual 模式只入待审队列）；
- 未知值启动失败（`AppConfig::validate`）。

证据：`crates/common/src/cognitive.rs` 的解析单测、
`test_cognitive_chat_propose_is_disabled_without_extractor`、
`embedded_jobs_do_not_schedule_cognitive_jobs`（前序提交）。

**未验证：** Lite / Production 运行态观察（需起二进制）；真实 LLM 抽取器（能力缺口）。

---

## 6. P1 migration 015

**状态：中断/重试已由 CI 实测；大表耗时与锁、旧二进制延迟已于 2026-10-30 在 2C/1G 演练机上实测。**

- 迁移 015 在启动迁移事务内执行非 `CONCURRENTLY` 的 `create index`：全程持锁，
  失败整条事务回滚且 `_hub_migrations` 不记账 → 下次启动自动重试。
- 执行器不能容纳 `CREATE INDEX CONCURRENTLY`（事务块内禁止）。
- 查询侧 `similar()` 已内联状态字面量，与索引表达式逐字一致（前序提交修正）。
- 中断/重试：`pg_migration_015_interruption_leaves_no_partial_index_and_retries`
  在 CI 的 PG15 上通过（`lock_timeout` 打断 → 无半成品 → 重试成功）。

**2026-10-30 演练（50k 行 × 1024 维，2C/1G，PG 15.19 + pgvector 0.8.7，详见
[`MIGRATION_015_DRILL_2026-10-30.md`](./MIGRATION_015_DRILL_2026-10-30.md)）**：

| 路径 | 耗时 | 写请求 |
|---|---|---|
| 非 `CONCURRENTLY`（迁移路径，256 MB） | **432 s** | 构建期间写全部阻塞（5 s 超时，79/79 采样有锁等待） |
| `CONCURRENTLY`（维护窗口，512 MB） | **3236 s（7.5×）** | p50 2.7 ms、p95 154 ms、超时 1.1%（0 锁等待） |
| 旧二进制 fp32 查询（回滚态） | p50 **498 ms** / Seq Scan | 新形态 p50 **21 ms** / Index Scan |

顺带确认：并行建索引需要 `/dev/shm ≥ maintenance_work_mem`（本次 351 MB 不够 533 MB，
扩容到 1G 后才成功）；201k 行时非并发构建 60 分钟只到 52%，按 ~29 元组/秒外推 1M
约 9–10 小时写阻塞。

**仍未验证：**
- 百万行完整构建（本机只有 30 G 盘、1 GB 内存，装不下 1M 的向量 + 索引 + WAL）；
- 构建期间 WAL 增量、`CONCURRENTLY` 失败后残留索引的处理；
- 生产规格（若恢复为 2C/2GB 或更高）下的绝对值——**本机为 2C/1G，绝对值不可外推**。

---

## 7. P2 文档与元数据

| 文件 | 变更 |
|---|---|
| `README.md` / `README.en.md` | 备份恢复段改写（原子 replace、merge 补偿、v6、指向新文档）；英文版 v5 → v6；认知抽取器默认关闭 |
| `docs/backup-restore.md` | **新增**权威说明：格式版本、三种模式、自动回滚真实边界、剩余风险 |
| `docs/pg-maintenance-runbook.md` | 新增 §2.1 先建后删与维护窗口序列、§2.2 失败与重试、§2.3 旧版本回滚性能 |
| `docs/README.md` | 迁移数量 17 → 18，补 018 |
| `docs/upgrade/LIGHTWEIGHT_EXECUTION_REPORT.md` | 顶部加**历史快照**标记 + 被取代结论清单 |
| `docs/upgrade/REPOSITORY_AUDIT.md` | 同上（18 工具、v4/v5、人工回滚） |
| `docs/upgrade/GOVERNANCE_AUDIT.md` | 同上 |
| `docs/upgrade/COGNITIVE_SCHEDULING_AUDIT.md` | §4 改为“Mock，且生产默认不装配”，补配置矩阵与代码变更 |
| GitHub About | 「18 个 MCP 工具」→「**38 个** MCP 工具」（已核实 38，`gh repo edit` 已执行） |

---

## 8. 测试报告

> 环境：macOS arm64；rustc/cargo 1.98.1；无 PostgreSQL / pgvector / docker；
> 仓库位于外置卷 `/Volumes/WD Drive/...`。
>
> 两个环境坑（都会伪装成"测试失败/卡死"，不是代码问题）：
> 1. **路径含空格**：`--all-features` 会启用 `tikv-jemalloc-sys`，
>    其 `configure` 直接拒绝带空格的 `--prefix`（`Prefix should not contain spaces`）。
> 2. **外置卷 I/O**：在 `target/` 里新构建的测试二进制从外置卷启动会被系统扫描拖死
>    （`--list` 都能挂几分钟；拷到 `/tmp` 后 0.6s 返回）。
>
> 解法：`CARGO_TARGET_DIR=/tmp/mh-target`——产物落在本地磁盘，且路径不含空格，
> 两个问题一起消失。下面的正式结果均在该配置下取得。
> 未运行的用例一律不折算成通过。

| # | 命令 | 环境 | 结果 | 说明 |
|---|---|---|---|---|
| 1 | `cargo fmt --all -- --check` | 本机 | 通过 | exit 0 |
| 2 | `cargo test -p mem-core --lib backup_service` | 本机 | 通过 | 27 passed / 0 failed（新增 5 条恢复失败注入用例） |
| 3 | `CARGO_TARGET_DIR=/tmp/mh-target cargo test --workspace --all-features` | 本机 | 见 §8.1 | 与 CI `quality-gate.sh` 同口径 |
| 4 | `CARGO_TARGET_DIR=/tmp/mh-target cargo clippy --all-targets --all-features -- -D warnings` | 本机 | 见 §8.1 | 同上 |
| 5 | `cargo test -p store --features pg-integration -- --ignored` | CI | **通过**（run `40853880906`，13 passed / 0 failed） | 含本轮新增的 015 中断用例 |
| 6 | 迁移 015 中断/重试 | CI（PG15 + pgvector） | **通过** | `pg_migration_015_interruption_leaves_no_partial_index_and_retries ... ok` |
| 7 | 迁移 015 大表耗时与锁 | 演练机 2C/1G（2026-10-30） | **已完成（单次）** | 50k 行：非并发 **432 s**（写全阻塞）vs 并发 **3236 s**（写 p50 2.7 ms） |
| 8 | 旧二进制回滚延迟 | 同上 | **已完成（单次）** | 50k 行：旧形态 p50 **498 ms**（Seq Scan）vs 新形态 **21 ms**（Index Scan） |
| 9 | 百万行完整构建 | — | **未运行** | 本机 30 G 盘 / 1 GB 内存装不下 |
| 10 | Lite/Production 运行态观察 | — | **未运行** | 需起二进制 |
| 11 | 真实 embedding 语义验收 | — | **不在本轮范围** | 任务书明确不做 |

### 8.1 本次运行结果（2026-10-29，`CARGO_TARGET_DIR=/tmp/mh-target`）

| 命令 | 结果 |
|---|---|
| `cargo fmt --all -- --check` | **通过**（exit 0） |
| `cargo test -p mem-core --lib backup_service` | **27 passed / 0 failed** |
| `cargo test --workspace --all-features --no-fail-fast` | **574 passed / 0 failed / 20 ignored**，exit 0 |
| `cargo clippy --all-targets --all-features -- -D warnings` | **通过**（exit 0，无 warning） |

20 个 ignored 中包括 `store --features pg-integration` 的 PG 用例与本轮新增的
`pg_migration_015_interruption_leaves_no_partial_index_and_retries`——它们**已编译通过但本机未执行**
（无 `DATABASE_URL`），不计入上面的本机通过数。

### 8.1.1 CI 实测（PR #2，run `40853880906`）

| Job | 结果 |
|---|---|
| 依赖准入与安全审计 | **success** |
| 质量门禁（`fmt` / `clippy --all-features` / `test --all-features`） | **success** |
| PostgreSQL 集成测试（PG15 + pgvector） | **success**：13 passed / 0 failed，含 `pg_migration_015_interruption_leaves_no_partial_index_and_retries ... ok` |
| Release 构建 | **success** |

因此“PG 侧的执行证据”不再缺失：015 中断/重试、018 历史去重、v1–v6 恢复矩阵、
ANN 部分索引命中都在真实 PG 上跑过。仍缺的只有生产规模数据与运行态观察。

### 8.2 运行后清理
构建产物在 `/tmp/mh-target`（仓库外），诊断日志在 `/tmp/mh-*.log`（仓库外）；
仓库内未新增日志、测试数据或临时文件（`git status` 只有本轮有意修改的文件）。

---

## 9. 回滚手册（摘要）

1. **应用版本回滚**：代码层面每个修复独立 commit，可单独 `git revert`。
   注意 v6 备份无法被旧二进制恢复——降级前保留新二进制或先完成恢复。
2. **数据库迁移 018**：
   - 回滚 = `drop index if exists uq_integration_proposals_open_business_key`；
   - 被清理的重复提案**不会自动恢复**（已 delete）；
   - 旧二进制可继续运行，只是重新失去跨进程去重保护。
3. **迁移 015**：不改表结构，回滚应用可启动；但旧查询走 fp32，**不走 halfvec 索引**，
   大表上会退化为 `Seq Scan`。需要性能兼容时按 runbook §2.3 临时重建 fp32 索引。
4. **恢复操作失败**：
   - `replace` 失败 → 单事务已回滚；核对 `safety_backup_ids` 是否齐全；
   - `merge` 失败 → 错误信息会说明“已按补偿账本回滚”或“灾难恢复状态”；
     后者请用本次操作前的安全备份手工恢复，并保留现场日志。

---

## 10. 剩余风险（按严重度）

| # | 风险 | 严重度 | 是否阻塞发布 |
|---|---|---|---|
| R1 | 真实 embedding / LLM 抽取器未接入，语义能力与认知提炼均未验收 | 高（能力缺口） | 否（本轮范围外，已在 README 与健康检查如实暴露） |
| R2 | merge 补偿在 PostgreSQL 上没有注入测试，只有内存实现证据 | 中 | 否（replace 的原子路径有 PG 证据） |
| R3 | 迁移 015 已在 **50k / 2C1G** 实测（非并发 432 s 写全阻塞、并发 3236 s）；**百万行仍未验证** | 中 | 否（大表按 runbook 维护窗口；1M 规模需更大机器） |
| R8 | 生产 `id` 为随机 UUID，慢盘上批量恢复写入仅 ~5 MB/分钟（顺序 UUID 为 ~18 MB/秒） | 中 | 否（恢复是运维操作，可安排在维护窗口并监控进度） |
| R4 | 迁移 018 清理会删除历史重复提案（不可回溯） | 低 | 否（同逻辑候选的冗余副本） |
| R5 | `partial=true` 的 merge 不保证原子；调用方必须检查报告 | 低 | 否（已在文档与报告字段显式暴露） |
| R6 | 补偿回滚会让条目版本号 +2 | 低 | 否（内容与关联正确） |
| R7 | 仓库在外置卷时，新构建的测试二进制启动会被系统扫描拖慢甚至“假死”；需用 `CARGO_TARGET_DIR` 指向本地磁盘 | 低 | 否（已写入本文 §8 环境说明） |

---

<!-- kimi-badge -->
[![powered by KimiCode](../../.github/assets/kimi-badge.svg)](https://www.kimi.com/code)
