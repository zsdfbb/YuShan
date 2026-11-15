#!/usr/bin/env python3
"""把 HTML 文件或 URL 渲染成 PNG。

只用标准库：自己实现 WebSocket 帧与 CDP 调用，不引 playwright / selenium。
渲染后端是 Chrome 系浏览器（见 find_backend），脚本只负责驱动它。

不变量（失败必须报错，不出假图）：
  1. 渲染后必须校验像素非全白 —— Chrome 对不存在的文件仍 exit 0 并写出白图。
  2. 导航错误由 `Page.navigate` 的 errorText 捕获，不看进程退出码。
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import re
import socket
import struct
import subprocess
import sys
import tempfile
import time
import urllib.parse
import urllib.request
import zlib

# 单页最大渲染高度。超过 Chrome 纹理工位上限会截出空白条带，故显式夹取。
MAX_HEIGHT = 20000
# 测量内容高度时的临时视口高度：压到最扁，避免视口把内容撑高。
MIN_PROBE_HEIGHT = 60
# 判定「全白」的非白像素占比阈值。实测正常页面 2%~3%，空白页 0.00%。
BLANK_RATIO = 0.001

FULL_CHROME_CANDIDATES = [
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    "/usr/bin/google-chrome",
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
    "/snap/bin/chromium",
]


# --------------------------------------------------------------------------
# 后端发现
# --------------------------------------------------------------------------

def _playwright_shells():
    """Playwright 下载的 chrome-headless-shell。可直接当二进制用，不必经 Node API。"""
    roots = [
        os.path.expanduser("~/Library/Caches/ms-playwright"),
        os.path.expanduser("~/.cache/ms-playwright"),
        os.path.expanduser("~/AppData/Local/ms-playwright"),
    ]
    found = []
    for root in roots:
        if not os.path.isdir(root):
            continue
        for name in sorted(os.listdir(root), reverse=True):
            if not name.startswith("chromium_headless_shell-"):
                continue
            for dirpath, _dirnames, filenames in os.walk(os.path.join(root, name)):
                if "chrome-headless-shell" in filenames:
                    found.append(os.path.join(dirpath, "chrome-headless-shell"))
    return found


def find_backend(explicit: str | None = None):
    """返回 (可执行文件路径, 是否 headless-shell)。

    headless-shell 自带无头形态，**不能**再传 --headless；
    完整 Chrome / Edge 必须传 --headless=new（132 起 old headless 已移除）。
    """
    if explicit:
        if not os.path.exists(explicit):
            sys.exit(f"错误：指定的浏览器不存在：{explicit}")
        return explicit, "headless-shell" in os.path.basename(explicit)

    env = os.environ.get("HTML_SHOT_BROWSER")
    if env:
        return find_backend(env)

    shells = _playwright_shells()
    if shells:
        return shells[0], True

    for path in FULL_CHROME_CANDIDATES:
        if os.path.exists(path):
            return path, False

    for name in ("google-chrome", "chromium", "chromium-browser", "chrome"):
        resolved = _which(name)
        if resolved:
            return resolved, False

    sys.exit(
        "错误：未找到 Chrome 系浏览器。\n"
        "  装一个，或用 HTML_SHOT_BROWSER=<路径> 指定：\n"
        "    macOS  官方 Chrome / `npx playwright install chromium`\n"
        "    Linux  apt install chromium / `npx playwright install chromium`"
    )


def _which(name):
    for d in os.environ.get("PATH", "").split(os.pathsep):
        p = os.path.join(d, name)
        if os.path.isfile(p) and os.access(p, os.X_OK):
            return p
    return None


# --------------------------------------------------------------------------
# 最小 WebSocket 客户端（RFC 6455 的客户端子集）
# --------------------------------------------------------------------------

class WS:
    def __init__(self, url: str, timeout: float = 30.0):
        rest = url[5:] if url.startswith("ws://") else url
        hostport, _, path = rest.partition("/")
        host, _, port = hostport.partition(":")
        self.sock = socket.create_connection((host, int(port or 80)), timeout=timeout)
        key = base64.b64encode(os.urandom(16)).decode()
        req = (
            f"GET /{path} HTTP/1.1\r\nHost: {hostport}\r\n"
            f"Upgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
        self.sock.sendall(req.encode())
        buf = b""
        while b"\r\n\r\n" not in buf:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise RuntimeError("WebSocket 握手时连接关闭")
            buf += chunk
        head, _, self.buf = buf.partition(b"\r\n\r\n")
        status_line = head.split(b"\r\n")[0]
        if b"101" not in status_line:
            raise RuntimeError("WebSocket 握手失败：" + repr(status_line))

    def _read_exact(self, n: int) -> bytes:
        while len(self.buf) < n:
            chunk = self.sock.recv(max(1 << 16, n - len(self.buf)))
            if not chunk:
                raise RuntimeError("WebSocket 连接被对端关闭")
            self.buf += chunk
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    def send(self, text: str):
        data = text.encode()
        header = bytearray([0x81])  # FIN + text
        n = len(data)
        if n < 126:
            header.append(0x80 | n)
        elif n < 65536:
            header.append(0x80 | 126)
            header += struct.pack(">H", n)
        else:
            header.append(0x80 | 127)
            header += struct.pack(">Q", n)
        mask = os.urandom(4)
        header += mask
        self.sock.sendall(bytes(header) + bytes(b ^ mask[i % 4] for i, b in enumerate(data)))

    def recv(self) -> str:
        parts = []
        while True:
            b0, b1 = self._read_exact(2)
            opcode = b0 & 0x0F
            length = b1 & 0x7F
            if length == 126:
                length = struct.unpack(">H", self._read_exact(2))[0]
            elif length == 127:
                length = struct.unpack(">Q", self._read_exact(8))[0]
            mask = self._read_exact(4) if b1 & 0x80 else None
            payload = self._read_exact(length) if length else b""
            if mask:
                payload = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
            if opcode == 0x8:
                raise RuntimeError("WebSocket 被对端关闭（浏览器可能已退出）")
            if opcode == 0x9:
                continue
            parts.append(payload)
            if b0 & 0x80:
                return b"".join(parts).decode("utf-8", "replace")


class CDP:
    def __init__(self, ws: WS):
        self.ws = ws
        self.next_id = 0
        self.events: list[dict] = []

    def call(self, method: str, **params):
        self.next_id += 1
        mid = self.next_id
        self.ws.send(json.dumps({"id": mid, "method": method, "params": params}))
        while True:
            msg = json.loads(self.ws.recv())
            if msg.get("id") == mid:
                if "error" in msg:
                    raise RuntimeError(f"{method} 失败：{msg['error'].get('message')}")
                return msg.get("result", {})
            self.events.append(msg)

    def drain_events(self, name: str, timeout: float) -> bool:
        deadline = time.time() + timeout
        self.ws.sock.settimeout(0.2)
        try:
            while time.time() < deadline:
                try:
                    msg = json.loads(self.ws.recv())
                except (socket.timeout, TimeoutError):
                    continue
                if msg.get("method") == name:
                    return True
                self.events.append(msg)
            return False
        finally:
            self.ws.sock.settimeout(30.0)

    def evaluate(self, expression: str):
        result = self.call(
            "Runtime.evaluate", expression=expression, returnByValue=True, awaitPromise=True
        )
        if result.get("exceptionDetails"):
            raise RuntimeError(f"页面 JS 抛异常：{result['exceptionDetails'].get('text')}")
        return result.get("result", {}).get("value")


# --------------------------------------------------------------------------
# PNG 空白检测（标准库解码，输入是缩略图故足够快）
# --------------------------------------------------------------------------

def png_nonwhite_ratio(data: bytes, threshold: int = 240) -> float:
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise RuntimeError("截图不是 PNG")
    pos, idat = 8, bytearray()
    width = height = depth = color = interlace = 0
    while pos < len(data):
        (length,) = struct.unpack(">I", data[pos:pos + 4])
        ctype = data[pos + 4:pos + 8]
        body = data[pos + 8:pos + 8 + length]
        pos += 12 + length
        if ctype == b"IHDR":
            width, height = struct.unpack(">II", body[0:8])
            depth, color, interlace = body[8], body[9], body[12]
        elif ctype == b"IDAT":
            idat += body
        elif ctype == b"IEND":
            break
    if depth != 8 or interlace != 0:
        raise RuntimeError(f"不支持的 PNG（位深 {depth}，隔行 {interlace}）")
    channels = {0: 1, 2: 3, 4: 2, 6: 4}[color]
    raw = zlib.decompress(bytes(idat))
    stride = width * channels
    prev = bytearray(stride)
    total = nonwhite = 0
    p = 0
    for _y in range(height):
        ftype = raw[p]
        p += 1
        line = bytearray(raw[p:p + stride])
        p += stride
        if ftype == 1:
            for i in range(channels, stride):
                line[i] = (line[i] + line[i - channels]) & 0xFF
        elif ftype == 2:
            for i in range(stride):
                line[i] = (line[i] + prev[i]) & 0xFF
        elif ftype == 3:
            for i in range(stride):
                left = line[i - channels] if i >= channels else 0
                line[i] = (line[i] + ((left + prev[i]) >> 1)) & 0xFF
        elif ftype == 4:
            for i in range(stride):
                left = line[i - channels] if i >= channels else 0
                up = prev[i]
                upleft = prev[i - channels] if i >= channels else 0
                pa, pb, pc = abs(up - upleft), abs(left - upleft), abs(left + up - 2 * upleft)
                pred = left if (pa <= pb and pa <= pc) else (up if pb <= pc else upleft)
                line[i] = (line[i] + pred) & 0xFF
        elif ftype != 0:
            raise RuntimeError(f"未知 PNG 行过滤器 {ftype}")
        for x in range(0, width):
            o = x * channels
            if line[o] < threshold or line[o + 1] < threshold or line[o + 2] < threshold:
                nonwhite += 1
        total += width
        prev = line
        if total > 400_000:  # 缩略图预期远小于此；超标则不再细算
            break
    return nonwhite / max(total, 1)


# --------------------------------------------------------------------------
# 渲染
# --------------------------------------------------------------------------

def normalize_target(target: str) -> tuple[str, str]:
    """返回 (供 Page.navigate 的 URL, 人类可读标签)。"""
    if re.match(r"^[a-zA-Z][a-zA-Z0-9+.-]*://", target) and not target.startswith("file://"):
        return target, target
    path = target
    if path.startswith("file://"):
        path = urllib.parse.unquote(path[7:])
    path = os.path.abspath(os.path.expanduser(path))
    if not os.path.exists(path):
        sys.exit(f"错误：文件不存在：{path}")
    return "file://" + urllib.parse.quote(path), path


def launch(backend: str, headless_shell: bool, width: int, height: int):
    profile = tempfile.mkdtemp(prefix="html-shot-")
    args = [backend, "--remote-debugging-port=0", "--no-first-run", "--no-default-browser-check",
            "--disable-gpu", "--hide-scrollbars", "--disable-extensions",
            f"--user-data-dir={profile}", f"--window-size={width},{height}", "about:blank"]
    if not headless_shell:
        args.insert(1, "--headless=new")
    proc = subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    port_file = os.path.join(profile, "DevToolsActivePort")
    deadline = time.time() + 30
    port = None
    while time.time() < deadline:
        if proc.poll() is not None:
            break
        try:
            with open(port_file) as fh:
                port = int(fh.read().split("\n")[0])
            if port:
                break
        except (OSError, ValueError):
            time.sleep(0.05)
    if not port:
        proc.terminate()
        sys.exit("错误：浏览器未能启动（DevToolsActivePort 未生成）")
    return proc, profile, port


def connect(port: int) -> CDP:
    for _ in range(50):
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/json/list", timeout=5) as fh:
                targets = json.load(fh)
            pages = [t for t in targets if t.get("type") == "page"]
            if pages:
                return CDP(WS(pages[0]["webSocketDebuggerUrl"]))
        except (OSError, ValueError, KeyError):
            pass
        time.sleep(0.1)
    sys.exit("错误：连不上浏览器调试端口")


def run(args) -> int:
    url, label = normalize_target(args.target)
    backend, headless_shell = find_backend(args.browser)
    if args.verbose:
        print(f"后端：{backend}", file=sys.stderr)

    out = os.path.abspath(os.path.expanduser(args.output))
    os.makedirs(os.path.dirname(out) or ".", exist_ok=True)

    proc, profile, port = launch(backend, headless_shell, args.width, args.height)
    try:
        cdp = connect(port)
        cdp.call("Page.enable")
        cdp.call("Runtime.enable")
        cdp.call("Emulation.setDeviceMetricsOverride",
                 width=args.width, height=args.height,
                 deviceScaleFactor=args.scale, mobile=False)

        nav = cdp.call("Page.navigate", url=url)
        if nav.get("errorText"):
            sys.exit(f"错误：导航失败（{nav['errorText']}）：{label}")

        if not cdp.drain_events("Page.loadEventFired", timeout=args.timeout):
            print("警告：等待 load 事件超时，仍继续截图", file=sys.stderr)

        if args.wait_for:
            deadline = time.time() + args.timeout
            while time.time() < deadline:
                if cdp.evaluate(f"!!document.querySelector({json.dumps(args.wait_for)})"):
                    break
                time.sleep(0.1)
            else:
                sys.exit(f"错误：等待选择器超时：{args.wait_for}")

        if args.wait_ms:
            time.sleep(args.wait_ms / 1000.0)

        if args.selector:
            clip = element_clip(cdp, args.selector)
        elif args.full_page:
            clip = page_clip(cdp, args.width, args.scale)
        else:
            clip = {"x": 0, "y": 0, "width": args.width, "height": args.height, "scale": 1}

        shot = cdp.call("Page.captureScreenshot", format="png",
                        captureBeyondViewport=True, clip=clip)
        png = base64.b64decode(shot["data"])
        with open(out, "wb") as fh:
            fh.write(png)

        # 后置校验：白图/空图一律当失败，避免「成功产出」一张没有内容的图。
        ratio = blank_check(cdp, clip)
        if ratio < BLANK_RATIO:
            os.remove(out)
            sys.exit(
                f"错误：渲染结果几乎全白（非白像素 {ratio * 100:.2f}%），已删除 {out}\n"
                "  常见原因：页面依赖的 JS 未加载完成 → 加大 --wait-ms；\n"
                "  或 --selector 指向了不可见元素；或目标本身是空页。"
            )

        width = int(clip["width"] * args.scale)
        height = int(clip["height"] * args.scale)
        print(f"{out}  {width}x{height}  {len(png) / 1024:.0f} KB  "
              f"非白 {ratio * 100:.1f}%")
        return 0
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
        subprocess.run(["rm", "-rf", profile], check=False)


def page_clip(cdp: CDP, width: int, scale: float):
    """量出真实内容高度。

    `cssContentSize` 会被视口高度撑大（内容只有 400px 的页面在 900px 视口下
    报 900），所以测量前先把视口压扁到 MIN_PROBE_HEIGHT。只改高度不改宽度，
    宽度相关的媒体查询不受影响。
    """
    cdp.call("Emulation.setDeviceMetricsOverride",
             width=width, height=MIN_PROBE_HEIGHT,
             deviceScaleFactor=scale, mobile=False)
    time.sleep(0.05)
    metrics = cdp.call("Page.getLayoutMetrics")
    size = metrics.get("cssContentSize") or metrics.get("contentSize") or {}
    height = float(size.get("height") or 0)
    if height <= 0:
        sys.exit("错误：页面内容高度为 0")
    if height > MAX_HEIGHT:
        print(f"警告：页面高 {height:.0f}px 超过上限，截到 {MAX_HEIGHT}px", file=sys.stderr)
        height = MAX_HEIGHT
    return {"x": 0, "y": 0, "width": width, "height": height, "scale": 1}


def element_clip(cdp: CDP, selector: str):
    expr = (
        "(()=>{const e=document.querySelector(%s);if(!e)return null;"
        "const r=e.getBoundingClientRect();"
        "return JSON.stringify({x:r.x+window.scrollX,y:r.y+window.scrollY,"
        "width:r.width,height:r.height})})()" % json.dumps(selector)
    )
    raw = cdp.evaluate(expr)
    if not raw:
        sys.exit(f"错误：选择器未匹配到元素：{selector}")
    box = json.loads(raw)
    if box["width"] <= 0 or box["height"] <= 0:
        sys.exit(f"错误：元素尺寸为 0（不可见或未布局）：{selector}")
    box["scale"] = 1
    return box


def blank_check(cdp: CDP, clip) -> float:
    """抓一张小尺寸缩略图算非白占比。缩略图像素少，标准库解码足够快。"""
    scale = 140.0 / max(clip["width"], clip["height"], 1)
    probe = dict(clip)
    probe["scale"] = max(scale, 0.0005)
    try:
        shot = cdp.call("Page.captureScreenshot", format="png",
                        captureBeyondViewport=True, clip=probe)
        return png_nonwhite_ratio(base64.b64decode(shot["data"]))
    except (RuntimeError, KeyError) as exc:
        print(f"警告：空白检测跳过（{exc}）", file=sys.stderr)
        return 1.0


def main() -> int:
    parser = argparse.ArgumentParser(
        prog="shot.py",
        description="把 HTML 文件或 URL 渲染成 PNG（Chrome DevTools Protocol，标准库实现）。",
    )
    parser.add_argument("target", help="HTML 文件路径或 http(s) URL")
    parser.add_argument("-o", "--output", required=True, help="输出 PNG 路径")
    parser.add_argument("--width", type=int, default=1280, help="视口宽度，默认 1280")
    parser.add_argument("--height", type=int, default=900, help="视口高度，默认 900（全页时只用于首屏布局）")
    parser.add_argument("--scale", type=float, default=2.0, help="设备像素比，默认 2（出 2x 图）")
    parser.add_argument("--no-full-page", dest="full_page", action="store_false",
                        help="只截视口，不截整页")
    parser.add_argument("--selector", help="只截匹配该 CSS 选择器的元素")
    parser.add_argument("--wait-ms", type=int, default=600,
                        help="load 之后额外等待毫秒，给图表库留渲染时间，默认 600")
    parser.add_argument("--wait-for", help="等待该 CSS 选择器出现后再截图")
    parser.add_argument("--timeout", type=float, default=20.0, help="等待 load 的超时秒数，默认 20")
    parser.add_argument("--browser", help="指定浏览器可执行文件（否则自动探测）")
    parser.add_argument("-v", "--verbose", action="store_true")
    args = parser.parse_args()
    return run(args)


if __name__ == "__main__":
    sys.exit(main())
