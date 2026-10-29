# 阻塞项清单与解除方案

**整理时间：** 2026-10-29
**分支：** `reliability/productionization`（`/tmp/memory-hub-reliability`）
**已提交：** `a9ca558`、`5cb129e`、`e063d0e`、`fc3ce64`、`682b1f0`、`a8d72bc`、`154f57a`、`e4baa0b`

---

## 0. 一句话结论

所有离线可验证的修复已完成并通过（`fmt` / `clippy -D warnings` / workspace 562 passed、0 failed）。
**剩余阻塞只有两类根因**：① 需要真实 PostgreSQL + pgvector；② 需要真实 embedding 服务。
其中 ① **不必新建云环境**——现有 CI 已自带 PG15+pgvector，把分支推上去即可实证。

---

## 1. 阻塞总表

| # | 阻塞项 | 归属 | 根因 | 解除条件 | 优先级 |
|---|---|---|---|---|---|
| B1 | 原子多 namespace 替换的真实回滚 | P0-A | 无 PG | PG 运行 `pg_atomic_namespace_replace_rolls_back_on_failure` | **P0** |
| B2 | PG 备份/恢复往返、审计插入、保留策略 | P0-A | 无 PG | PG 跑 `pg_backup_and_restore_roundtrip` 等 | **P0** |
| B3 | v6 备份格式在真实库的兼容性 | P0-A | 无 PG | PG 上跑 v1–v6 恢复矩阵 | **P0** |
| B4 | 迁移 018 唯一索引 + 真实并发进程 | P0-B | 无 PG | PG 跑 `pg_open_proposal_business_key_is_unique` + 两进程实测 | **P0** |
| B5 | 迁移 018 历史重复清理的破坏性 | P0-B | 需生产数据 | 生产执行前先全量备份并核对将被删除的行 | **P0** |
| B6 | 015 索引命中 / `indisvalid` / 体积 / 中断重试 | P1-B | 无 PG | PG 跑 `EXPLAIN (ANALYZE, BUFFERS)` 等 | P1 |
| B7 | 大表 `CONCURRENTLY` 实测耗时 | P1-B | 需生产规模数据 | 维护窗口演练 | P1 |
| B8 | 真实模型语义指标（Recall/MRR/nDCG/延迟/成本/错误合并率） | P1-C | 无 endpoint | 配置真实 embedding 服务 | P1 |
| B9 | 阈值 0.65 在真实模型下重标定 | P1-C | 同 B8 | `HUB_EVAL_CALIBRATE=1` + 真实模型 | P1 |
| B10 | Lite/Production 运行态观察（作业实际运行） | P1-A | 未部署 | 起二进制观察日志 | P1 |
| B11 | 镜像偏差 / 正式 CI 结果 / 发布溯源 | P1-D | 需推送与远端状态 | 推送分支并读 CI 结果 | P1 |
| B12 | 供应链审计（cargo-deny/audit）本地未跑 | P2 | 工具未装 | CI 的 supply-chain job | P2 |

---

## 2. B1–B4、B6：PG 相关阻塞——两条解除路径

### 路径 A（推荐）：推分支，让现有 CI 跑 PG 测试

**为什么可行**：`.github/workflows/ci.yml` 的 `postgres` job 已经启动
`pgvector/pgvector:pg15` 服务，并且用 glob 应用**全部**迁移文件：

```bash
for f in crates/store/migrations/*.sql; do psql ... -v ON_ERROR_STOP=1 -f "$f"; done
cargo test -p store --features pg-integration -- --ignored --test-threads=1
```

所以 `20261028000018_proposal_open_uniqueness.sql` 会被应用，我们新增的
`pg_atomic_namespace_replace_rolls_back_on_failure`、
`pg_open_proposal_business_key_is_unique` 都会被执行。

**操作**：

```bash
git push origin reliability/productionization
gh run list --branch reliability/productionization --limit 5
gh run watch <run-id>
```

**代价 / 风险**：会向 GitHub 推送分支（工作区外的副作用），需你明确授权。
不会改动 `main`，不部署，不触发 CNB 生产流水线（CNB 只对 `main` 与 tag 部署）。

**若失败怎么办**：CI 报错即为真实现证结果，按错误修复后重跑——这正是我们需要的信息，
不是风险。

### 路径 B：本地 brew 装 PostgreSQL 15 + pgvector（离线、可重复）

本机 `brew` 可用，且 formula 存在（`postgresql@15` 15.19、`pgvector` 0.8.7）：

```bash
brew install postgresql@15 pgvector
brew services start postgresql@15
createdb memory_hub_test
psql -d memory_hub_test -c "create extension if not exists vector;"
for f in crates/store/migrations/*.sql; do
  psql -d memory_hub_test -v ON_ERROR_STOP=1 -f "$f"
done
DATABASE_URL="postgresql://$(whoami)@127.0.0.1:5432/memory_hub_test?sslmode=disable" \
  cargo test -p store --features pg-integration -- --ignored --test-threads=1
```

**代价**：本机安装数据库软件（约数百 MB），需要 `brew services` 常驻或手动 `pg_ctl`。
**优点**：完全离线、随时可重跑、不推送任何东西。
**注意**：这是本地开发环境，不是生产库；`memory_hub_test` 是独立数据库。

### 路径 C（不推荐）：跳过

不解除。B1–B4、B6 继续标记为未验证，P0-A / P0-B 不能宣称通过。

---

## 3. B5：迁移 018 的破坏性清理——执行前必做

迁移 018 为了让唯一索引能建成，会**删除**同业务键的重复 open 提案，每组保留最早一条。

**执行前检查**：

```sql
-- 1) 先看会被删掉多少行、是哪些
select namespace, kind, member_ids, count(*) as dup_count,
       min(created_at) as earliest, max(created_at) as latest
from integration_proposals
where status = 'open'
group by namespace, kind, member_ids
having count(*) > 1;

-- 2) 全量备份（或至少备份该表）
pg_dump -t integration_proposals -Fc memory_hub > integration_proposals_pre018.dump
```

**回滚**：

```sql
drop index if exists uq_integration_proposals_open_business_key;
```

删除的行**不可通过 drop index 恢复**，必须靠上面的 dump。
这是本迁移唯一的不可逆点，务必先备份。

---

## 4. B8 / B9：真实 embedding——需要一个 OpenAI 兼容端点

评测代码已就绪（`crates/retrieval/tests/eval_bench.rs`）：

```bash
HUB_EVAL_EMBED_ENDPOINT=https://<你的服务>/v1/embeddings \
HUB_EVAL_EMBED_MODEL=bge-m3 \
HUB_EVAL_EMBED_DIM=1024 \
HUB_EVAL_EMBED_API_KEY=<key> \
HUB_EVAL_REQUIRE_SEMANTIC=1 \
HUB_EVAL_WRITE_BASELINE=1 \
  cargo test -p retrieval --test eval_bench -- --nocapture
```

**关键点**：

- 必须先 `HUB_EVAL_WRITE_BASELINE=1` 冻结**真实模型**基线（现有基线是伪向量模态，
  指纹/嵌入器不同会被新门禁硬失败——这是本轮刚修的行为）。
- 冻结后用 `HUB_EVAL_REQUIRE_SEMANTIC=1` 作为 CI 门禁，伪向量会直接 panic。
- 阈值重标定：`HUB_EVAL_CALIBRATE=1 cargo test -p retrieval --test eval_bench -- --ignored --nocapture`。
- 结果**单独报告**，不得与伪向量数字混用。

**本机不可行的原因**：无 `ollama` / `llama-server`，Python 无 `torch` /
`sentence_transformers`，因此无法本地起模型。需要你提供端点，或自行安装本地推理服务。

**报告口径**：在拿到真实结果前，所有语义结论保持"未验证"。

---

## 5. B10：Lite / Production 运行态

不需要数据库权限也能做一部分（内存存储可起 `mcp-server` 预览），但**作业真实运行**需要 PG：

```bash
# Lite：内嵌作业
HUB_DB_URL=postgresql://... HUB_DEPLOYMENT_PROFILE=lite HUB_EMBEDDED_WORKER=1 \
  ./target/debug/mcp-server   # 观察 "内嵌后台作业调度已启动" 与周期日志

# Lite：独立 worker 只允许 --once
HUB_DB_URL=postgresql://... HUB_DEPLOYMENT_PROFILE=lite \
  ./target/debug/backup-worker --once
# 预期：不带 --once 时以 exit 2 拒绝常驻

# Production：独立 worker 常驻
HUB_DB_URL=postgresql://... ./target/debug/backup-worker
```

依赖 B1–B4 的 PG 环境。

---

## 6. B11：镜像与 CI 溯源

已知（Task 0 实测）：

- GitHub `main` = `d35f033`；CNB `main` = `4b140e0`（已 ahead 55 个提交）。
- 两端 55 个 patch-id **一一对应**，关键文件哈希一致，`v0.1.0` peeled commit 相同。
- 即：内容等价，历史是重新生成的分叉。

**待确认**：

1. 哪个是权威写入源（`scripts/push-cnb.sh` 暗示 GitHub→CNB 单向重放）。
2. 镜像同步失败的检测手段（当前无自动化告警）。
3. 正式 CI 结果需推送后才能读到（本地只能证明本地命令通过）。

**可立即做**：`gh run list --repo orderye/memory-hub --limit 10` 查看 `main` 最近 CI 状态，
不需要推送。

---

## 7. B12：供应链审计

本地未装 `cargo-deny` / `cargo-audit`；CI 的 `supply-chain` job 已在跑（`cargo-deny check all`
+ `rustsec/audit-check`，并豁免 `RUSTSEC-2023-0071`）。

**可选本地执行**：

```bash
cargo install cargo-deny cargo-audit   # 需要联网
cargo deny check all --all-features
cargo audit
```

或直接读 CI 的 `supply-chain` job 结果。

---

## 8. 建议执行顺序

| 顺序 | 动作 | 解除 | 需要你做什么 |
|---|---|---|---|
| 1 | 授权推送分支到 GitHub | B1–B4、B6、B11 的一半 | 说"可以推送" |
| 2 | 读 CI 结果，按失败修复 | B1–B4、B6 | 无（我自动跟进） |
| 3 | 生产执行迁移 018 前备份 + 重复行核对 | B5 | 在生产机执行前确认 |
| 4 | 提供 embedding 端点 | B8、B9 | 给 endpoint / model / dim / key |
| 5 | 起二进制观察作业 | B10 | 依赖 1 完成后的 PG |
| 6 | 维护窗口演练 CONCURRENTLY | B7 | 生产规模数据 |

**若你只想做最小一步**：选第 1 项。它一次解除 P0-A 与 P0-B 的绝大部分阻塞，
且不改动 `main`、不部署、不产生费用。

---

## 9. 当前不可宣称通过的能力

在阻塞解除前，以下结论**不成立**，任何报告都不得声称：

- P0-A 备份/恢复的原子回滚已验收；
- P0-B 跨进程 proposal 去重已验收；
- 迁移 015 索引在真实库上生效；
- 迁移 018 唯一约束在真实库上生效；
- 真实 embedding 的语义检索质量达标；
- Lite/Production 后台作业已实际运行。

<!-- kimi-badge -->
[![powered by KimiCode](../../.github/assets/kimi-badge.svg)](https://www.kimi.com/code)
