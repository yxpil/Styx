"""Styx × phi4 web E2E：多轮对话 + 通道分流检查。

用 HTTP 打真实接口，不走任何 mock。每轮把模型的四个通道
（台词 / 动作 / 内心 / 素材）分开打印，便于一眼看出哪一路丢了。
"""
import json
import sys
import time
import urllib.request

BASE = "http://127.0.0.1:8770"
SESSION = sys.argv[1] if len(sys.argv) > 1 else "e2e"

# 显式绕开 HTTP 代理：这台机器设了 HTTP_PROXY，连 127.0.0.1 也会被转发，
# 于是后端一旦没起来，看到的不是"连接被拒"而是代理吐的 502 Bad Gateway——
# 那种错误信息会把"服务没起"误导成"服务起了但上游挂了"。
_OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def call(path, payload=None, params="", timeout=600, method="POST"):
    url = f"{BASE}{path}?session={SESSION}{params}"
    data = json.dumps(payload).encode() if payload is not None else None
    if method == "POST" and data is None:
        data = b"{}"
    req = urllib.request.Request(
        url, data=data, headers={"Content-Type": "application/json"}, method=method
    )
    with _OPENER.open(req, timeout=timeout) as r:
        return json.loads(r.read().decode())


def show(title, d):
    reply = d.get("reply") or {}
    def fmt(items):
        if not items:
            return "—"
        return "\n".join(f"      {x if isinstance(x, str) else x.get('key', x)}" for x in items)
    print(f"\n--- {title} ---")
    print(f"  ok={d.get('ok')}  turn={d.get('turn')}  端点={((d.get('usage') or {}).get('endpoint'))}")
    print(f"  台词:\n{fmt(reply.get('speech'))}")
    print(f"  动作:\n{fmt(reply.get('actions'))}")
    print(f"  内心:\n{fmt(reply.get('thoughts'))}")
    print(f"  表情包: {reply.get('stickers') or '—'}")
    print(f"  图片:   {reply.get('images') or '—'}")
    if d.get("recalled"):
        print(f"  召回: {[r.get('text','')[:30] for r in d['recalled']]}")
    if d.get("prompt"):
        print(f"  提示: {d['prompt'].splitlines()[0]}")


def main():
    call("/api/reset")
    turns = [
        "我今天特别开心，遇到了很久没见的老朋友！",
        "（我笑得很夸张，还比了个耶）",
        "唉……其实我妈去年也走了。",
        "你还好吗？我感觉你刚才愣了一下。",
    ]
    for i, text in enumerate(turns, 1):
        t0 = time.time()
        d = call("/api/say", {"text": text})
        show(f"turn {i}  用户：{text}  （{time.time()-t0:.1f}s）", d)

    # 反向通道：用户发表情包
    t0 = time.time()
    d = call("/api/sticker", {"id": "cry_03"})
    show(f"用户发来一张 cry_03 表情包（{time.time()-t0:.1f}s）", d)

    # 汇总：每轮四个通道各有多少条
    print("\n=== 通道分流汇总 ===")
    ev = call("/api/events", None, "&limit=200", method="GET")
    events = ev.get("events") or []
    if isinstance(events, dict):
        events = events.get("events") or []
    kinds = {}
    for e in events:
        kinds[e.get("kind")] = kinds.get(e.get("kind"), 0) + 1
    print("  事件类型计数:", kinds)


if __name__ == "__main__":
    main()
