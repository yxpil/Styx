#!/usr/bin/env python3
"""Styx 并发压测。

用标准库 socket 直接说协议，而不是套一层 HTTP 客户端——压的是
**服务层自己**（accept、准入、会话表、序列化），不掺杂第三方库的
连接池行为。`--mode http` 时改成手写 GET，压的是 `styx web` 那条路径。

用法：

    # 先起服务
    ./target/release/styx --config e2e/ollama.toml serve --addr 127.0.0.1:7879
    # 再压
    python e2e/loadtest.py --addr 127.0.0.1:7879 --conns 64 --requests 50

    # 压 web（注意仍要用 release 二进制）
    ./target/release/styx --config e2e/ollama.toml web --addr 127.0.0.1:8770
    python e2e/loadtest.py --addr 127.0.0.1:8770 --mode http --conns 64 --requests 50

输出里最该看的是 **P99** 和 **被拒数**：
P99 决定"少数人有多难受"，被拒数决定"配额是不是定小了"。
"""

from __future__ import annotations

import argparse
import socket
import statistics
import sys
import threading
import time


def recv_until_close(sock: socket.socket) -> bytes:
    chunks = []
    while True:
        try:
            b = sock.recv(65536)
        except OSError:
            break
        if not b:
            break
        chunks.append(b)
    return b"".join(chunks)


def recv_line(sock: socket.socket) -> bytes:
    buf = b""
    while not buf.endswith(b"\n"):
        b = sock.recv(65536)
        if not b:
            break
        buf += b
    return buf


# 服务端的配额拒绝长得像 `{"ok":false,"error":"服务繁忙：连接数已达上限"}`。
# 判定"被拒"和判定"失败"必须分开：被拒是**预期行为**（说明配额在起作用），
# 混进失败里会让人以为服务端崩了。
QUOTA_HINTS = ("上限", "繁忙", "too many", "busy", "refused")

# 鉴权失败同理，必须单独一类 —— 否则 `--mode http` 压一个开了令牌的服务时，
# 满屏 401 会被算成"成功"，量出来的是 401 快路径的吞吐，毫无意义。
AUTH_HINTS = ("缺少或错误的访问令牌", "unauthorized", "forbidden")


def _classify(body: bytes) -> str:
    text = body.decode("utf-8", "ignore").lower()
    if any(hint.lower() in text for hint in QUOTA_HINTS):
        return "quota"
    if any(hint.lower() in text for hint in AUTH_HINTS):
        return "auth"
    return "ok"


def one_request(addr, mode: str, token: str | None = None) -> tuple[float, bytes]:
    """发一个请求，返回 (耗时秒, 响应体)。每次新建连接是刻意的：
    要压的正是"连接建立 → 准入 → 处理 → 关闭"这一整条路径。"""
    t0 = time.perf_counter()
    with socket.create_connection(addr, timeout=15) as s:
        s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        if mode == "http":
            auth = f"Authorization: Bearer {token}\r\n" if token else ""
            s.sendall(
                f"GET /api/state HTTP/1.1\r\nHost: styx\r\n{auth}Connection: close\r\n\r\n".encode()
            )
            body = recv_until_close(s)
        else:
            s.sendall(b'{"op":"ping"}\n')
            body = recv_line(s)
            s.shutdown(socket.SHUT_WR)
            recv_until_close(s)
    return time.perf_counter() - t0, body


def worker(addr, mode, count, token, latencies, errors, busy, denied, lock):
    for _ in range(count):
        try:
            dt, body = one_request(addr, mode, token)
        except Exception as e:  # noqa: BLE001 —— 压测脚本要的是"失败长什么样"
            with lock:
                errors.append(repr(e))
            continue
        kind = _classify(body)
        if kind == "quota":
            with lock:
                busy.append(1)
        elif kind == "auth":
            with lock:
                denied.append(1)
        else:
            with lock:
                latencies.append(dt)


def percentile(sorted_values, p: float) -> float:
    if not sorted_values:
        return 0.0
    k = (len(sorted_values) - 1) * p
    lo, hi = int(k), min(int(k) + 1, len(sorted_values) - 1)
    return sorted_values[lo] + (sorted_values[hi] - sorted_values[lo]) * (k - lo)


def main() -> int:
    ap = argparse.ArgumentParser(description="Styx 并发压测")
    ap.add_argument("--addr", default="127.0.0.1:7879", help="服务地址 host:port")
    ap.add_argument("--mode", choices=["ndjson", "http"], default="ndjson")
    ap.add_argument("--conns", type=int, default=32, help="并发连接数")
    ap.add_argument("--requests", type=int, default=20, help="每个连接发几个请求")
    ap.add_argument(
        "--token",
        default=None,
        help="访问令牌（仅 --mode http 用得上；服务端开了 [auth] 就必须给）",
    )
    args = ap.parse_args()

    host, _, port = args.addr.rpartition(":")
    try:
        addr = (host or "127.0.0.1", int(port))
    except ValueError:
        print(f"地址格式不对：{args.addr}（应当形如 127.0.0.1:7879）", file=sys.stderr)
        return 2

    latencies: list[float] = []
    errors: list[str] = []
    busy: list[int] = []
    denied: list[int] = []
    lock = threading.Lock()

    total = args.conns * args.requests
    print(f"目标 {addr[0]}:{addr[1]} · 模式 {args.mode} · 并发 {args.conns} × {args.requests} = {total} 个请求")

    started = time.perf_counter()
    threads = [
        threading.Thread(
            target=worker,
            args=(addr, args.mode, args.requests, args.token, latencies, errors, busy, denied, lock),
        )
        for _ in range(args.conns)
    ]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    wall = time.perf_counter() - started

    ok = len(latencies)
    latencies.sort()
    print()
    print(f"耗时        {wall:.2f} s")
    print(f"成功        {ok} / {total}")
    print(f"被拒（配额） {len(busy)}")
    print(f"被拒（鉴权） {len(denied)}")
    print(f"失败        {len(errors)}")
    if ok:
        print(f"吞吐        {ok / wall:,.0f} req/s")
        print(f"平均        {statistics.fmean(latencies) * 1000:.2f} ms")
        print(f"P50         {percentile(latencies, 0.50) * 1000:.2f} ms")
        print(f"P95         {percentile(latencies, 0.95) * 1000:.2f} ms")
        print(f"P99         {percentile(latencies, 0.99) * 1000:.2f} ms")
        print(f"最慢        {latencies[-1] * 1000:.2f} ms")
    if errors[:3]:
        print()
        print("前几条失败：")
        for e in errors[:3]:
            print("  ", e)
    return 0 if not errors else 1


if __name__ == "__main__":
    sys.exit(main())
