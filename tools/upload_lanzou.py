#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""蓝奏云自动上传（网页接口，需登录 Cookie）

用法:
    python tools/upload_lanzou.py <文件路径> [--folder <folder_id>] [--json]

说明:
    蓝奏云没有官方开放 API，本脚本调用其网页端上传接口（up.woozooo.com）。
    认证依靠登录后的 Cookie（phpdisk_info），Cookie 过期后需重新获取。

首次使用:
    1) 复制 tools/lanzou_config.example.json 为 tools/lanzou_config.json
    2) 浏览器登录蓝奏云网页版 → F12 → Network → 任意请求 → 复制完整 Cookie
    3) 把 cookie 填进配置；folder_id 填目标文件夹 ID（根目录用 -1）

退出码: 0 成功 / 2 配置缺失 / 3 登录失效 / 4 上传失败
"""

import argparse
import json
import sys
import time
from pathlib import Path

try:
    import requests
except ImportError:
    print("[错误] 缺少 requests，请先执行: pip install requests", file=sys.stderr)
    sys.exit(2)

ROOT = Path(__file__).resolve().parent
CONFIG_PATH = ROOT / "lanzou_config.json"

UA = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36"
)
UPLOAD_URL = "https://up.woozooo.com/html5up.php"
LIST_URL = "https://up.woozooo.com/doupload.php"
REFERER = "https://pc.woozooo.com/mydisk.php"

# 蓝奏云是国内站点，必须直连；一旦走系统代理（如 v2ray 的 10808）会被重置连接。
S = requests.Session()
S.trust_env = False


def _stdout_utf8() -> None:
    try:
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
        sys.stderr.reconfigure(encoding="utf-8", errors="replace")
    except Exception:
        pass


def load_config() -> dict:
    if not CONFIG_PATH.exists():
        print(
            f"[错误] 找不到配置文件: {CONFIG_PATH}\n"
            "       请先复制 tools/lanzou_config.example.json 为 lanzou_config.json 并填入 cookie。",
            file=sys.stderr,
        )
        sys.exit(2)
    try:
        cfg = json.loads(CONFIG_PATH.read_text(encoding="utf-8"))
    except Exception as e:
        print(f"[错误] 配置文件不是合法 JSON: {e}", file=sys.stderr)
        sys.exit(2)

    cookie = (cfg.get("cookie") or "").strip()
    if cookie.lower().startswith("cookie:"):
        cookie = cookie[7:].strip()
    if not cookie:
        print("[错误] 配置里的 cookie 为空。", file=sys.stderr)
        sys.exit(2)

    cfg["cookie"] = cookie
    cfg.setdefault("folder_id", "-1")
    return cfg


def headers(cookie: str) -> dict:
    return {
        "User-Agent": UA,
        "Referer": REFERER,
        "Origin": "https://pc.woozooo.com",
        "Cookie": cookie,
    }


def upload(path: Path, folder_id: str, cookie: str, timeout: int = 300) -> dict:
    """走 html5up.php 上传。字段顺序按网页端原生顺序，file 放最后。"""
    size = path.stat().st_size
    mtime = time.strftime("%a %b %d %Y %H:%M:%S GMT+0800 (中国标准时间)", time.localtime())

    # 用 files 列表承载文本字段，可保证发送顺序，且 file 一定在最后
    parts = [
        ("task", (None, "1")),
        ("folder_id", (None, str(folder_id))),
        ("id", (None, "WU_FILE_0")),
        ("name", (None, path.name)),
        ("type", (None, "application/octet-stream")),
        ("lastModifiedDate", (None, mtime)),
        ("size", (None, str(size))),
    ]
    fh = open(path, "rb")
    # 注意：蓝奏网页端 html5up.php 的文件字段名是 upload_file（不是 file），
    # 写成 file 会被服务端判为「请选择需要上传的文件」。
    parts.append(("upload_file", (path.name, fh, "application/octet-stream")))
    try:
        resp = S.post(
            UPLOAD_URL, files=parts, headers=headers(cookie), timeout=timeout
        )
    finally:
        fh.close()

    try:
        data = resp.json()
    except Exception:
        print(f"[错误] 上传返回非 JSON（HTTP {resp.status_code}）：{resp.text[:300]}", file=sys.stderr)
        sys.exit(4)

    zt = data.get("zt")
    if str(zt) == "9":
        print("[错误] 登录已失效（zt=9 login not），请重新获取 Cookie 填入配置。", file=sys.stderr)
        sys.exit(3)
    if str(zt) not in ("1", "2"):
        print(f"[错误] 上传失败：{json.dumps(data, ensure_ascii=False)[:400]}", file=sys.stderr)
        sys.exit(4)
    return data


def extract_file(data: dict) -> dict:
    """从上传响应里取出文件 id / 官方域名 / 分享链接（各版本字段名不一，做兼容）。"""
    item = data.get("text")
    if isinstance(item, list) and item:
        item = item[0]
    if not isinstance(item, dict):
        info = data.get("info")
        item = info if isinstance(info, dict) else {}

    file_id = str(item.get("id") or item.get("file_id") or "")
    # 分享链接用的是 f_id（10 位分享码），不是数字 id
    share_code = str(item.get("f_id") or "")
    domain = item.get("is_newd") or item.get("dom") or ""
    name = item.get("name_all") or item.get("name") or ""
    share = f"{domain.rstrip('/')}/{share_code}" if domain and share_code else ""
    return {"id": file_id, "f_id": share_code, "domain": domain, "name": name, "share_url": share}


def fill_share_url(cookie: str, folder_id: str, file_name: str, cur: dict) -> dict:
    """上传响应没带域名时，回查文件夹列表补上分享链接。"""
    if cur.get("share_url"):
        return cur
    try:
        resp = S.post(
            LIST_URL,
            data={"task": "5", "folder_id": str(folder_id), "pg": "1"},
            headers=headers(cookie),
            timeout=60,
        )
        data = resp.json()
    except Exception:
        return cur
    if str(data.get("zt")) == "9":
        return cur
    items = data.get("text") or []
    if not isinstance(items, list):
        return cur
    for it in items:
        if not isinstance(it, dict):
            continue
        if it.get("name_all") == file_name:
            fid = str(it.get("id") or "")
            code = str(it.get("f_id") or "")
            dom = it.get("is_newd") or ""
            if code and dom:
                cur["id"] = cur.get("id") or fid
                cur["f_id"] = cur.get("f_id") or code
                cur["domain"] = cur.get("domain") or dom
                cur["share_url"] = f"{dom.rstrip('/')}/{code}"
            break
    return cur


def list_files(cookie: str, folder_id: str) -> list:
    """列出文件夹内所有文件（多页拉取）。

    接口实测：task=5 + folder_id + pg 才是「列文件」；
    task=47 列的是子文件夹，task=3 是删除文件夹（务必别混用）。
    """
    out = []
    for pg in range(1, 11):  # 最多 10 页，够用
        try:
            resp = S.post(
                LIST_URL,
                data={"task": "5", "folder_id": str(folder_id), "pg": str(pg)},
                headers=headers(cookie),
                timeout=60,
            )
            data = resp.json()
        except Exception:
            break
        if str(data.get("zt")) == "9":
            print("[错误] 列目录时登录失效（zt=9），请重新获取 Cookie。", file=sys.stderr)
            sys.exit(3)
        items = data.get("text")
        if not isinstance(items, list) or not items:
            break
        for it in items:
            if not isinstance(it, dict):
                continue
            fid = it.get("id") or it.get("file_id")
            name = it.get("name_all") or it.get("name") or ""
            if fid:
                out.append(
                    {
                        "id": str(fid),
                        "name": name,
                        "size": it.get("size") or "",
                        "time": it.get("time") or "",
                    }
                )
        if len(items) < 20:  # 不满一页说明到底了
            break
    return out


def delete_file(cookie: str, file_id: str) -> tuple:
    """删除单个文件。蓝奏网页端删除走 doupload.php 的 task=6。"""
    try:
        resp = S.post(
            LIST_URL,
            data={"task": "6", "file_id": str(file_id)},
            headers=headers(cookie),
            timeout=60,
        )
        data = resp.json()
    except Exception as e:
        return False, f"请求异常: {e}"
    zt = str(data.get("zt"))
    if zt == "9":
        return False, "login not"
    if zt == "1":
        return True, "ok"
    return False, json.dumps(data, ensure_ascii=False)[:200]


def clean_old(cookie: str, folder_id: str, file_name: str) -> None:
    """删除同目录下所有同名旧文件，避免每次发布堆积、分享链接混乱。"""
    files = list_files(cookie, folder_id)
    targets = [f for f in files if f["name"] == file_name]
    if not targets:
        print("[信息] 目录内没有同名旧文件，直接上传。")
        return
    print(f"[信息] 发现 {len(targets)} 个同名旧文件，先删除：")
    failed = 0
    for f in targets:
        ok, msg = delete_file(cookie, f["id"])
        if ok:
            print(f"       已删除 {f['name']}（id={f['id']}）")
        else:
            failed += 1
            print(f"       [警告] 删除失败 id={f['id']}：{msg}", file=sys.stderr)

    # 复核：重新列一次，确认确实没了
    remain = [f for f in list_files(cookie, folder_id) if f["name"] == file_name]
    if remain:
        print(
            f"[警告] 仍有 {len(remain)} 个同名文件未删除（删除接口可能已变更）。"
            "上传会继续，但目录里会有重复文件。",
            file=sys.stderr,
        )
    elif not failed:
        print("[信息] 旧文件已清空。")


def main() -> int:
    _stdout_utf8()
    ap = argparse.ArgumentParser(description="上传文件到蓝奏云")
    ap.add_argument("file", help="要上传的本地文件路径")
    ap.add_argument("--folder", default=None, help="目标文件夹 ID（默认取配置里的 folder_id）")
    ap.add_argument("--json", action="store_true", help="只输出 JSON 结果，便于其他脚本调用")
    ap.add_argument("--keep-old", action="store_true", help="不删除同名旧文件，直接上传")
    args = ap.parse_args()

    path = Path(args.file).expanduser().resolve()
    if not path.is_file():
        print(f"[错误] 文件不存在: {path}", file=sys.stderr)
        return 2

    cfg = load_config()
    folder_id = str(args.folder or cfg["folder_id"])

    if not args.json:
        print(f"[信息] 目标目录 {folder_id}，文件 {path.name}（{path.stat().st_size} 字节）")

    if not args.keep_old:
        clean_old(cfg["cookie"], folder_id, path.name)
    elif not args.json:
        print("[信息] 已指定 --keep-old，跳过删除旧文件。")

    if not args.json:
        print("[信息] 开始上传 …")

    data = upload(path, folder_id, cfg["cookie"])
    result = extract_file(data)
    result = fill_share_url(cfg["cookie"], folder_id, path.name, result)
    result["ok"] = True
    result["local_file"] = str(path)
    result["size"] = path.stat().st_size
    result["uploaded_at"] = time.strftime("%Y-%m-%d %H:%M:%S")
    result["raw"] = data if not args.json else None

    if args.json:
        print(json.dumps({k: v for k, v in result.items() if k != "raw"}, ensure_ascii=False))
    else:
        print("[成功] 已上传到蓝奏云")
        if result.get("id"):
            print(f"       文件 ID : {result['id']}")
        if result.get("share_url"):
            print(f"       分享链接: {result['share_url']}")
        else:
            print("       （分享链接未取到，请到网盘页面确认）")
        print(f"       原始响应: {json.dumps(data, ensure_ascii=False)[:300]}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
