#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""列出蓝奏云文件夹及其 folder_id（诊断辅助工具）

新版界面 https://up.woozooo.com/u 不把 folder_id 暴露在地址栏，
本脚本用配置里的 Cookie 查询接口 / 抓页面，把能找到的 folder_id 全列出来。

用法:
    python tools/lanzou_folders.py

配置复用 tools/lanzou_config.json 里的 cookie。
"""

import json
import re
import sys
from pathlib import Path

try:
    import requests
except ImportError:
    print("[错误] 缺少 requests: pip install requests", file=sys.stderr)
    sys.exit(2)

ROOT = Path(__file__).resolve().parent
CONFIG_PATH = ROOT / "lanzou_config.json"

UA = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36"
)


def _utf8() -> None:
    try:
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
        sys.stderr.reconfigure(encoding="utf-8", errors="replace")
    except Exception:
        pass


def load_cookie() -> str:
    if not CONFIG_PATH.exists():
        print(f"[错误] 缺少配置: {CONFIG_PATH}", file=sys.stderr)
        sys.exit(2)
    cfg = json.loads(CONFIG_PATH.read_text(encoding="utf-8"))
    cookie = (cfg.get("cookie") or "").strip()
    if cookie.lower().startswith("cookie:"):
        cookie = cookie[7:].strip()
    if not cookie:
        print("[错误] 配置里的 cookie 为空，请先填入。", file=sys.stderr)
        sys.exit(2)
    return cookie


def headers(cookie: str, referer: str = "https://up.woozooo.com/u") -> dict:
    return {"User-Agent": UA, "Referer": referer, "Cookie": cookie}


def try_api(cookie: str) -> list:
    """task=47 列目录；返回里可能同时含文件与子文件夹。"""
    found = []
    for host in ("https://up.woozooo.com", "https://pc.woozooo.com"):
        url = host + "/doupload.php"
        try:
            r = requests.post(
                url,
                data={"task": "47", "folder_id": "-1", "pg": "1"},
                headers=headers(cookie),
                timeout=30,
            )
        except Exception as e:
            print(f"[调试] {host} 请求失败: {e}", file=sys.stderr)
            continue
        body = r.text.strip()
        print(f"[调试] {host} HTTP {r.status_code}，返回前 200 字符：\n{body[:200]}\n")
        try:
            data = r.json()
        except Exception:
            continue
        if str(data.get("zt")) == "9":
            print(f"[提示] {host} 判定未登录（Cookie 可能只对 up.woozooo.com 有效）。", file=sys.stderr)
            continue
        items = data.get("text")
        if isinstance(items, list):
            for it in items:
                if not isinstance(it, dict):
                    continue
                # 文件夹项一般带 folder_id / folder_name 字段
                fid = it.get("folder_id") or it.get("fol_id")
                fname = it.get("folder_name") or it.get("name")
                if fid and fname:
                    found.append((str(fid), str(fname), host))
        # 有些版本把文件夹放在单独键里
        for key in ("folders", "folder", "info"):
            v = data.get(key)
            if isinstance(v, list):
                for it in v:
                    if isinstance(it, dict):
                        fid = it.get("folder_id") or it.get("id")
                        fname = it.get("folder_name") or it.get("name")
                        if fid and fname:
                            found.append((str(fid), str(fname), host))
    return found


def try_html(cookie: str) -> list:
    """老版网页把文件夹渲染成 folder_id=NNN 的链接，直接正则抓。"""
    found = []
    pages = [
        "https://pc.woozooo.com/mydisk.php?item=files&action=index&folder_id=-1",
        "https://up.woozooo.com/u",
    ]
    for url in pages:
        try:
            r = requests.get(url, headers=headers(cookie), timeout=30)
        except Exception as e:
            print(f"[调试] GET {url} 失败: {e}", file=sys.stderr)
            continue
        html = r.text
        print(f"[调试] GET {url} → HTTP {r.status_code}，{len(html)} 字节", file=sys.stderr)
        if r.status_code != 200 or len(html) < 200:
            continue
        # 抓 folder_id=123 及其附近可能的名字
        for m in re.finditer(r"folder_id=(\d+)", html):
            fid = m.group(1)
            seg = html[max(0, m.start() - 400) : m.end() + 400]
            name_m = re.search(r">([^<>{}\[\]]{1,40})</a>", seg)
            name = name_m.group(1).strip() if name_m else ""
            found.append((fid, name, url.split("?")[0]))
        # 新版界面把数据塞在 JS 里，尝试抓 folder_id: "123" 形式
        for m in re.finditer(r'folder_id["\']?\s*[:=]\s*["\']?(\d+)', html):
            found.append((m.group(1), "(JS 内联)", url))
    return found


def main() -> int:
    _utf8()
    cookie = load_cookie()
    print("=== 1) 接口查询 ===")
    found = try_api(cookie)
    print("=== 2) 页面抓取 ===")
    found += try_html(cookie)

    # 去重
    seen, uniq = set(), []
    for fid, name, src in found:
        if fid in seen:
            continue
        seen.add(fid)
        uniq.append((fid, name, src))

    print("\n=== 结果 ===")
    if not uniq:
        print("没找到 folder_id。可能原因：Cookie 无效 / 接口字段变了。")
        print("兜底办法：网页里进入「ScreenMarker」文件夹，F12 → Network，")
        print("找到 doupload.php 或 list 类请求，看它的 Form Data 里的 folder_id。")
        return 4
    for fid, name, src in uniq:
        print(f"  folder_id={fid}   名称: {name or '(未识别)'}   来源: {src}")
    print("\n把目标文件夹对应的 folder_id 填进 tools/lanzou_config.json 即可。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
