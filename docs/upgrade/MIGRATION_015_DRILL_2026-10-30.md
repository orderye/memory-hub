# Migration 015 大表升级演练（2026-10-30）

**环境**：阿里云 2 vCPU / 1 GB（可见 701 MiB）/ 30 GB 云盘，Debian 12，PostgreSQL **15.19**，
pgvector **0.8.7**（与生产同版本），额外挂 4 GB swap。
**数据**：50,000 行 × 1024 维（fp32），表总大小 286 MB，向量经 TOAST 外置存储；9 个二级索引。
**口径**：单次运行，未做重复取中位数；磁盘为云盘默认档，未标定 IOPS。
**结论先说**：在这台 2C/1G 上，

- 非并发建索引（迁移路径）**432 秒**，期间写请求被完全阻塞；
- 并发建索引（维护窗口路径）**3236 秒**（7.5×），写请求基本不受影响；
- 201k 行时非并发构建 60 分钟只完成 52%，**大表不能走"启动即迁移"**；
- 并行建索引需要 `/dev/shm` ≥ `maintenance_work_mem`，默认值不够会直接失败。

---

## 1. 为什么重跑

此前只做过静态分析与 CI 上的 `EXPLAIN` 验证，没有在真实规模上测过耗时、锁与失败路径。
本轮在独立演练机（非生产）上补测，机器为 **2C/1G**；原先假设的 2C/2GB 规格已注销，
本仓库后续测试统一使用 2C/1G。

## 2. 方法

1. 用仓库真实迁移建库：`001`–`014`、`016`–`018` 共 17 个，**跳过 015**，保留"升级前"状态。
2. `COPY` 灌入 50k 行随机单位向量（顺序 UUID 主键；见 §6 的对照）。
3. 两条路径各跑一次，每次同时施加读写负载：
   - 写：按 id `update memories set updated_at = now()`，间隔 50 ms，`statement_timeout=5s`；
   - 读：`halfvec` ANN top-20，间隔 50 ms，同样 5 s 超时；
   - 每 5 s 采样 `pg_stat_progress_create_index` 与 `pg_locks`。
4. 构建完成后跑新旧查询对比（各 60 次，含 `EXPLAIN (ANALYZE, BUFFERS)`）。

**T1 = 迁移路径**：把 015 原文件放进 `BEGIN … COMMIT` 执行（与运行时执行器的行为一致，
非 `CONCURRENTLY`，`maintenance_work_mem=256MB`）。

**T2 = 维护窗口路径**：`CREATE INDEX CONCURRENTLY`，`maintenance_work_mem=512MB`，
`max_parallel_maintenance_workers=2`，索引定义与 015 逐字一致。

## 3. 结果

| 指标 | T1 迁移路径 | T2 维护窗口路径 |
|---|---|---|
| 耗时（50k 行） | **432 s** | **3236 s**（7.5×） |
| 索引大小 / 状态 | 130 MB / valid·ready | 130 MB / valid·ready |
| 写请求超时 | **81 次**（构建期间持续阻塞，约 5 s 一次） | **74 / 6703（1.1%）** |
| 写延迟（未被阻塞时） | p50 1.7 ms | **p50 2.7 ms / p95 154 ms** |
| 锁等待采样 | **79 / 79 个周期有等待**（`ungranted_locks=2`） | **0 / 79** |
| 读请求（5 s 超时） | 80 / 98 超时 | 167 / 189 超时 |

读在两条路径下都严重超时，原因是**索引构建完成前 ANN 查询本身就是全表扫描**（50k 行约 500 ms），
再叠加构建的 I/O 竞争；这不是锁阻塞，不能读成"并发构建阻塞读"。

### 3.1 201k 行的中断观测

同一非并发路径在 201,000 行上跑 60 分钟：块进度 2523/4873（52%），元组速率降到 ~29/s，
未完成。CPU 83% 空闲、swap 使用 0 —— 瓶颈是云盘随机写 IOPS，不是内存。
按该速率线性外推，1M 行约 **9–10 小时写阻塞**。这是**部分运行的外推**，不是实测。

## 4. 失败路径：`/dev/shm` 不足

T2 首次执行直接失败：

```
ERROR:  could not resize shared memory segment "/PostgreSQL.872614454" to 533761856 bytes:
        No space left on device
```

`/dev/shm` 只有 351 MB，而并行建索引需要 533 MB。`mount -o remount,size=1G /dev/shm` 后成功。
Docker 默认 `/dev/shm` 常为 64 MB，**这个坑在生产容器里更容易踩**。

## 5. 旧二进制回滚后的查询延迟（50k）

| 查询形态 | p50 | p95 | 计划 |
|---|---|---|---|
| 新（`embedding::halfvec(1024) <=>`） | **21 ms** | 316 ms | `Index Scan using idx_memories_embedding_hf` |
| 旧（`embedding <=>` fp32，015 之前的代码） | **498 ms** | 509 ms | **`Seq Scan on memories`（全表 50k 行）** |

- 旧二进制的 SQL 形如 `select … (embedding <=> $1::vector) as dist … order by dist limit 20`，
  与 halfvec 表达式索引不匹配，**必然退化为顺序扫描**；
- 这是 O(n)：1M 行时按比例约 **10 秒/查询**；
- 结论：**回滚到 015 之前的二进制时，如需可用性能，必须先临时重建 fp32 索引**（runbook §2.3）。

## 6. 顺带发现：随机 UUID 主键的批量写入

同一块盘、同一批数据：

| 主键形态 | 写入速率 |
|---|---|
| 随机 UUID（生产形态） | **~5 MB/分钟** |
| 顺序 UUID | **~18 MB/秒**（>200×） |

生产的 `id` 是随机 UUID，这意味着**大批量恢复（`replace`）在慢盘上会显著偏慢**，
且与 `shared_buffers` 大小强相关。这条已写入恢复性能风险。

## 7. 适用边界（必须一起读）

- 规模是 **50k**，不是百万级；201k 只有部分运行数据。**不能把 432 s / 3236 s 直接外推到生产**；
- 机器是 **2C/1G**，磁盘为云盘默认档；生产若用 ESSD 或更大规格，绝对值会好很多，
  但"非并发会阻塞写、并发约慢一个数量级"这个**关系**与规模无关；
- 单次运行，无重复；读超时受 I/O 竞争影响，波动大；
- 未测：百万行完整构建、`CONCURRENTLY` 失败后残留索引的处理、构建期间的 WAL 增量。

## 8. 对运维手册的修改

本次结果已回写 `docs/pg-maintenance-runbook.md`：

- 部署基线改为 **2C/1G**（原 2C/2GB 已注销）；
- §2.1 增加 `/dev/shm` 前置检查与 `mount -o remount,size=1G /dev/shm`；
- §2.1 记录"非并发 = 写全阻塞 / 并发 ≈ 7.5×"的实测对照；
- §2.3 用实测数字替换原来的"未实测"表述。

<!-- kimi-badge -->
[![powered by KimiCode](../../.github/assets/kimi-badge.svg)](https://www.kimi.com/code)
