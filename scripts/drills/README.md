# 演练脚本：迁移 015 / ANN 延迟 / 索引构建影响

配套报告：[`docs/upgrade/MIGRATION_015_DRILL_2026-10-30.md`](../../docs/upgrade/MIGRATION_015_DRILL_2026-10-30.md)。
这些脚本在 **2C/1G Debian 12 + PG 15.19 + pgvector 0.8.7** 上跑过，不是通用工具：
它们假设以 `root` 执行、通过 `runuser -u postgres` 连库，库名默认 `mh_drill`。

## 前置

```bash
apt-get install -y postgresql-15 postgresql-15-pgvector python3-numpy python3-psycopg2
pg_lsclusters                     # 15 main online
df -h /dev/shm                    # 必须 >= maintenance_work_mem，否则并行建索引会失败
mount -o remount,size=1G /dev/shm
```

建库：先按文件名顺序应用 `crates/store/migrations/*.sql`，**跳过 015**（演练时手动执行）。

```bash
for f in $(ls /tmp/mh_migrations/*.sql | sort); do
  case "$f" in *15_vector_index*) continue;; esac
  runuser -u postgres -- psql -v ON_ERROR_STOP=1 -q -d mh_drill -f "$f" || exit 1
done
```

先建一条命名空间链路（`memories.namespace` 有外键）：

```sql
insert into accounts(id,username) values ('00000000-0000-0000-0000-000000000001','drill') on conflict do nothing;
insert into agents(agent_id,name,owner_account_id)
  values ('drill','drill','00000000-0000-0000-0000-000000000001') on conflict do nothing;
insert into namespaces(namespace,owner_agent) values ('drill','drill') on conflict do nothing;
```

## 灌数据

```bash
# 大批量灌数前先摘掉二级索引（随机 UUID 主键 + 小 shared_buffers 会让写入退到 ~5MB/分钟）
python3 scripts/drills/seed_memories.py 50000 mh_drill
runuser -u postgres -- psql -d mh_drill -c "analyze memories"
```

## 索引构建 + 负载

两个终端各跑一个：

```bash
# 采样：每 5s 记一次 pg_stat_progress_create_index 与锁等待
touch /tmp/sampling.on
bash scripts/drills/sampler.sh /tmp/progress.csv &

# 负载：写 update + halfvec ANN 读，各 50ms 间隔，5s 语句超时
runuser -u postgres -- python3 scripts/drills/workload.py 1800 /tmp/workload.csv
```

构建（两条路径二选一）：

```bash
# A. 迁移路径（与运行时执行器一致：事务内、非 CONCURRENTLY）
( echo "begin;"; cat crates/store/migrations/20261027000015_vector_index_halfvec_partial.sql; echo "commit;" ) \
  | runuser -u postgres -- psql -v ON_ERROR_STOP=1 -d mh_drill

# B. 维护窗口路径（每 -c 一条语句，否则会被包进隐式事务而报
#    "CREATE INDEX CONCURRENTLY cannot run inside a transaction block"）
runuser -u postgres -- psql -d mh_drill \
  -c "set maintenance_work_mem='512MB'" \
  -c "set max_parallel_maintenance_workers=2" \
  -c "create index concurrently idx_memories_embedding_hf on memories using hnsw ((embedding::halfvec(1024)) halfvec_cosine_ops) where status in ('active','archived')"
```

收尾：`rm /tmp/sampling.on`，然后统计 CSV。

## 新旧查询延迟

```bash
runuser -u postgres -- python3 scripts/drills/bench_ann.py new 60 | head -3   # halfvec，应走 Index Scan
runuser -u postgres -- python3 scripts/drills/bench_ann.py old 60 | head -3   # fp32，应退化为 Seq Scan
```

## 2026-10-30 结果（50k 行，2C/1G）

| 路径 | 耗时 | 写请求 |
|---|---|---|
| 非 `CONCURRENTLY` | 432 s | 构建期间写全阻塞（79/79 采样有锁等待） |
| `CONCURRENTLY` | 3236 s | p50 2.7 ms / p95 154 ms / 超时 1.1%，0 锁等待 |

旧形态 p50 498 ms（Seq Scan）→ 新形态 21 ms（Index Scan）。

> 当日首轮 `workload.py` 有个度量 bug：对 `UPDATE` 调用 `fetchall()` 会抛
> `ProgrammingError`，把成功写记成失败。报告里的数字是按延迟重算的
> （≥4.9s 记为超时），脚本已修复并新增 `err` 列。

<!-- kimi-badge -->
[![powered by KimiCode](../../.github/assets/kimi-badge.svg)](https://www.kimi.com/code)
