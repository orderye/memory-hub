#!/usr/bin/env python3
"""向 memories 表流式灌入 N 行 1024 维向量（COPY text）。

用法：PGPASSWORD=... python3 seed_memories.py <rows> [dbname]
依赖：python3-numpy
"""
import datetime
import os
import subprocess
import sys
import uuid

import numpy as np

rows = int(sys.argv[1])
db = sys.argv[2] if len(sys.argv) > 2 else "mh_drill"
dim = 1024
batch = 2000
rng = np.random.default_rng(42)
base = datetime.datetime.utcnow()

copy_cmd = (
    "COPY memories (id, namespace, key, kind, content, tags, metadata, embedding, "
    "embedding_model, source_agent, confidence, status, version, created_at, updated_at, "
    "tsv_tokens) FROM STDIN"
)
proc = subprocess.Popen(
    ["runuser", "-u", "postgres", "--", "psql", "-v", "ON_ERROR_STOP=1", "-d", db, "-c", copy_cmd],
    stdin=subprocess.PIPE,
    stdout=subprocess.DEVNULL,
    text=True,
    bufsize=1 << 20,
)
assert proc.stdin is not None

done = 0
while done < rows:
    m = min(batch, rows - done)
    vecs = rng.standard_normal((m, dim), dtype=np.float32)
    vecs /= np.linalg.norm(vecs, axis=1, keepdims=True)
    lines = []
    for i in range(m):
        v = vecs[i]
        arr = "[" + ",".join(f"{x:.5f}" for x in v.tolist()) + "]"
        ts = (base - datetime.timedelta(seconds=int(rng.integers(0, 30 * 86400)))).isoformat(sep=" ")
        rid = uuid.uuid4()
        lines.append(
            f"{rid}\tdrill\t\\N\tfact\tdrill memory {done + i} about topic {i % 97}"
            f"\t{{}}\t{{}}\t{arr}\tbgem3\tdrill\t0.9\tactive\t1\t{ts}\t{ts}\t\n"
        )
    proc.stdin.write("".join(lines))
    done += m
    if done % 20000 == 0:
        print(f"seeded {done}", file=sys.stderr, flush=True)

proc.stdin.close()
rc = proc.wait()
print(f"COPY finished rc={rc} rows={rows}", file=sys.stderr)
sys.exit(rc)
