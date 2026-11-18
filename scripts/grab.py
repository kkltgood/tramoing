"""抓一块屏幕区域，存成 BMP 或 PNG（零依赖，只用 ctypes + GDI）。

DPI 无感的进程拿到的全是虚拟坐标，3840×2160@150% 下会差 1.5 倍，
所以先 SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2) 再抓。

用法：python scripts/grab.py <x> <y> <w> <h> <out.bmp|out.png> [zoom]
坐标都是**物理像素**。`zoom` 是最近邻整数放大倍数，用来放大核对小字号/图形，
默认 1。
"""

import ctypes
import struct
import sys
import zlib
from ctypes import wintypes as wt

user32 = ctypes.WinDLL("user32", use_last_error=True)
gdi32 = ctypes.WinDLL("gdi32", use_last_error=True)

# PER_MONITOR_AWARE_V2 = -4
user32.SetProcessDpiAwarenessContext(ctypes.c_void_p(-4))

SRCCOPY = 0x00CC0020
DIB_RGB_COLORS = 0


class BITMAPFILEHEADER(ctypes.Structure):
    _pack_ = 1
    _fields_ = [
        ("bfType", wt.WORD),
        ("bfSize", wt.DWORD),
        ("bfReserved1", wt.WORD),
        ("bfReserved2", wt.WORD),
        ("bfOffBits", wt.DWORD),
    ]


class BITMAPINFOHEADER(ctypes.Structure):
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


def grab(x, y, w, h, path, zoom=1):
    screen_dc = user32.GetDC(None)
    mem_dc = gdi32.CreateCompatibleDC(screen_dc)
    bmp = gdi32.CreateCompatibleBitmap(screen_dc, w, h)
    gdi32.SelectObject(mem_dc, bmp)

    ok = gdi32.BitBlt(mem_dc, 0, 0, w, h, screen_dc, x, y, SRCCOPY)
    if not ok:
        raise OSError("BitBlt failed")

    bi = BITMAPINFOHEADER()
    bi.biSize = ctypes.sizeof(BITMAPINFOHEADER)
    bi.biWidth = w
    bi.biHeight = -h  # 负数 = 自上而下，省得再翻一遍行
    bi.biPlanes = 1
    bi.biBitCount = 32
    bi.biCompression = 0  # BI_RGB

    buf = ctypes.create_string_buffer(w * h * 4)
    gdi32.GetDIBits(mem_dc, bmp, 0, h, buf, ctypes.byref(bi), DIB_RGB_COLORS)

    gdi32.DeleteObject(bmp)
    gdi32.DeleteDC(mem_dc)
    user32.ReleaseDC(None, screen_dc)

    data = buf.raw
    if zoom > 1:
        # 最近邻放大：核对字形/三角这类小图形时不能用上面那套平滑插值，
        # 否则看不出原始像素长什么样。
        zw, zh = w * zoom, h * zoom
        out = bytearray(zw * zh * 4)
        for yy in range(zh):
            src = (yy // zoom) * w * 4
            row = data[src : src + w * 4]
            dst = yy * zw * 4
            for xx in range(w):
                out[dst + xx * zoom * 4 : dst + (xx + 1) * zoom * 4] = row[xx * 4 : xx * 4 + 4] * zoom
        data, w, h = bytes(out), zw, zh

    if path.lower().endswith(".png"):
        write_png(path, w, h, data)
    else:
        fh = BITMAPFILEHEADER()
        fh.bfType = 0x4D42  # "BM"
        fh.bfOffBits = ctypes.sizeof(BITMAPFILEHEADER) + ctypes.sizeof(BITMAPINFOHEADER)
        fh.bfSize = fh.bfOffBits + len(data)
        with open(path, "wb") as f:
            f.write(bytes(fh))
            f.write(bytes(bi))
            f.write(data)


def write_png(path, w, h, bgra):
    """BGRA（自上而下）→ PNG。只为存档截图，速度够用就行。"""

    def chunk(kind, data):
        out = struct.pack(">I", len(data)) + kind + data
        return out + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)

    raw = bytearray()
    for y in range(h):
        raw.append(0)  # filter: None
        row = bgra[y * w * 4 : (y + 1) * w * 4]
        rgb = bytearray(w * 3)
        rgb[0::3] = row[2::4]
        rgb[1::3] = row[1::4]
        rgb[2::3] = row[0::4]
        raw.extend(rgb)

    ihdr = struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)  # 8bit RGB
    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n")
        f.write(chunk(b"IHDR", ihdr))
        f.write(chunk(b"IDAT", zlib.compress(bytes(raw), 9)))
        f.write(chunk(b"IEND", b""))


if __name__ == "__main__":
    x, y, w, h = (int(v) for v in sys.argv[1:5])
    zoom = int(sys.argv[6]) if len(sys.argv) > 6 else 1
    grab(x, y, w, h, sys.argv[5], zoom)
    print("ok")
