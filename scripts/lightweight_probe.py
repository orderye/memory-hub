#!/usr/bin/env python3
"""Memory Hub 轻量化升级 · 可复现启动与延迟探针（不依赖 PostgreSQL / 嵌入服务）。

用途
----
在**内存存储**（`HUB_DB_URL=memory://local`）上测量：
  1. 进程启动到 `/healthz` 返回 200 的耗时；
  2. 空闲 RSS（默认在启动后静置 3 秒再读）；
  3. `memory_put` / `memory_get` / `memory_search` 的 p50 / p95 延迟。

口径声明
--------
内存存储**不代表**生产 PostgreSQL 性能：本脚本产出的数字只能用于
"同一台机器上，同一二进制在不同配置下的相对对比"（例如关闭嵌入前后）。
真实 PG / 真实嵌入端点的数字必须在具备该环境时另行测量，并记为 NOT MEASURED。

用法
----
    python3 scripts/lightweight_probe.py --label before
    python3 scripts/lightweight_probe.py --label after-lite \
        --profile lite --embed-provider off --out target/baseline/after-lite.json

令牌
----
用 `HUB_TOKEN_SECRET` 离线签发 HS256 访问令牌（与 auth-server 同算法同 claims），
因此无需启动授权服务器即可调用受保护的 MCP 工具。
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import hmac
import json
import os
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
import uuid

ACCOUNT_ID = str(uuid.UUID(int=1))  # common::BOOTSTRAP_ACCOUNT_ID
DEFAULT_SECRET = "lightweight-probe-secret-at-least-32-bytes"


def b64url(raw: bytes) -> str:
    return base64.urlsafe_b64encode(raw).decode().rstrip("=")


def mint_token(secret: str, issuer: str, audience: str, namespace: str) -> str:
    now = int(time.time())
    header = {"alg": "HS256", "typ": "JWT"}
    claims = {
        "sub": "probe-agent",
        "iss": issuer,
        "aud": audience,
        "client_id": "probe-agent",
        "account": ACCOUNT_ID,
        "scopes": ["memory:read", "memory:write", "memory:admin"],
        "grants": [{"namespace": namespace, "read": True, "write": True}],
        "iat": now,
        "exp": now + 3600,
        "jti": str(uuid.uuid4()),
    }
    signing_input = f"{b64url(json.dumps(header, separators=(',', ':')).encode())}." \
                    f"{b64url(json.dumps(claims, separators=(',', ':')).encode())}"
    sig = hmac.new(secret.encode(), signing_input.encode(), hashlib.sha256).digest()
    return f"{signing_input}.{b64url(sig)}"


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def wait_health(port: int, deadline_s: float) -> float | None:
    start = time.perf_counter()
    while time.perf_counter() - start < deadline_s:
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/healthz", timeout=1) as resp:
                if resp.status == 200:
                    return (time.perf_counter() - start) * 1000.0
        except (urllib.error.URLError, ConnectionError, OSError):
            pass
        time.sleep(0.02)
    return None


def rss_kb(pid: int) -> int | None:
    """读取子进程常驻内存（KB）。

    不使用 `ps`：本机沙箱只允许执行工作区内的二进制，`ps` 会被拒绝（EPERM）。
    优先走 macOS libproc 的 `proc_pidinfo(PROC_PIDTASKINFO)`，取 `proc_taskinfo`
    前两个字段中的 `pti_resident_size`（偏移 8，uint64）；其它平台返回 None。
    """
    try:
        import ctypes

        libproc = ctypes.CDLL("/usr/lib/libproc.dylib")
        PROC_PIDTASKINFO = 4
        buf = ctypes.create_string_buffer(256)
        written = libproc.proc_pidinfo(ctypes.c_int(pid), ctypes.c_int(PROC_PIDTASKINFO),
                                        ctypes.c_uint64(0), buf, ctypes.c_int(256))
        if written <= 0:
            return None
        resident = ctypes.cast(ctypes.byref(buf, 8), ctypes.POINTER(ctypes.c_uint64))[0]
        return int(resident) // 1024
    except Exception:
        return None


def call(url: str, token: str, tool: str, arguments: dict) -> tuple[float, dict]:
    payload = json.dumps(
        {
            "jsonrpc": "2.0",
            "id": str(uuid.uuid4()),
            "method": "tools/call",
            "params": {"name": tool, "arguments": arguments},
        }
    ).encode()
    req = urllib.request.Request(
        url,
        data=payload,
        headers={
            "content-type": "application/json",
            "authorization": f"Bearer {token}",
            "accept": "application/json",
        },
        method="POST",
    )
    t0 = time.perf_counter()
    with urllib.request.urlopen(req, timeout=30) as resp:
        body = json.loads(resp.read().decode())
    return (time.perf_counter() - t0) * 1000.0, body


def percentile(values: list[float], p: float) -> float:
    if not values:
        return float("nan")
    ordered = sorted(values)
    idx = min(len(ordered) - 1, max(0, int(round(p * (len(ordered) - 1)))))
    return ordered[idx]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default="target/release/mcp-server")
    ap.add_argument("--label", default="run")
    ap.add_argument("--profile", default="production", help="HUB_DEPLOYMENT_PROFILE")
    ap.add_argument("--embed-provider", default="mock", help="HUB_EMBED_PROVIDER")
    ap.add_argument("--iterations", type=int, default=30)
    ap.add_argument(
        "--bulk-entries",
        type=int,
        default=0,
        help="额外写入 N 条记忆后测量 RSS（用于观察\"是否落向量\"对常驻内存的影响）",
    )
    ap.add_argument("--idle-settle-ms", type=int, default=3000)
    ap.add_argument("--out", default=None, help="把结果 JSON 写入该路径")
    args = ap.parse_args()

    if not os.path.exists(args.binary):
        print(f"[probe] 二进制不存在：{args.binary}（先 cargo build --release）", file=sys.stderr)
        return 2

    port = free_port()
    namespace = "probe"
    secret = os.environ.get("HUB_TOKEN_SECRET", DEFAULT_SECRET)
    issuer = "https://auth.memory-hub.local"
    audience = "https://hub.memory-hub.local"

    env = dict(os.environ)
    env.update(
        {
            "HUB_SERVE_MODE": "http",
            "HUB_DB_URL": "memory://local",
            "HUB_MCP_BIND": f"127.0.0.1:{port}",
            "HUB_TOKEN_SECRET": secret,
            "AUTH_ISSUER": issuer,
            "HUB_RESOURCE_URI": audience,
            "HUB_DEPLOYMENT_PROFILE": args.profile,
            "HUB_EMBED_PROVIDER": args.embed_provider,
            "RATE_LIMIT_PER_AGENT_RPS": "0",
            "LOG_REDACTION": "always",
        }
    )

    proc = subprocess.Popen(
        [args.binary],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    result: dict = {
        "label": args.label,
        "profile": args.profile,
        "embed_provider": args.embed_provider,
        "storage": "memory://local",
        "iterations": args.iterations,
        "platform": f"{sys.platform}/{os.uname().machine}",
    }
    ok = True
    try:
        startup = wait_health(port, deadline_s=30)
        result["startup_ms"] = None if startup is None else round(startup, 2)
        if startup is None:
            print("[probe] 启动超时：30s 内 /healthz 未就绪", file=sys.stderr)
            return 3

        time.sleep(max(0, args.idle_settle_ms) / 1000.0)
        result["idle_rss_kb"] = rss_kb(proc.pid)

        token = mint_token(secret, issuer, audience, namespace)
        base = f"http://127.0.0.1:{port}/mcp"

        if args.bulk_entries > 0:
            # 观察"是否落向量"对常驻内存的影响：1024 维 f32 ≈ 4KB/条
            for i in range(args.bulk_entries):
                call(
                    base,
                    token,
                    "memory_put",
                    {
                        "namespace": namespace,
                        "key": f"bulk:{i}",
                        "kind": "note",
                        "content": f"批量样本 {i}：部署在华东区域，服务端口 3000，版本 v{i}.0",
                    },
                )
            time.sleep(1.0)
            result["bulk_entries"] = args.bulk_entries
            result["rss_after_bulk_kb"] = rss_kb(proc.pid)

        # 预热（不计入统计）
        for i in range(5):
            call(base, token, "memory_put",
                 {"namespace": namespace, "key": f"warm:{i}", "kind": "fact",
                  "content": f"预热记忆 {i} 端口 3000"})

        put_ms: list[float] = []
        get_ms: list[float] = []
        search_ms: list[float] = []
        errors: list[str] = []

        for i in range(args.iterations):
            key = f"probe:{i}"
            content = f"轻量化探针第 {i} 条：服务端口 3000，环境 linux，版本 v{i}.0"
            ms, body = call(base, token, "memory_put",
                            {"namespace": namespace, "key": key, "kind": "fact", "content": content})
            put_ms.append(ms)
            if body.get("result", {}).get("isError"):
                errors.append(f"put#{i}: {json.dumps(body)[:200]}")

            ms, body = call(base, token, "memory_get",
                            {"namespace": namespace, "key": key})
            get_ms.append(ms)
            if body.get("result", {}).get("isError"):
                errors.append(f"get#{i}: {json.dumps(body)[:200]}")

            ms, body = call(base, token, "memory_search",
                            {"namespace": namespace, "query_text": f"轻量化探针第 {i} 条",
                             "top_k": 5})
            search_ms.append(ms)
            if body.get("result", {}).get("isError"):
                errors.append(f"search#{i}: {json.dumps(body)[:200]}")

        def summarize(values: list[float]) -> dict:
            return {
                "count": len(values),
                "p50_ms": round(percentile(values, 0.50), 3),
                "p95_ms": round(percentile(values, 0.95), 3),
                "max_ms": round(max(values), 3) if values else None,
            }

        result["latency"] = {
            "put": summarize(put_ms),
            "get": summarize(get_ms),
            "search": summarize(search_ms),
        }
        result["errors"] = errors[:5]
        result["ok"] = not errors
        ok = not errors
    finally:
        try:
            proc.send_signal(signal.SIGTERM)
            proc.wait(timeout=10)
        except Exception:
            proc.kill()

    text = json.dumps(result, ensure_ascii=False, indent=2)
    print(text)
    if args.out:
        os.makedirs(os.path.dirname(os.path.abspath(args.out)), exist_ok=True)
        with open(args.out, "w", encoding="utf-8") as fh:
            fh.write(text + "\n")
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
