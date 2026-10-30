#!/usr/bin/env python3
"""对比 015 之前/之后的 ANN 查询形态（同一数据、同一批查询向量）。

用法: bench_ann.py old|new <iters>
  old: (embedding <=> $1::vector)      —— 015 之前二进制发出的 SQL
  new: (embedding::halfvec(1024) <=> $1::halfvec(1024))
"""
import statistics
import sys
import time

import psycopg2

MODE = sys.argv[1]
ITERS = int(sys.argv[2]) if len(sys.argv) > 2 else 100
DSN = "dbname=mh_drill application_name=drill_bench"

if MODE == "old":
    SQL = (
        "select id, (embedding <=> %s::vector) as dist from memories "
        "where embedding is not null and status = 'active' order by dist limit 20"
    )
else:
    SQL = (
        "select id, (embedding::halfvec(1024) <=> %s::halfvec(1024)) as dist from memories "
        "where embedding is not null and status in ('active') order by dist limit 20"
    )

conn = psycopg2.connect(DSN)
conn.autocommit = True
cur = conn.cursor()
cur.execute(
    "select embedding::text from memories where embedding is not null "
    "order by random() limit %s",
    (ITERS,),
)
queries = [r[0] for r in cur.fetchall()]

cur.execute("explain (analyze, buffers) " + SQL, (queries[0],))
plan = "\n".join(r[0] for r in cur.fetchall())

lat = []
for q in queries:
    t0 = time.perf_counter()
    cur.execute(SQL, (q,))
    cur.fetchall()
    lat.append((time.perf_counter() - t0) * 1000)

lat.sort()


def pct(p):
    return round(lat[min(len(lat) - 1, int(len(lat) * p))], 1)


print(f"MODE={MODE} n={len(lat)}")
print(f"p50={pct(0.50)}ms p95={pct(0.95)}ms p99={pct(0.99)}ms max={round(lat[-1],1)}ms")
print("PLAN:")
print(plan)
