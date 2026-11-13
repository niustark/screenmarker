#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""自动获取蓝奏云 Cookie 并写入配置

原理:
    启动一个独立的浏览器实例（独立 user-data-dir + 调试端口），打开蓝奏云。
    你在窗口里正常登录（账号密码/扫码都行，验证码也由你手动过），
    登录完成后回到命令行按回车，脚本通过 Chrome DevTools 协议读出 Cookie，
    自动写入 tools/lanzou_config.json 并验证一次。

优点:
    - 不用你翻 F12 找 Cookie
    - 独立配置目录，不影响你日常浏览器；登录态会保留，下次再跑通常免登录

用法:
    python tools/get_lanzou_cookie.py                 # 完整流程
    python tools/get_lanzou_cookie.py --folder-name ScreenMarker   # 顺便自动匹配目录 ID
    python tools/get_lanzou_cookie.py --no-browser    # 不启浏览器，仅用已开的调试端口
"""

import argparse
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import time
from pathlib import Path

try:
    import requests
    import websocket  # websocket-client
except ImportError as e:
    print(f"[错误] 缺少依赖: {e}\n       请执行: pip install requests websocket-client", file=sys.stderr)
    sys.exit(2)

ROOT = Path(__file__).resolve().parent
CONFIG_PATH = ROOT / "lanzou_config.json"
PROFILE_DIR = ROOT / ".lanzou_profile"

# 蓝奏云与本地调试端口都应直连；系统代理（如 v2ray 10808）会重置国内站点连接
for _k in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"):
    os.environ.pop(_k, None)

UA = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36"
)
START_URL = "https://up.woozooo.com/u"

BROWSERS = [
    r"C:\Program Files\Google\Chrome\Application\chrome.exe",
    r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
    r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
    r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
]

# 蓝奏云需要的 Cookie 名，按重要性排序（phpdisk_info 是核心凭据）
PRIORITY = ["phpdisk_info", "ylogin", "phpdisk_zcore_info", "folder_id_c", "osd"]


def _utf8() -> None:
    try:
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
        sys.stderr.reconfigure(encoding="utf-8", errors="replace")
    except Exception:
        pass


def find_browser() -> str:
    for p in BROWSERS:
        if Path(p).is_file():
            return p
    print("[错误] 没找到 Edge 或 Chrome，请手动粘贴 Cookie 到配置里。", file=sys.stderr)
    sys.exit(2)


def port_free(port: int) -> bool:
    s = socket.socket()
    try:
        s.bind(("127.0.0.1", port))
        return True
    except OSError:
        return False
    finally:
        s.close()


def pick_port(preferred: int = 9333) -> int:
    for p in range(preferred, preferred + 30):
        if port_free(p):
            return p
    raise RuntimeError("找不到空闲调试端口")


def launch(browser: str, port: int) -> subprocess.Popen:
    PROFILE_DIR.mkdir(parents=True, exist_ok=True)
    args = [
        browser,
        f"--remote-debugging-port={port}",
        f"--user-data-dir={PROFILE_DIR}",
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-background-networking",
        # 屏蔽插件与首次运行页，避免扩展欢迎页抢焦点、干扰调试
        "--disable-extensions",
        "--disable-component-extensions-with-background-pages",
        START_URL,
    ]
    return subprocess.Popen(args)


def wait_debug(port: int, timeout: float = 40.0) -> dict:
    """等调试端口起来，返回 /json/version 内容。"""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            r = requests.get(f"http://127.0.0.1:{port}/json/version", timeout=2)
            if r.status_code == 200:
                return r.json()
        except Exception:
            pass
        time.sleep(0.6)
    raise RuntimeError("调试端口未就绪（浏览器可能启动失败）")


def cdp_cookies(ws_url: str, timeout: float = 20.0) -> list:
    """通过 CDP 的 Storage.getCookies 读取该浏览器实例的全部 Cookie。"""
    ws = websocket.create_connection(ws_url, timeout=timeout)
    try:
        ws.send(json.dumps({"id": 1, "method": "Storage.getCookies", "params": {}}))
        deadline = time.time() + timeout
        while time.time() < deadline:
            msg = json.loads(ws.recv())
            if msg.get("id") == 1:
                return msg.get("result", {}).get("cookies", [])
        raise RuntimeError("等待 Cookie 响应超时")
    finally:
        try:
            ws.close()
        except Exception:
            pass


def filter_cookie(cookies: list) -> str:
    picked = {}
    for c in cookies:
        domain = (c.get("domain") or "").lower()
        name = c.get("name") or ""
        if "woozooo" not in domain and "lanzou" not in domain:
            continue
        if name not in picked or c.get("expires", 0) > 0:
            picked[name] = c.get("value", "")

    if "phpdisk_info" not in picked:
        return ""

    order = [n for n in PRIORITY if n in picked] + [n for n in picked if n not in PRIORITY]
    return "; ".join(f"{n}={picked[n]}" for n in order)


def verify(cookie: str) -> tuple:
    """用 Cookie 打一次接口，确认登录有效。"""
    try:
        r = requests.post(
            "https://up.woozooo.com/doupload.php",
            data={"task": "47", "folder_id": "-1", "pg": "1"},
            headers={"User-Agent": UA, "Referer": START_URL, "Cookie": cookie},
            timeout=30,
        )
        data = r.json()
    except Exception as e:
        return False, f"请求失败: {e}"
    if str(data.get("zt")) == "9":
        return False, "接口判定未登录（Cookie 无效或已过期）"
    return True, "登录态有效"


def detect_folders(cookie: str) -> list:
    """尽力列出文件夹（名称 + id），失败返回空列表。"""
    hdr = {"User-Agent": UA, "Referer": START_URL, "Cookie": cookie}
    found = []

    # 1) 接口：task=19 直接返回全部文件夹（最可靠）
    for host in ("https://up.woozooo.com", "https://pc.woozooo.com"):
        try:
            r = requests.post(
                host + "/doupload.php",
                data={"task": "19", "folder_id": "-1"},
                headers=hdr,
                timeout=25,
            )
            data = r.json()
        except Exception:
            continue
        items = data.get("info")
        if isinstance(items, list):
            for it in items:
                if isinstance(it, dict):
                    fid = it.get("folder_id")
                    name = it.get("folder_name")
                    if fid and name:
                        found.append((str(fid), str(name)))

    # 2) 接口：task=47 列根目录（返回里的 fol_id 即文件夹 id）
    for host in ("https://up.woozooo.com", "https://pc.woozooo.com"):
        try:
            r = requests.post(
                host + "/doupload.php",
                data={"task": "47", "folder_id": "-1", "pg": "1"},
                headers=hdr,
                timeout=25,
            )
            data = r.json()
        except Exception:
            continue
        for key in ("text", "folders"):
            items = data.get(key)
            if isinstance(items, list):
                for it in items:
                    if isinstance(it, dict):
                        fid = it.get("folder_id") or it.get("fol_id")
                        name = it.get("folder_name") or it.get("name_all") or it.get("name")
                        if fid and name and str(fid) != "-1":
                            found.append((str(fid), str(name)))

    # 2) 老版网页：链接里带 folder_id
    try:
        r = requests.get(
            "https://pc.woozooo.com/mydisk.php?item=files&action=index&folder_id=-1",
            headers=hdr,
            timeout=25,
        )
        for m in re.finditer(r"folder_id=(\d+)", r.text):
            fid = m.group(1)
            seg = r.text[max(0, m.start() - 500) : m.end() + 500]
            nm = re.search(r">([^<>{}]{1,40})</a>", seg)
            if nm:
                found.append((fid, nm.group(1).strip()))
    except Exception:
        pass

    seen, uniq = set(), []
    for fid, name in found:
        key = (fid, name)
        if key in seen:
            continue
        seen.add(key)
        uniq.append((fid, name))
    return uniq


def save_config(cookie: str, folder_id: str = None) -> None:
    cfg = {"cookie": "", "folder_id": "-1"}
    if CONFIG_PATH.exists():
        try:
            cfg.update(json.loads(CONFIG_PATH.read_text(encoding="utf-8")))
        except Exception:
            pass
    cfg["cookie"] = cookie
    if folder_id:
        cfg["folder_id"] = str(folder_id)
    CONFIG_PATH.write_text(json.dumps(cfg, ensure_ascii=False, indent=2), encoding="utf-8")


def kill_tree(proc: subprocess.Popen) -> None:
    try:
        subprocess.run(
            ["taskkill", "/PID", str(proc.pid), "/T", "/F"],
            capture_output=True,
            timeout=15,
        )
    except Exception:
        try:
            proc.terminate()
        except Exception:
            pass


def obtain_cookie(ws_url: str, auto_wait: int) -> str:
    """获取 Cookie。auto_wait>0 时轮询等待登录，否则等用户按回车。"""
    if auto_wait > 0:
        print(f" 请在弹出的窗口里登录，脚本会自动检测（最多等 {auto_wait} 秒）…")
        deadline = time.time() + auto_wait
        last_report = 0
        while time.time() < deadline:
            try:
                c = filter_cookie(cdp_cookies(ws_url, timeout=15))
            except Exception:
                c = ""
            if c:
                ok, _ = verify(c)
                if ok:
                    print("[信息] 检测到登录态")
                    return c
            waited = int(time.time() - (deadline - auto_wait))
            if waited - last_report >= 15:
                print(f"       已等待 {waited}s … 请在浏览器里完成登录")
                last_report = waited
            time.sleep(3)
        print("[错误] 等待超时，仍未检测到登录态。", file=sys.stderr)
        return ""

    try:
        input(" 登录完成后按回车 … ")
    except (EOFError, KeyboardInterrupt):
        print("\n[信息] 已取消")
        return ""
    try:
        cookies = cdp_cookies(ws_url)
    except Exception as e:
        print(f"[错误] 读取 Cookie 失败: {e}", file=sys.stderr)
        return ""
    return filter_cookie(cookies)


def main() -> int:
    _utf8()
    ap = argparse.ArgumentParser(description="自动获取蓝奏云 Cookie")
    ap.add_argument("--folder-name", default=None, help="自动匹配该名称的文件夹，写入 folder_id")
    ap.add_argument("--port", type=int, default=9333, help="调试端口（默认 9333）")
    ap.add_argument("--no-browser", action="store_true", help="不启动浏览器，直接读已存在的调试端口")
    ap.add_argument("--keep-open", action="store_true", help="完成后不关闭浏览器")
    ap.add_argument(
        "--auto-wait",
        type=int,
        default=0,
        help="自动轮询等待登录的秒数（0=等按回车，适合脚本化调用）",
    )
    args = ap.parse_args()

    proc = None
    port = args.port
    if not args.no_browser:
        browser = find_browser()
        port = pick_port(args.port)
        print(f"[信息] 启动浏览器: {Path(browser).name}")
        print(f"[信息] 独立配置目录: {PROFILE_DIR}")
        proc = launch(browser, port)

    print("[信息] 等待调试端口就绪 …")
    try:
        info = wait_debug(port)
    except Exception as e:
        print(f"[错误] {e}", file=sys.stderr)
        if proc:
            kill_tree(proc)
        return 4

    ws_url = info.get("webSocketDebuggerUrl")
    if not ws_url:
        print("[错误] 没拿到调试连接地址。", file=sys.stderr)
        if proc:
            kill_tree(proc)
        return 4

    print()
    print("=" * 62)
    print(" 请在弹出的浏览器窗口里登录蓝奏云（扫码/账号密码都行）")
    print(" 登录后能看到「我的文件」列表即可")
    print("=" * 62)

    cookie = obtain_cookie(ws_url, args.auto_wait)
    if not cookie:
        if proc and not args.keep_open:
            kill_tree(proc)
        return 3

    print(f"[信息] 读到 Cookie，长度 {len(cookie)} 字符")
    ok, msg = verify(cookie)
    print(f"[信息] 校验: {msg}")
    if not ok:
        print("[错误] Cookie 无效，未写入配置。", file=sys.stderr)
        if proc and not args.keep_open:
            kill_tree(proc)
        return 3

    folders = detect_folders(cookie)
    matched = None
    if args.folder_name:
        for fid, name in folders:
            if name.strip() == args.folder_name:
                matched = fid
                break
        if matched:
            print(f"[信息] 匹配到文件夹「{args.folder_name}」→ folder_id={matched}")

    save_config(cookie, matched)
    print(f"[成功] 已写入 {CONFIG_PATH}")

    if folders:
        print("\n[信息] 探测到的文件夹：")
        for fid, name in folders:
            mark = " ←" if matched and fid == matched else ""
            print(f"       folder_id={fid}  {name}{mark}")
    elif not matched:
        print("[提示] 没探测到文件夹列表，folder_id 保持原值；可用 tools/lanzou_folders.py 再查。")

    if proc and not args.keep_open:
        kill_tree(proc)
        print("[信息] 浏览器已关闭")
    return 0


if __name__ == "__main__":
    sys.exit(main())
