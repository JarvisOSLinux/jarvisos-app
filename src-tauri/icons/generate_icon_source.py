#!/usr/bin/env python3
"""Generate every icon bundle.icon references, with no third-party deps.

    python3 src-tauri/icons/generate_icon_source.py           # write
    python3 src-tauri/icons/generate_icon_source.py --check   # verify

Writes icon-source.png plus the five files tauri.conf.json requires:
32x32.png, 128x128.png, 128x128@2x.png, icon.icns, icon.ico. Those five
are COMMITTED -- the Tauri build reads them directly, so a clean checkout
could not compile without them (the previous version of this script wrote
only icon-source.png and left the rest to `cargo tauri icon`, which meant
`cargo build` failed on any machine that had not run the CLI by hand).

The art is procedural, so each size is rendered at its own resolution
rather than resampled.

--check compares PIXELS, not file bytes: deflate output differs between zlib
builds, so two correct runs on different machines produce different bytes for
identical images. A byte comparison here fails on CI while nothing is actually
wrong.
"""
import os
import struct
import sys
import zlib

SIZE = 1024
BG = (10, 14, 26, 255)       # #0a0e1a
CYAN = (0, 200, 255, 255)    # #00c8ff


def make_pixels(size=SIZE):
    cx = cy = size / 2
    outer_r = size * 0.42
    ring_thickness = size * 0.07
    inner_r = size * 0.10
    rows = []
    for y in range(size):
        row = bytearray()
        for x in range(size):
            dx, dy = x - cx, y - cy
            dist = (dx * dx + dy * dy) ** 0.5
            if abs(dist - outer_r) <= ring_thickness / 2 or dist <= inner_r:
                px = CYAN
            else:
                px = BG
            row.extend(px)
        rows.append(bytes(row))
    return rows


def png_bytes(size):
    def chunk(tag, data):
        return (struct.pack(">I", len(data)) + tag + data +
                struct.pack(">I", zlib.crc32(tag + data) & 0xffffffff))

    rows = make_pixels(size)
    sig = b'\x89PNG\r\n\x1a\n'
    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    raw = b''.join(b'\x00' + r for r in rows)
    idat = zlib.compress(raw, 9)
    return sig + chunk(b'IHDR', ihdr) + chunk(b'IDAT', idat) + chunk(b'IEND', b'')


def write_png(path, size):
    with open(path, 'wb') as f:
        f.write(png_bytes(size))


def write_ico(path, sizes):
    """ICO with PNG-compressed entries (supported since Windows Vista)."""
    images = [png_bytes(s) for s in sizes]
    header = struct.pack("<HHH", 0, 1, len(images))
    offset = len(header) + 16 * len(images)
    entries, blobs = b'', b''
    for size, data in zip(sizes, images):
        # 256 is encoded as 0 in the 1-byte width/height fields.
        dim = 0 if size >= 256 else size
        entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32,
                               len(data), offset)
        blobs += data
        offset += len(data)
    with open(path, 'wb') as f:
        f.write(header + entries + blobs)


def write_icns(path, type_sizes):
    """ICNS with PNG entries; OSType codes pick the slot for each size."""
    body = b''
    for ostype, size in type_sizes:
        data = png_bytes(size)
        body += ostype + struct.pack(">I", len(data) + 8) + data
    with open(path, 'wb') as f:
        f.write(b'icns' + struct.pack(">I", len(body) + 8) + body)


def png_pixels(data):
    """Decode one of OUR PNGs to raw RGBA rows.

    Only handles what png_bytes writes -- 8-bit RGBA, no interlace, every row
    filtered with type 0 (None) -- so unfiltering is just dropping each row's
    leading filter byte.
    """
    assert data[:8] == b'\x89PNG\r\n\x1a\n', "not a PNG"
    pos, width, idat = 8, None, b''
    while pos < len(data):
        length = struct.unpack(">I", data[pos:pos + 4])[0]
        tag = data[pos + 4:pos + 8]
        body = data[pos + 8:pos + 8 + length]
        if tag == b'IHDR':
            width = struct.unpack(">II", body[:8])[0]
        elif tag == b'IDAT':
            idat += body
        pos += 12 + length
    raw = zlib.decompress(idat)
    stride = width * 4 + 1
    return [raw[i + 1:i + stride] for i in range(0, len(raw), stride)]


def embedded_pngs(data):
    """Yield each PNG blob inside an ICO or ICNS container."""
    sig = b'\x89PNG\r\n\x1a\n'
    start = data.find(sig)
    while start != -1:
        end = data.find(sig, start + 1)
        yield data[start:end if end != -1 else len(data)]
        start = end


def check(here, targets):
    failures = []
    for name, expected_sizes in targets:
        path = os.path.join(here, name)
        if not os.path.exists(path):
            failures.append(f"{name}: missing")
            continue
        with open(path, 'rb') as f:
            blob = f.read()
        found = list(embedded_pngs(blob)) if name.endswith(('.ico', '.icns')) \
            else [blob]
        if len(found) != len(expected_sizes):
            failures.append(
                f"{name}: holds {len(found)} image(s), expected "
                f"{len(expected_sizes)}")
            continue
        for image, size in zip(found, expected_sizes):
            if png_pixels(image) != make_pixels(size):
                failures.append(f"{name}: {size}x{size} pixels differ")
    return failures


if __name__ == "__main__":
    here = os.path.dirname(__file__)
    targets = [
        ("32x32.png", [32]),
        ("128x128.png", [128]),
        ("128x128@2x.png", [256]),
        ("icon.ico", [16, 32, 48, 64, 128, 256]),
        ("icon.icns", [128, 256, 512]),
    ]

    if "--check" in sys.argv:
        problems = check(here, targets)
        if problems:
            print("committed icons do not match this generator:")
            for line in problems:
                print(f"  {line}")
            sys.exit(1)
        print("committed icons match this generator")
        sys.exit(0)

    write_png(os.path.join(here, "icon-source.png"), SIZE)
    write_png(os.path.join(here, "32x32.png"), 32)
    write_png(os.path.join(here, "128x128.png"), 128)
    write_png(os.path.join(here, "128x128@2x.png"), 256)
    write_ico(os.path.join(here, "icon.ico"), [16, 32, 48, 64, 128, 256])
    write_icns(os.path.join(here, "icon.icns"),
               [(b'ic07', 128), (b'ic08', 256), (b'ic09', 512)])
    print("wrote icon-source.png, 32x32.png, 128x128.png, 128x128@2x.png, "
          "icon.ico, icon.icns")
