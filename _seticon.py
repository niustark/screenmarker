"""把 app.ico 写进 exe 的资源段（RT_GROUP_ICON + RT_ICON）。

本机工具链没有 windres/rc.exe，所以不走 .rc + build.rs，
改用 UpdateResource 系列 API 直接修改已编译好的 exe —— 这是系统官方支持的
「给现成 PE 加资源」的方式，不需要任何编译器。

用法: python _seticon.py [exe路径] [ico路径]
"""
import ctypes, sys, struct
from ctypes import wintypes

exe = sys.argv[1] if len(sys.argv) > 1 else "target/release/screenmarker.exe"
ico = sys.argv[2] if len(sys.argv) > 2 else "app.ico"

k32 = ctypes.windll.kernel32
k32.BeginUpdateResourceW.restype = wintypes.HANDLE
k32.BeginUpdateResourceW.argtypes = [wintypes.LPCWSTR, wintypes.BOOL]
k32.UpdateResourceW.argtypes = [wintypes.HANDLE, ctypes.c_void_p, ctypes.c_void_p,
                                wintypes.WORD, ctypes.c_void_p, wintypes.DWORD]
k32.EndUpdateResourceW.argtypes = [wintypes.HANDLE, wintypes.BOOL]

RT_ICON, RT_GROUP_ICON = 3, 14
LANG = 0x0409

data = open(ico, "rb").read()
reserved, rtype, count = struct.unpack_from("<HHH", data, 0)
assert reserved == 0 and rtype == 1, "不是合法 ICO"

entries = []
for i in range(count):
    off = 6 + i * 16
    bw, bh, bc, br, planes, bits, nbytes, img_off = struct.unpack_from("<BBBBHHII", data, off)
    entries.append((bw, bh, bc, br, planes, bits, nbytes, img_off))

# 组图标目录：ICONDIR 头 + 每个成员的 GRPICONDIRENTRY（末位是资源 ID）
grp = struct.pack("<HHH", 0, 1, count)
for idx, (bw, bh, bc, br, planes, bits, nbytes, img_off) in enumerate(entries, start=1):
    grp += struct.pack("<BBBBHHIH", bw, bh, bc, br, planes, bits, nbytes, idx)

h = k32.BeginUpdateResourceW(exe, False)
if not h:
    raise SystemExit("BeginUpdateResource 失败: %d" % ctypes.get_last_error())

try:
    for idx, (bw, bh, bc, br, planes, bits, nbytes, img_off) in enumerate(entries, start=1):
        blob = data[img_off:img_off + nbytes]
        buf = ctypes.create_string_buffer(blob, len(blob))
        if not k32.UpdateResourceW(h, ctypes.c_void_p(RT_ICON),
                                   ctypes.c_void_p(idx), LANG, buf, len(blob)):
            raise SystemExit("写 RT_ICON %d 失败" % idx)
    gbuf = ctypes.create_string_buffer(grp, len(grp))
    if not k32.UpdateResourceW(h, ctypes.c_void_p(RT_GROUP_ICON),
                               ctypes.c_void_p(1), LANG, gbuf, len(grp)):
        raise SystemExit("写 RT_GROUP_ICON 失败")
finally:
    k32.EndUpdateResourceW(h, False)

print(f"已写入 {count} 个尺寸的图标 -> {exe}")
