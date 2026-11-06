"""Generate icon.ico (for the exe) and the extension's PNG icons.

    python tools/make_icons.py

Pure stdlib. Large ICO frames are stored PNG-compressed, which Vista+ supports
and which keeps the embedded icon from dominating the binary.
"""
import os
import struct
import zlib

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ICO = os.path.join(HERE, "icon.ico")
EXT = os.path.join(HERE, "extension")

ICO_SIZES = (16, 32, 48, 64, 128, 256)
PNG_FROM = 64  # frames this size and up are stored as PNG


def shade(fx, fy):
    """Colour at (fx, fy) in the unit square: blue rounded tile, white arrow."""
    radius = 0.21
    dx = max(0.0, abs(fx - 0.5) - (0.5 - radius))
    dy = max(0.0, abs(fy - 0.5) - (0.5 - radius))
    if (dx * dx + dy * dy) ** 0.5 > radius:
        return (0, 0, 0, 0)
    arrow = ((0.17 <= fy <= 0.49 and abs(fx - 0.5) <= 0.085)
             or (0.43 <= fy <= 0.69 and abs(fx - 0.5) <= 0.235 * (0.69 - fy) / 0.26)
             or (0.76 <= fy <= 0.855 and abs(fx - 0.5) <= 0.255))
    if arrow:
        return (255, 255, 255, 255)
    return (int(28 + 26 * (1 - fy)), int(118 - 34 * fy), int(236 - 56 * fy), 255)


def pixels(size):
    """3x3 supersampled shade() -- keeps the 16px icon from looking chewed."""
    grid = (0.17, 0.5, 0.83)
    rows = []
    for y in range(size):
        row = []
        for x in range(size):
            acc, alpha, hits = [0, 0, 0], 0, 0
            for i in grid:
                for j in grid:
                    r, g, b, a = shade((x + i) / size, (y + j) / size)
                    alpha += a
                    if a:
                        acc[0] += r
                        acc[1] += g
                        acc[2] += b
                        hits += 1
            n = max(1, hits)
            row.append((acc[0] // n, acc[1] // n, acc[2] // n, alpha // 9))
        rows.append(row)
    return rows


def png_bytes(size):
    raw = b"".join(b"\0" + bytes(v for p in row for v in p) for row in pixels(size))

    def chunk(tag, data):
        body = tag + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw, 9))
            + chunk(b"IEND", b""))


def bmp_bytes(size):
    rows = pixels(size)
    header = struct.pack("<IiiHHIIiiII", 40, size, size * 2, 1, 32, 0, 0, 0, 0, 0, 0)
    body = b"".join(bytes(v for p in row for v in (p[2], p[1], p[0], p[3]))
                    for row in reversed(rows))          # BGRA, bottom-up
    mask = b"\0" * (((size + 31) // 32) * 4 * size)     # fully opaque AND mask
    return header + body + mask


def write_ico(path, sizes=ICO_SIZES):
    frames = [(s, png_bytes(s) if s >= PNG_FROM else bmp_bytes(s)) for s in sizes]
    out = struct.pack("<HHH", 0, 1, len(frames))
    offset = 6 + 16 * len(frames)
    for size, blob in frames:
        byte = size if size < 256 else 0
        out += struct.pack("<BBBBHHII", byte, byte, 0, 0, 1, 32, len(blob), offset)
        offset += len(blob)
    with open(path, "wb") as f:
        f.write(out + b"".join(blob for _, blob in frames))


def write_rgba(path, size):
    """Raw RGBA rows, which is what the tray icon API wants."""
    with open(path, "wb") as f:
        f.write(bytes(v for row in pixels(size) for p in row for v in p))


def main():
    write_ico(ICO)
    for size in (16, 48, 128):
        with open(os.path.join(EXT, "icon%d.png" % size), "wb") as f:
            f.write(png_bytes(size))
    write_rgba(os.path.join(HERE, "assets", "tray.rgba"), 32)
    print("assets/tray.rgba written (32x32 RGBA)")
    print("icon.ico: %,d bytes".replace("%,d", "{:,}").format(os.path.getsize(ICO)))
    print("extension/icon{16,48,128}.png written")


if __name__ == "__main__":
    main()
