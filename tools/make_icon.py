#!/usr/bin/env python3
"""Generate the application icon from code, so it needs no design tool.

Writes:
  assets/icon.ico      multi-size Windows icon (16..256), embedded in both exes
  assets/icon.png      256x256, for the Linux .desktop entry and the README
  assets/icon_64.rgba  raw 64x64 RGBA, embedded as the egui window icon

Design: two overlapping rounded slabs (a thing and its copy) on a dark tile,
with a small check mark on the front slab. It reads as "backup" at 16 px.

Standard library only. Run from the repo root: python tools/make_icon.py
"""
import math
import struct
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "assets"
SS = 4  # supersampling factor per axis


def rrect_sdf(px, py, cx, cy, hw, hh, r):
    dx = abs(px - cx) - (hw - r)
    dy = abs(py - cy) - (hh - r)
    ox, oy = max(dx, 0.0), max(dy, 0.0)
    return math.hypot(ox, oy) + min(max(dx, dy), 0.0) - r


def seg_sdf(px, py, ax, ay, bx, by):
    vx, vy = bx - ax, by - ay
    wx, wy = px - ax, py - ay
    t = max(0.0, min(1.0, (wx * vx + wy * vy) / (vx * vx + vy * vy)))
    return math.hypot(wx - t * vx, wy - t * vy)


def cover(d, aa):
    return max(0.0, min(1.0, 0.5 - d / aa))


def blend(dst, src, a):
    return tuple(dst[i] * (1 - a) + src[i] * a for i in range(3))


def render(size):
    """Return a list of rows of (r, g, b, a) floats in 0..1."""
    n = size * SS
    aa = 1.0  # one supersampled pixel of antialias
    # Geometry in unit coordinates, scaled to n.
    s = n
    tile = (0.5 * s, 0.5 * s, 0.46 * s, 0.46 * s, 0.17 * s)
    back = (0.455 * s, 0.445 * s, 0.26 * s, 0.19 * s, 0.045 * s)
    front = (0.555 * s, 0.575 * s, 0.26 * s, 0.19 * s, 0.045 * s)
    # Check mark on the front slab.
    c1 = (0.455 * s, 0.585 * s, 0.53 * s, 0.655 * s)
    c2 = (0.53 * s, 0.655 * s, 0.665 * s, 0.50 * s)
    check_w = 0.035 * s

    col_tile = (0.11, 0.16, 0.22)
    col_back = (0.18, 0.62, 0.60)
    col_front = (0.91, 0.95, 0.96)
    col_check = (0.10, 0.45, 0.42)

    rows = []
    for y in range(n):
        py = y + 0.5
        row = []
        for x in range(n):
            px = x + 0.5
            a_tile = cover(rrect_sdf(px, py, *tile), aa)
            rgb = col_tile
            a_back = cover(rrect_sdf(px, py, *back), aa)
            rgb = blend(rgb, col_back, a_back)
            a_front = cover(rrect_sdf(px, py, *front), aa)
            rgb = blend(rgb, col_front, a_front)
            d_check = min(seg_sdf(px, py, *c1), seg_sdf(px, py, *c2)) - check_w
            a_check = cover(d_check, aa) * a_front
            rgb = blend(rgb, col_check, a_check)
            row.append((rgb[0], rgb[1], rgb[2], a_tile))
        rows.append(row)

    # Box-downsample.
    out = []
    for y in range(size):
        orow = []
        for x in range(size):
            r = g = b = a = 0.0
            for sy in range(SS):
                for sx in range(SS):
                    pr, pg, pb, pa = rows[y * SS + sy][x * SS + sx]
                    # Premultiply so edge pixels do not darken.
                    r += pr * pa
                    g += pg * pa
                    b += pb * pa
                    a += pa
            k = SS * SS
            if a > 0:
                orow.append((r / a, g / a, b / a, a / k))
            else:
                orow.append((0.0, 0.0, 0.0, 0.0))
        out.append(orow)
    return out


def to_bytes(img):
    b = bytearray()
    for row in img:
        for r, g, bl, a in row:
            b += bytes((int(round(r * 255)), int(round(g * 255)), int(round(bl * 255)), int(round(a * 255))))
    return bytes(b)


def png(img):
    size = len(img)
    raw = bytearray()
    rgba = to_bytes(img)
    stride = size * 4
    for y in range(size):
        raw += b"\x00" + rgba[y * stride:(y + 1) * stride]

    def chunk(tag, data):
        c = struct.pack(">I", len(data)) + tag + data
        return c + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
            + chunk(b"IEND", b""))


def dib(img):
    """A 32-bpp BGRA bottom-up DIB with an all-zero AND mask, as .ico wants."""
    size = len(img)
    hdr = struct.pack("<IiiHHIIiiII", 40, size, size * 2, 1, 32, 0, size * size * 4, 0, 0, 0, 0)
    px = bytearray()
    for row in reversed(img):
        for r, g, b, a in row:
            px += bytes((int(round(b * 255)), int(round(g * 255)), int(round(r * 255)), int(round(a * 255))))
    mask_stride = ((size + 31) // 32) * 4
    mask = bytes(mask_stride * size)
    return hdr + bytes(px) + mask


def ico(images):
    entries = []
    blobs = []
    offset = 6 + 16 * len(images)
    for img in images:
        size = len(img)
        blob = png(img) if size >= 256 else dib(img)
        entries.append(struct.pack("<BBBBHHII", size % 256, size % 256, 0, 0, 1, 32, len(blob), offset))
        blobs.append(blob)
        offset += len(blob)
    return struct.pack("<HHH", 0, 1, len(images)) + b"".join(entries) + b"".join(blobs)


def main():
    OUT.mkdir(exist_ok=True)
    sizes = [256, 128, 64, 48, 32, 24, 16]
    imgs = {s: render(s) for s in sizes}
    (OUT / "icon.ico").write_bytes(ico([imgs[s] for s in sizes]))
    (OUT / "icon.png").write_bytes(png(imgs[256]))
    (OUT / "icon_64.rgba").write_bytes(to_bytes(imgs[64]))
    for s in sizes:
        print(f"  {s:>3}px rendered")
    print(f"wrote {OUT / 'icon.ico'}, icon.png, icon_64.rgba")


if __name__ == "__main__":
    main()
