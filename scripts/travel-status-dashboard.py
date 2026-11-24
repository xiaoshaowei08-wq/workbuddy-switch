#!/usr/bin/env python3
"""派猫猫旅行实时看板（只读）。

读取 ~/.wb-switch 下的 auto_travel_config.json / travel_cache.json / accounts.json，
在本地起一个轻量 HTTP 页面，展示每个账号的旅行进度、倒计时与今日汇总。
不调用任何官方接口、不写文件、不输出任何 token；账号库只读取展示字段。
"""

import json
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

STORE = Path.home() / ".wb-switch"
PORT = 57891
# 生产构建的前端产物（npm run build 的输出）。本服务用它替换预编译二进制里
# 嵌入的旧前端；所有 /api 请求仍反向代理回真正在跑的 57890 服务。
DIST = Path(__file__).resolve().parent.parent / "dist"
UPSTREAM = "http://127.0.0.1:57890"

MIME = {
    ".html": "text/html; charset=utf-8",
    ".js": "text/javascript; charset=utf-8",
    ".css": "text/css; charset=utf-8",
    ".json": "application/json; charset=utf-8",
    ".png": "image/png",
    ".svg": "image/svg+xml",
    ".ico": "image/x-icon",
    ".woff2": "font/woff2",
    ".woff": "font/woff",
    ".webp": "image/webp",
    ".map": "application/json",
    ".webm": "video/webm",
}

# 网页端先派出的行程没有真实出发时刻；缓存里的 at 每轮循环都会被覆盖。
# 这里按 (账号, 到达时刻) 记住本看板首次观察到该行程的时刻，让进度单调前进。
FIRST_SEEN = {}

HTML = """<!doctype html>
<html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>派猫猫旅行 · 实时状态</title>
<style>
:root{color-scheme:dark}
*{box-sizing:border-box}
body{margin:0;background:#0f1115;color:#e8eaed;font:14px/1.6 -apple-system,BlinkMacSystemFont,"PingFang SC","Microsoft YaHei",sans-serif}
.wrap{max-width:760px;margin:0 auto;padding:28px 20px 40px}
h1{margin:0 0 4px;font-size:19px;font-weight:600}
.sub{color:#8b93a1;font-size:12px}
.bar{display:flex;align-items:center;justify-content:space-between;gap:12px;margin:20px 0 12px;flex-wrap:wrap}
.chips{display:flex;gap:8px;flex-wrap:wrap}
.chip{background:#1b1f27;border:1px solid #262b35;border-radius:999px;padding:3px 10px;font-size:12px;color:#aeb6c2}
.chip b{color:#e8eaed;font-weight:600}
button{background:#1b1f27;border:1px solid #313845;color:#cfd6e0;border-radius:7px;padding:5px 12px;font-size:12px;cursor:pointer}
button:hover{background:#242a34}
.card{background:#171a21;border:1px solid #262b35;border-radius:11px;padding:13px 15px;margin-bottom:10px}
.row{display:flex;align-items:center;justify-content:space-between;gap:10px}
.name{font-weight:600;font-size:13px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.tag{font-size:11px;border-radius:6px;padding:2px 8px;white-space:nowrap}
.traveling{background:rgba(90,169,248,.15);color:#79baff}
.finished{background:rgba(62,207,142,.15);color:#5fdaa5}
.no-buddy{background:rgba(240,180,41,.15);color:#f0c45e}
.untraveled{background:rgba(139,147,161,.14);color:#9aa2af}
.track{height:6px;background:#222833;border-radius:99px;overflow:hidden;margin:11px 0 8px}
.fill{height:100%;background:#5aa9f8;border-radius:99px}
.meta{display:flex;gap:14px;flex-wrap:wrap;font-size:12px;color:#8b93a1}
.meta .hot{color:#79baff}
.foot{margin-top:18px;font-size:12px;color:#707988;line-height:1.8}
.err{color:#f08a8a}
</style></head><body><div class="wrap">
<h1>派猫猫旅行 · 实时状态</h1>
<div class="sub" id="head">正在读取…</div>
<div class="bar"><div class="chips" id="chips"></div><button onclick="load()">立即刷新</button></div>
<div id="list"></div>
<div class="foot" id="foot">数据来源：~/.wb-switch/travel_cache.json（后台循环写入）。倒计时与进度条由浏览器按秒实时计算，状态每 30 秒自动重读。</div>
</div><script>
let data=null;
const esc=(s)=>String(s??'').replace(/[&<>"]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
const LABEL={traveling:'旅行中',finished:'已结束','no-buddy':'无 Buddy',untraveled:'未旅行'};
function clock(sec){return new Date(sec*1000).toLocaleTimeString('zh-CN',{hour:'2-digit',minute:'2-digit',hour12:false});}
function remain(arriveAt,now){if(!arriveAt)return null;const s=arriveAt*1000-now;if(s<=0)return '即将到达';const m=Math.ceil(s/60000),h=Math.floor(m/60),r=m%60;return h>0?(r>0?`剩余 ${h} 小时 ${r} 分钟`:`剩余 ${h} 小时`):`剩余 ${r} 分钟`;}
function progress(r,nowSec){if(r.label!=='traveling'||!r.arriveAt)return null;const start=(!r.already&&r.departAt)?r.departAt:r.observedAt;if(!start)return null;const total=r.arriveAt-start;if(total<=0)return null;return Math.min(100,Math.max(0,(nowSec-start)/total*100));}
function render(){
 const now=Date.now();
 const d=data;
 document.getElementById('head').textContent=`自动派发${d.enabled?'已开启':'已关闭'} · 今日 ${d.date} · 上次同步 ${d.lastUpdatedAt?clock(d.lastUpdatedAt/1000):'尚无记录'}`;
 document.getElementById('chips').innerHTML=[
  ['旅行中',d.summary.traveling],['已结束',d.summary.finished],['无 Buddy',d.summary['no-buddy']],['未旅行',d.summary.untraveled]
 ].map(([t,c])=>`<span class="chip">${t} <b>${c}</b></span>`).join('');
 document.getElementById('list').innerHTML=d.accounts.length===0?'<div class="card">暂无支持成长中心的国内版账号</div>':d.accounts.map(r=>{
  const pct=progress(r,now/1000),left=r.label==='traveling'?remain(r.arriveAt,now):null;
  const bits=[];
  if(r.locationName)bits.push(esc(r.locationName));
  if(r.rewardCredit!=null)bits.push(`预计 +${r.rewardCredit} 积分`);
  if(left)bits.push(`<span class="hot">${left}</span>`);
  if(r.label==='traveling'&&r.arriveAt)bits.push(`预计 ${clock(r.arriveAt)} 到达`);
  if(r.already&&r.observedAt)bits.push(`观察起点 ${clock(r.observedAt)}（网页端派出，无出发时刻）`);
  if(r.label==='no-buddy')bits.push('等待下一轮自动重试');
  if(r.label==='untraveled')bits.push('等待派发');
  if(r.label==='finished')bits.push(r.claimed?'奖励已领取':'行程已结束');
  return `<div class="card"><div class="row"><span class="name">${esc(r.email)}</span><span class="tag ${r.label}">${LABEL[r.label]||r.label}</span></div>
  ${pct!==null?`<div class="track"><div class="fill" style="width:${pct.toFixed(1)}%"></div></div>`:''}
  <div class="meta">${bits.map(b=>`<span>${b}</span>`).join('')}</div></div>`;
 }).join('');
 document.getElementById('foot').textContent=`派发每 ${d.dispatchIntervalMinutes} 分钟 · 领取每 ${d.claimIntervalMinutes} 分钟；${d.enabled?(d.completed?'今日已完成，无可重试项。':'后台持续补派中。'):'自动派发已关闭，仅展示已有行程。'} 数据来源：~/.wb-switch/travel_cache.json，倒计时与进度条按秒实时计算，状态每 30 秒自动重读。`;
}
async function load(){try{const res=await fetch('/api/state',{cache:'no-store'});if(!res.ok)throw new Error('HTTP '+res.status);data=await res.json();render();}catch(e){document.getElementById('head').innerHTML='<span class="err">状态读取失败：'+esc(e.message)+'</span>';}}
setInterval(()=>{if(data)render();},1000);
setInterval(load,30000);
load();
</script></body></html>"""


def read_json(name, default):
    try:
        with (STORE / name).open(encoding="utf-8") as handle:
            return json.load(handle)
    except (OSError, ValueError):
        return default


def display_name(account):
    for key in ("note", "nickname", "email"):
        value = account.get(key)
        if isinstance(value, str) and value.strip():
            return value.strip()
    return account.get("id") or "未知账号"


def state():
    config = read_json("auto_travel_config.json", {})
    cache = read_json("travel_cache.json", {})
    accounts = read_json("accounts.json", [])
    if isinstance(accounts, dict):
        accounts = accounts.get("accounts", [])
    results = cache.get("results") or {}
    today = time.strftime("%Y-%m-%d")
    same_day = cache.get("date") == today

    rows = []
    last_updated = 0
    for account in accounts:
        if account.get("variant") == "ai":
            continue
        account_id = account.get("id") or ""
        result = results.get(account_id) or {}
        at_ms = result.get("at") or 0
        if isinstance(at_ms, (int, float)):
            last_updated = max(last_updated, at_ms)
        ok = result.get("ok") is True
        claimed = result.get("claimed") is True
        skip = result.get("skip")
        if ok and not claimed:
            label = "traveling"
        elif same_day and skip == "no-buddy":
            label = "no-buddy"
        elif same_day and claimed:
            label = "finished"
        else:
            label = "untraveled"
        observed_at = None
        if label == "traveling" and result.get("arriveAt"):
            key = (account_id, result["arriveAt"])
            observed_at = FIRST_SEEN.setdefault(key, int(time.time()))
        rows.append({
            "accountId": account_id,
            "email": display_name(account),
            "label": label,
            "locationName": result.get("locationName"),
            "rewardCredit": result.get("rewardCredit"),
            "arriveAt": result.get("arriveAt"),
            "departAt": int(at_ms / 1000) if at_ms else None,
            "observedAt": observed_at,
            "already": result.get("already") is True,
            "claimed": claimed,
            "message": result.get("message"),
            "skip": skip,
        })

    order = {"traveling": 0, "finished": 1, "no-buddy": 2, "untraveled": 3}
    rows.sort(key=lambda row: order.get(row["label"], 9))
    summary = {
        key: sum(1 for row in rows if row["label"] == key)
        for key in ("traveling", "finished", "no-buddy", "untraveled")
    }
    return {
        "enabled": config.get("enabled") is True,
        "date": cache.get("date") or today,
        "completed": cache.get("completed") is True,
        "lastUpdatedAt": last_updated or None,
        "dispatchIntervalMinutes": 30,
        "claimIntervalMinutes": 15,
        "summary": summary,
        "accounts": rows,
    }


def overview():
    """按 Rust travel_overview() 的字段结构组装只读快照（前端 TravelOverview 类型）。"""
    snapshot = state()
    summary = snapshot["summary"]
    return {
        "enabled": snapshot["enabled"],
        "date": snapshot["date"],
        "completed": snapshot["completed"],
        "lastUpdatedAt": snapshot["lastUpdatedAt"],
        "dispatchIntervalMinutes": 30,
        "claimIntervalMinutes": 15,
        "summary": {
            "total": len(snapshot["accounts"]),
            "traveling": summary["traveling"],
            "finished": summary["finished"],
            "noBuddy": summary["no-buddy"],
            "untraveled": summary["untraveled"],
        },
        "accounts": [
            {key: value for key, value in row.items() if key != "observedAt"}
            for row in snapshot["accounts"]
        ],
    }


class Handler(BaseHTTPRequestHandler):
    """新版前端 + 本地 overview 垫片 + 其余请求透明代理到 57890。"""

    def _send(self, status, body, content_type):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Cache-Control", "no-store")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def _json(self, payload, status=200):
        self._send(status, json.dumps(payload, ensure_ascii=False).encode("utf-8"),
                   "application/json; charset=utf-8")

    def _static(self, relative):
        target = (DIST / relative).resolve()
        root = DIST.resolve()
        if root not in target.parents or not target.is_file():
            return False
        self._send(200, target.read_bytes(), MIME.get(target.suffix, "application/octet-stream"))
        return True

    def _proxy(self):
        length = int(self.headers.get("Content-Length") or 0)
        payload = self.rfile.read(length) if length else None
        headers = {
            key: value
            for key, value in self.headers.items()
            if key.lower() not in ("host", "content-length", "connection", "accept-encoding")
        }
        request = urllib.request.Request(
            UPSTREAM + self.path, data=payload, method=self.command, headers=headers
        )
        try:
            with urllib.request.urlopen(request, timeout=30) as upstream:
                body = upstream.read()
                status = upstream.status
                content_type = upstream.headers.get("Content-Type", "application/octet-stream")
        except urllib.error.HTTPError as exc:
            body = exc.read()
            status = exc.code
            content_type = exc.headers.get("Content-Type", "application/octet-stream")
        except Exception as exc:  # noqa: BLE001
            self._json({"error": "%s: %s" % (type(exc).__name__, exc)}, 502)
            return
        self._send(status, body, content_type)

    def do_GET(self):
        path = self.path.split("?")[0]
        if path == "/api/travel/overview":
            self._json(overview())
            return
        if path == "/api/state":
            self._json(state())
            return
        if path in ("/dashboard", "/dashboard/"):
            self._send(200, HTML.encode("utf-8"), "text/html; charset=utf-8")
            return
        if path.startswith("/api/"):
            self._proxy()
            return
        relative = "index.html" if path in ("", "/") else path.lstrip("/")
        if self._static(relative) or self._static("index.html"):
            return
        self._proxy()

    def do_POST(self):
        self._proxy()

    def do_PUT(self):
        self._proxy()

    def do_DELETE(self):
        self._proxy()

    def do_HEAD(self):
        self.do_GET()

    def log_message(self, *_args):
        pass


if __name__ == "__main__":
    for port in range(PORT, PORT + 10):
        try:
            server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
            break
        except OSError:
            continue
    else:
        raise SystemExit("no available port")
    print(f"http://127.0.0.1:{server.server_address[1]}", flush=True)
    server.serve_forever()
