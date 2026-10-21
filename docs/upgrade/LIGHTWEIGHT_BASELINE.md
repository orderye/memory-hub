# Memory Hub 轻量化升级 · 性能基线（LIGHTWEIGHT_BASELINE）

**阶段：** P0.4 性能基线 + P1/P2 优化前后对比
**基线提交：** `3048efc`（分支 `main`），优化在分支 `feat/lightweight-deployment`
**测量工具：** [`scripts/lightweight_probe.py`](../scripts/lightweight_probe.py)（可复现，见 §7）

---

## 1. 口径声明（先读这一节）

| 项目 | 本报告的处理 |
| :--- | :--- |
| 存储后端 | **内存存储**（`HUB_DB_URL=memory://local`）。数字只用于"同一台机器、同一二进制、不同配置"的相对对比 |
| PostgreSQL / pgvector | **NOT MEASURED** —— 本机没有 docker、没有 psql、5432 无监听（详见审计 §0.3），任何 PG 往返 / 索引 / 连接池数字都没有实测依据 |
| 真实嵌入端点 | **NOT MEASURED** —— 无可用端点与凭据；伪向量（mock）延迟只反映本地 SHA-256 计算，不代表任何真实模型 |
| 干净全量构建耗时 | **NOT MEASURED** —— 仓库在外置盘（路径含空格），清空 target 后重建需数十分钟以上；只记录**增量**构建耗时，并注明增量基线 |
| 备份 / 恢复耗时、水平扩展、多副本租约实测 | **NOT MEASURED / NOT RUN** —— 需要真实 PG 与多进程环境 |

**不做的事**：不把伪向量下的分数当语义结论，不把内存存储的延迟当生产延迟，
不用"应该更快"的说法替代实测，也不为了凑 10% 的改善而改口径。

---

## 2. 测量环境

| 项 | 值 |
| :--- | :--- |
| 机器 / 系统 | macOS 27.0.1（26A434），arm64 |
| 工具链 | cargo 1.98.1 / rustc 1.98.1 |
| 仓库位置 | 外置盘，路径含空格（因此本机禁止 `--all-features`，见 `GEMINI.md` §2） |
| 二进制 | `target/release/{mcp-server,auth-server,backup-worker}`，默认 feature（不含 jemalloc） |
| 探针 | HTTP + 离线 HS256 令牌，30 次迭代，3 秒空闲后读 RSS |

---

## 3. 优化前（基线，`3048efc`）

### 3.1 构建与产物

| 指标 | 值 | 说明 |
| :--- | :--- | :--- |
| `cargo build --release -p mcp-server -p auth-server -p backup-worker` | **100.09 s**（real） | 依赖已全量编译过的**增量**构建；user 211.65 s / sys 6.32 s |
| `mcp-server` 体积 | **17,757,888 B**（16.9 MiB） | 含 adapter-cjs 默认 feature |
| `auth-server` 体积 | **11,418,368 B**（10.9 MiB） | — |
| `backup-worker` 体积 | **11,168,336 B**（10.7 MiB） | — |

### 3.2 启动与常驻内存（`profile=production`，`HUB_EMBED_PROVIDER=mock`）

| 轮次 | 启动到 `/healthz` 200 | 空闲 RSS |
| :--- | :--- | :--- |
| 第 1 次（冷启动，首次执行新二进制） | **561.96 ms** | 8,832 KB |
| 第 2 次 | **6.90 ms** | 8,832 KB |
| 第 3 次 | **7.99 ms** | 8,880 KB |

> 冷启动 562 ms 主要是操作系统首次载入/校验二进制；稳态启动约 **7 ms**。
> 后续对比一律看稳态（第 2/3 次）。

### 3.3 代表性读写延迟（内存存储，30 次）

| 轮次 | put p50 / p95 | get p50 / p95 | search p50 / p95 |
| :--- | :--- | :--- | :--- |
| 第 1 次 | 0.282 / 0.431 ms | 0.273 / 0.397 ms | 0.640 / 0.865 ms（max 32.8 ms） |
| 第 2 次 | 0.345 / 0.545 ms | 0.324 / — ms | 0.791 / 0.948 ms |
| 第 3 次 | 0.248 / 0.424 ms | 0.249 / — ms | 0.583 / 0.922 ms |

> 第 1 次 search 的 32.8 ms 是首次进入 `spawn_blocking` 的线程池预热，属离群值。
> 每次 `put` / `search` 都包含一次 mock 嵌入计算（1024 维 SHA-256 伪向量）。

---

## 4. 优化后（`feat/lightweight-deployment`）

### 4.1 产物（`profile=production`，默认 feature）

| 指标 | 值 | 与优化前 |
| :--- | :--- | :--- |
| `cargo build --release`（三二进制，增量） | **32.33 s / 34.72 s**（两次） | 100.09 s → 约 1/3（**注意：增量基线不同，见 §5.3**） |
| `mcp-server` | **17,839,680 B** | +81,792 B（+0.46%）：新增 profile/租约/契约相关代码 |
| `auth-server` | **11,419,680 B** | +1,312 B（+0.01%） |
| `backup-worker` | **11,264,768 B** | +96,432 B（+0.86%）：`--once`、租约、`serde_json` 摘要输出 |

### 4.2 启动与常驻内存（无额外数据）

| 配置 | 启动（稳态） | 空闲 RSS |
| :--- | :--- | :--- |
| `production` + `mock`（3 次） | 6.37 / 42.0 / 6.68 ms | 9,024 / 9,040 / 9,088 KB |
| `lite` + `off`（3 次） | 6.28 / 7.04 / 6.61 ms | 9,024 / 8,992 / 9,040 KB |

> 42 ms 是首次执行的页缓存冷启动离群值。稳态下两种模式启动时间一致（约 6–7 ms），
> 空闲 RSS 一致（约 9.0 MB）：**轻量化不以"更小的空转进程"为卖点，而是省掉实际工作**。

### 4.3 写入 2000 条后的常驻内存（本轮最明确的收益）

| 配置 | 写入 2000 条后 RSS | 相对 |
| :--- | :--- | :--- |
| `production` + `mock`（每条落 1024 维伪向量） | **21,264 KB** | 基线 |
| `lite` + `off`（每条不落向量） | **13,232 KB** | **−8,032 KB（−37.8%）**，折合 **≈4.1 KiB/条** |

> 该差值与 `1024 × 4 B = 4 KiB/条` 的向量体积吻合：关闭嵌入后，向量既不算也不存。
> 内存存储下能看到这个差值；PostgreSQL 下对应的是 `embedding` 列为 NULL、
> HNSW 索引不含该行（**生产库的实际收益 NOT MEASURED**）。

### 4.4 延迟

| 场景 | put p50 / p95 | get p50 | search p50 / p95 |
| :--- | :--- | :--- | :--- |
| `production` + `mock`，无额外数据（3 次） | 0.382 / 0.522 · 0.245 / 0.569 · 0.413 / 0.664 ms | 0.35 / 0.25 / 0.39 ms | 0.801 / 0.929 · 0.671 / 1.282 · 0.917 / 1.008 ms |
| `lite` + `off`，无额外数据（3 次） | 0.264 / 0.538 · 0.377 / 0.682 · 0.383 / 0.516 ms | 0.251 / 0.377 / 0.383 ms | 0.461 / 0.662 · 0.638 / 0.816 · 0.632 / 0.707 ms |
| `production` + 2000 条 | 0.333 / 0.741 ms | 0.283 ms | **9.880 / 15.129 ms** |
| `lite` + 2000 条 | 0.290 / 0.664 ms | 0.277 ms | **7.508 / 13.514 ms**（−24.0% / −10.7%） |

> 小数据量下 put/search 的差异被噪声淹没（30 次采样、±40% 波动），**不宣称改善**。
> 有区分度的是"候选池变大"之后：关掉向量路后每次检索少一遍 2000×1024 的点积
> 与相似度矩阵，p50 降低约 24%。

---

## 5. 前后对比结论（只写实测能支撑的部分）

1. **默认配置（production）行为不变**：启动 6.7 ms、空闲 RSS ≈9.0 MB、put/search p50
   与优化前处于同一噪声区间（优化前 0.282 / 0.640 ms，优化后 0.382 / 0.801 ms，
   单次 30 采样不足以分辨 <40% 的差异）。这是有意的：**轻量化不得顺手改动生产路径**。
2. **Lite 的真实收益是"不做无用功"，不是"进程更小"**：
   - 每条写入不再计算/存储 1024 维向量 → 2000 条时 RSS 从 21.3 MB 降到 13.2 MB（**−37.8%**）；
   - 每次检索不再发查询嵌入、不再做向量召回 → 2000 条候选时 search p50 9.88 → 7.51 ms（**−24.0%**）；
   - 每次写入/检索的嵌入调用数从 1 次降为 **0 次**（测试断言，非估算）。
3. **没有达到 10% 的项如实列出**：构建耗时、二进制体积、启动时间、空闲 RSS 在默认配置下
   均无 ≥10% 的改善（体积反而因新功能增大 <1%）。按任务书要求，这些项**不以牺牲正确性换数字**。
4. **未测量项**：PostgreSQL 往返、真实嵌入延迟、备份/恢复耗时、水平扩展、干净全量构建（见 §1）。

---

## 6. 调用计数与语义安全（来自测试断言，不是估算）

| 断言 | 结论 |
| :--- | :--- |
| B 模式（`HUB_CONSOLIDATION_MODE=rules`）embedding 调用数 | **0**（`consolidation_eval::test_rules_mode_makes_zero_embedding_calls`） |
| A 模式生成式 LLM 调用数 | **0**（仓库无任何 LLM SDK 依赖与调用点，见审计 §6.3） |
| 关闭嵌入（`off`）时检索的 embed 调用数 | **0**（`retrieval::tests::test_disabled_embedder_never_calls_embed_and_still_searches` 用 panic 型嵌入器证明） |
| 关闭嵌入时写入的 embed 调用数 | **0**（`mem-core::memory_service::tests::test_put_with_disabled_embedder_stores_no_vector`） |
| 自动错误合并数 / 硬约束违规数 | **0**（`consolidation_eval` 质量门禁） |

---

## 7. 复现命令

```bash
# 1) 构建（默认 feature；本机路径含空格，禁止 --all-features）
cargo build --release -p mcp-server -p auth-server -p backup-worker

# 2) 基线/对照探针（自动起停进程、离线签发令牌）
python3 scripts/lightweight_probe.py --label production-mock \
  --profile production --embed-provider mock \
  --iterations 30 --bulk-entries 2000 \
  --out target/baseline/probe-production.json

python3 scripts/lightweight_probe.py --label lite-off \
  --profile lite --embed-provider off \
  --iterations 30 --bulk-entries 2000 \
  --out target/baseline/probe-lite.json

# 3) 检索与整合质量门禁
cargo test -p retrieval --test eval_bench -- --nocapture
cargo test -p consolidation --test consolidation_eval -- --nocapture
```

> `--bulk-entries N` 会在测延迟前额外写入 N 条记忆并再读一次 RSS，
> 用于观察"是否落向量"对常驻内存的影响（1024 维 f32 ≈ 4 KiB/条）。
