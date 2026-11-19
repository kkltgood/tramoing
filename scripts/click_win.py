"""按标题找窗口 → 抓客户区 / 点一下（PostMessage）。

和 `grab.py` 分工：这个脚本按**窗口**取图，`grab.py` 按**屏幕坐标**取图。
OpenGL 窗口用 `PrintWindow` 抓出来常常是全黑（GL 的交换链不在 GDI 表面上），
真要看内容还是得用 `grab.py` 从屏幕上 BitBlt；这里留着是因为
「窗口在哪、客户区长什么样」用 `list` 一眼就能问出来。

用法：
  python scripts/click_win.py list
  python scripts/click_win.py grab  <title> <out.png>
  python scripts/click_win.py click <title> <x> <y>      # 客户区坐标，PostMessage

进程先设 PER_MONITOR_AWARE_V2：DPI 无感拿到的全是虚拟坐标，150% 下会差 1.5 倍。
PostMessage 的点击只对「不依赖真实光标位置」的控件可靠，需要真实鼠标轨迹时
用 SetCursorPos + mouse_event（本机验证过可用）。
"""

import ctypes
import struct
import sys
import time
import zlib
from ctypes import wintypes as wt

user32 = ctypes.WinDLL("user32", use_last_error=True)
gdi32 = ctypes.WinDLL("gdi32", use_last_error=True)
user32.SetProcessDpiAwarenessContext(ctypes.c_void_p(-4))

WM_MOUSEMOVE = 0x0200
WM_LBUTTONDOWN = 0x0201
WM_LBUTTONUP = 0x0202
PW_RENDERFULLCONTENT = 0x00000002
DIB_RGB_COLORS = 0


def windows():
    out = []

    @ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)
    def cb(hwnd, _):
        n = user32.GetWindowTextLengthW(hwnd)
        if n:
            buf = ctypes.create_unicode_buffer(n + 1)
            user32.GetWindowTextW(hwnd, buf, n + 1)
            r = wt.RECT()
            user32.GetWindowRect(hwnd, ctypes.byref(r))
            ex = user32.GetWindowLongW(hwnd, -20)
            out.append(
                (
                    hwnd,
                    buf.value,
                    (r.left, r.top, r.right - r.left, r.bottom - r.top),
                    user32.IsWindowVisible(hwnd),
                    ex,
                )
            )
        return True

    user32.EnumWindows(cb, 0)
    return out


def find(title):
    all_w = windows()
    exact = [w for w in all_w if w[1] == title]
    cands = exact or [w for w in all_w if title.lower() in w[1].lower()]
    if not cands:
        raise SystemExit(f"找不到标题含 {title!r} 的窗口")
    # 优先可见、且在屏幕上的（常驻实例平时被停在 -32000 那种位置）
    cands.sort(key=lambda w: (not w[3], w[2][0] < -10000))
    return cands[0]


def client_size(hwnd):
    r = wt.RECT()
    user32.GetClientRect(hwnd, ctypes.byref(r))
    return r.right - r.left, r.bottom - r.top


def grab(hwnd, path):
    w, h = client_size(hwnd)
    screen_dc = user32.GetDC(None)
    mem_dc = gdi32.CreateCompatibleDC(screen_dc)
    bmp = gdi32.CreateCompatibleBitmap(mem_dc, w, h)
    gdi32.SelectObject(mem_dc, bmp)
    ok = user32.PrintWindow(hwnd, mem_dc, PW_RENDERFULLCONTENT)
    print("PrintWindow:", ok)

    class BIH(ctypes.Structure):
        _fields_ = [
            ("biSize", wt.DWORD),
            ("biWidth", wt.LONG),
            ("biHeight", wt.LONG),
            ("biPlanes", wt.WORD),
            ("biBitCount", wt.WORD),
            ("biCompression", wt.DWORD),
            ("biSizeImage", wt.DWORD),
            ("biXPelsPerMeter", wt.LONG),
            ("biYPelsPerMeter", wt.LONG),
            ("biClrUsed", wt.DWORD),
            ("biClrImportant", wt.DWORD),
        ]

    bi = BIH()
    bi.biSize = ctypes.sizeof(BIH)
    bi.biWidth = w
    bi.biHeight = -h  # 负数 = 自上而下
    bi.biPlanes = 1
    bi.biBitCount = 32
    buf = ctypes.create_string_buffer(w * h * 4)
    gdi32.GetDIBits(mem_dc, bmp, 0, h, buf, ctypes.byref(bi), DIB_RGB_COLORS)
    gdi32.DeleteObject(bmp)
    gdi32.DeleteDC(mem_dc)
    user32.ReleaseDC(None, screen_dc)

    # BGRA → PNG（透明窗口抓出来 alpha 全是 0，这里直接丢 alpha）
    def chunk(kind, data):
        return (
            struct.pack(">I", len(data))
            + kind
            + data
            + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
        )

    raw = bytearray()
    for yy in range(h):
        raw.append(0)
        row = buf.raw[yy * w * 4 : (yy + 1) * w * 4]
        rgb = bytearray(w * 3)
        rgb[0::3] = row[2::4]
        rgb[1::3] = row[1::4]
        rgb[2::3] = row[0::4]
        raw.extend(rgb)
    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n")
        f.write(chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)))
        f.write(chunk(b"IDAT", zlib.compress(bytes(raw), 9)))
        f.write(chunk(b"IEND", b""))
    print(f"saved {path} {w}x{h}")


def click(hwnd, x, y):
    lp = (int(y) << 16) | (int(x) & 0xFFFF)
    user32.PostMessageW(hwnd, WM_MOUSEMOVE, 0, lp)
    time.sleep(0.05)
    user32.PostMessageW(hwnd, WM_LBUTTONDOWN, 1, lp)
    time.sleep(0.05)
    user32.PostMessageW(hwnd, WM_LBUTTONUP, 0, lp)
    print(f"clicked {x},{y}")


if __name__ == "__main__":
    cmd = sys.argv[1] if len(sys.argv) > 1 else "list"
    if cmd == "list":
        for hwnd, title, rect, vis, ex in windows():
            print(f"{hwnd:>10} vis={int(vis)} ex={ex:#x} rect={rect} {title!r}")
    elif cmd == "grab":
        grab(find(sys.argv[2])[0], sys.argv[3])
    elif cmd == "click":
        click(find(sys.argv[2])[0], int(sys.argv[3]), int(sys.argv[4]))
    else:
        raise SystemExit(__doc__)
