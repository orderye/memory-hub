#!/usr/bin/env python3
"""索引构建期间的读写负载：逐条落盘，可随时 SIGTERM 停止。

用法: workload.py <max_seconds> <out.csv>
两线程：写（update by id）、读（halfvec ANN），均带 5s 语句超时。
"""
import csv
import os
import random
import signal
import sys
import threading
import time

import psycopg2

MAX = float(sys.argv[1])
OUT = sys.argv[2]
DSN = "dbname=mh_drill application_name=drill_workload"
lock = threading.Lock()
stop = threading.Event()

admin = psycopg2.connect(DSN)
admin.autocommit = True
with admin.cursor() as cur:
    cur.execute("select id from memories tablesample system (0.5) limit 4000")
    ids = [r[0] for r in cur.fetchall()]
    cur.execute("select embedding::text from memories where embedding is not null limit 1")
    qvec = cur.fetchone()[0]
admin.close()

if not os.path.exists(OUT) or os.path.getsize(OUT) == 0:
    with open(OUT, "w", newline="") as f:
        csv.writer(f).writerow(["op", "ms", "ok", "err"])

out = open(OUT, "a", newline="")
writer = csv.writer(out)
buffered = 0

writer_sql = "update memories set updated_at = now() where id = %s"
reader_sql = (
    "select id from memories where status = 'active' and embedding is not null "
    "order by embedding::halfvec(1024) <=> %s::halfvec(1024) limit 20"
)


def worker(op, sql, interval):
    conn = psycopg2.connect(DSN)
    conn.autocommit = True
    with conn.cursor() as cur:
        cur.execute("set statement_timeout = '5000ms'")
        while not stop.is_set():
            param = random.choice(ids) if op == "w" else qvec
            t0 = time.perf_counter()
            ok = 1
            err = ""
            try:
                cur.execute(sql, (param,))
                # UPDATE 不产生结果集，对它调用 fetchall() 会抛 ProgrammingError，
                # 把成功写记成失败（2026-10-30 首轮演练踩过）。
                if op == "r":
                    cur.fetchall()
            except Exception as e:  # noqa: BLE001 - 记录任何失败原因
                ok = 0
                err = str(e).replace("\n", " ")[:80]
            ms = (time.perf_counter() - t0) * 1000
            with lock:
                global buffered
                writer.writerow([op, round(ms, 1), ok, err])
                buffered += 1
                if buffered % 20 == 0:
                    out.flush()
            time.sleep(interval)
    conn.close()


def on_term(*_):
    stop.set()


signal.signal(signal.SIGTERM, on_term)
signal.signal(signal.SIGINT, on_term)

threads = [
    threading.Thread(target=worker, args=("w", writer_sql, 0.05), daemon=True),
    threading.Thread(target=worker, args=("r", reader_sql, 0.05), daemon=True),
]
for t in threads:
    t.start()
deadline = time.time() + MAX
while time.time() < deadline and not stop.is_set():
    time.sleep(0.2)
stop.set()
for t in threads:
    t.join(timeout=10)
out.flush()
out.close()
print("workload stopped", flush=True)
