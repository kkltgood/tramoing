"""把 `assets/icon-1024.png` 转成 Windows 用的多尺寸 `.ico`。

为什么要转：

- 源图是 **1024×1024、无 alpha** 的 PNG，圆角外面是**纯黑**（不是透明的）。
  直接塞进 exe 的话，任务栏/开始菜单的大图标会带四个黑角，浅色任务栏上尤其难看；
- Windows 会按场景挑尺寸（16 任务栏 / 20 托盘 / 32 Alt-Tab / 48 大图标 /
  256 超大图标），只放一张 1024 的图进去，系统缩放出来的小尺寸会糊成一团。

做法：

1. 圆角矩形（(44,44)-(980,980)，半径 ≈196）用 SDF 算出每个源像素的覆盖率，
   当作 alpha —— 圆角外面的纯黑于是变成透明，边缘还是抗锯齿的；
2. 预乘 alpha 之后做**可分离**的盒式降采样（水平一遍、垂直一遍），
   比逐点取最近邻干净得多，16px 下波形还认得出；
3. 每一帧写成 **32bpp BMP**（不是 PNG 压缩帧）：NSIS 3 对 PNG 帧的支持不稳，
   而 256px 的 BMP 帧也就 256KB，安装包用 LZMA 压完差别可以忽略。

用法（改了图标源图之后重跑一次，产物要一起提交）：

    python assets/gen-ico.py
"""

import math
import struct
import zlib
from array import array

SRC = "assets/icon-1024.png"
DST = "assets/tramoing.ico"

# 圆角矩形的实测几何（含 1px 抗锯齿余量）
LEFT, TOP, RIGHT, BOTTOM, RADIUS = 44.0, 44.0, 980.0, 980.0, 196.0

# Windows 实际会用到的尺寸；20 是 125% DPI 下的托盘尺寸
SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]


def read_png(path):
    data = open(path, "rb").read()
    pos, idat = 8, b""
    while pos < len(data):
        ln = struct.unpack(">I", data[pos : pos + 4])[0]
        typ = data[pos + 4 : pos + 8]
        chunk = data[pos + 8 : pos + 8 + ln]
        if typ == b"IHDR":
            w, h, bd, ct = struct.unpack(">IIBB", chunk[:10])
        elif typ == b"IDAT":
            idat += chunk
        pos += 12 + ln
    assert bd == 8, "只处理 8 位/通道的 PNG"
    ch = {0: 1, 2: 3, 4: 2, 6: 4}[ct]
    raw = zlib.decompress(idat)
    stride = w * ch
    prev = bytearray(stride)
    rows = []
    i = 0

    def paeth(a, b, c):
        p = a + b - c
        pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
        return a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)

    for _ in range(h):
        f = raw[i]
        i += 1
        line = bytearray(raw[i : i + stride])
        i += stride
        if f == 1:
            for x in range(ch, stride):
                line[x] = (line[x] + line[x - ch]) & 255
        elif f == 2:
            for x in range(stride):
                line[x] = (line[x] + prev[x]) & 255
        elif f == 3:
            for x in range(stride):
                a = line[x - ch] if x >= ch else 0
                line[x] = (line[x] + ((a + prev[x]) >> 1)) & 255
        elif f == 4:
            for x in range(stride):
                a = line[x - ch] if x >= ch else 0
                c = prev[x - ch] if x >= ch else 0
                line[x] = (line[x] + paeth(a, prev[x], c)) & 255
        rows.append(bytes(line))
        prev = line
    return w, h, ch, rows


def axis_offsets(lo, hi, yc):
    """点到内缩矩形在某个轴上的距离（0 表示落在直边区段里）。"""
    if yc < lo:
        return lo - yc
    if yc > hi:
        return yc - hi
    return 0.0


def coverage(yc, xc):
    """源像素中心落在圆角矩形里的覆盖率，近似 1px 抗锯齿。"""
    vx = axis_offsets(LEFT + RADIUS, RIGHT - RADIUS, xc)
    vy = axis_offsets(TOP + RADIUS, BOTTOM - RADIUS, yc)
    d = math.hypot(vx, vy) if (vx and vy) else (vx or vy)
    t = RADIUS - d
    if t >= 0.5:
        return 1.0
    if t <= -0.5:
        return 0.0
    return t + 0.5


def premultiplied(w, h, ch, rows):
    """四个通道的预乘图：R·a, G·a, B·a, a（a 是 0..255）。"""
    out = [array("f", bytes(4 * w * h)) for _ in range(4)]
    r_out, g_out, b_out, a_out = out
    for y in range(h):
        yc = y + 0.5
        row = rows[y]
        base = y * w
        for x in range(w):
            a = coverage(yc, x + 0.5)
            if a <= 0.0:
                continue
            o = x * ch
            idx = base + x
            r_out[idx] = row[o] * a
            g_out[idx] = row[o + 1] * a
            b_out[idx] = row[o + 2] * a
            a_out[idx] = a * 255.0
    return out


def ranges(start, size, span):
    """目标像素 i 覆盖的源区间（盒式滤波的边界按像素中心对齐）。"""
    scale = span / size
    out = []
    for i in range(size):
        a = int(round(start + i * scale))
        b = int(round(start + (i + 1) * scale))
        out.append((a, max(a + 1, b)))
    return out


def box_downsample(src, w, h, size):
    """可分离盒式降采样：先横后竖，预乘空间里取平均（边缘不会发黑）。"""
    cols = ranges(LEFT, size, RIGHT - LEFT)
    lines = ranges(TOP, size, BOTTOM - TOP)

    tmp = [array("f", bytes(4 * size * h)) for _ in range(4)]
    for c in range(4):
        s, t = src[c], tmp[c]
        for y in range(h):
            base = y * w
            for i, (x0, x1) in enumerate(cols):
                acc = 0.0
                for x in range(x0, x1):
                    acc += s[base + x]
                t[i * h + y] = acc / (x1 - x0)

    out = [array("f", bytes(4 * size * size)) for _ in range(4)]
    for c in range(4):
        t, o = tmp[c], out[c]
        for j, (y0, y1) in enumerate(lines):
            n = y1 - y0
            for i in range(size):
                acc = 0.0
                for y in range(y0, y1):
                    acc += t[i * h + y]
                o[j * size + i] = acc / n
    return out


def build_bgra(src, w, h, size):
    r, g, b, a = box_downsample(src, w, h, size)
    bgra = bytearray(size * size * 4)
    for i in range(size * size):
        alpha = a[i]
        if alpha <= 0.5:
            continue
        k = 255.0 / alpha
        o = i * 4
        bgra[o] = min(255, int(b[i] * k + 0.5))
        bgra[o + 1] = min(255, int(g[i] * k + 0.5))
        bgra[o + 2] = min(255, int(r[i] * k + 0.5))
        bgra[o + 3] = min(255, int(alpha + 0.5))
    return bgra


def bmp_frame(bgra, size):
    """BITMAPINFOHEADER + 自下而上的 BGRA + AND 掩码（30 年历史的怪格式）。"""
    header = struct.pack(
        "<IiiHHIIiiII",
        40,        # biSize
        size,      # biWidth
        size * 2,  # biHeight：颜色 + 掩码两张图叠着，所以是两倍
        1,         # biPlanes
        32,        # biBitCount
        0,         # BI_RGB
        0, 0, 0, 0, 0,
    )
    body = bytearray()
    for y in range(size - 1, -1, -1):  # BMP 是自下而上
        body += bgra[y * size * 4 : (y + 1) * size * 4]
    # AND 掩码：32bpp 用不到，但格式要求每行补齐到 4 字节
    body += bytes((((size + 31) // 32) * 4) * size)
    return header + bytes(body)


def main():
    w, h, ch, rows = read_png(SRC)
    print(f"源图 {w}×{h}，圆角矩形 ({LEFT:.0f},{TOP:.0f})-({RIGHT:.0f},{BOTTOM:.0f}) r={RADIUS:.0f}")
    src = premultiplied(w, h, ch, rows)

    imgs = []
    for s in SIZES:
        frame = bmp_frame(build_bgra(src, w, h, s), s)
        imgs.append((s, frame))
        print(f"  {s:>3}px  {len(frame):>7} bytes")

    out = bytearray(struct.pack("<HHH", 0, 1, len(imgs)))  # ICONDIR
    offset = 6 + 16 * len(imgs)
    for s, data in imgs:
        out += struct.pack(
            "<BBBBHHII",
            0 if s >= 256 else s,  # 256 在格式里写 0
            0 if s >= 256 else s,
            0, 0, 1, 32,
            len(data), offset,
        )
        offset += len(data)
    for _, data in imgs:
        out += data
    open(DST, "wb").write(bytes(out))
    print(f"ok {DST} {len(out) / 1024:.0f} KB")


if __name__ == "__main__":
    main()
