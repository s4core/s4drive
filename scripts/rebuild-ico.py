#!/usr/bin/env python3
"""Rebuild icon.ico from existing PNG frames without external Python packages."""

from __future__ import annotations

import os
import struct
import sys

ICONS_DIR = os.path.join(os.path.dirname(__file__), "..", "src-tauri", "icons")
ICO_PATH = os.path.join(ICONS_DIR, "icon.ico")

PNG_SOURCES = [
    (32, os.path.join(ICONS_DIR, "32x32.png")),
    (128, os.path.join(ICONS_DIR, "128x128.png")),
    (256, os.path.join(ICONS_DIR, "128x128@2x.png")),
]


def read_png_size(data: bytes) -> tuple[int, int]:
    if len(data) < 24 or data[:8] != b"\x89PNG\r\n\x1a\n":
        raise ValueError("frame is not a PNG")
    width, height = struct.unpack(">II", data[16:24])
    return width, height


def main() -> int:
    frames: list[tuple[int, int, bytes]] = []
    for expected_size, path in PNG_SOURCES:
        if not os.path.exists(path):
            continue
        with open(path, "rb") as file:
            data = file.read()
        width, height = read_png_size(data)
        if width != height:
            raise ValueError(f"{path} is not square: {width}x{height}")
        if width != expected_size:
            print(
                f"warning: {path} is {width}x{height}, expected {expected_size}x{expected_size}",
                file=sys.stderr,
            )
        frames.append((width, height, data))

    if not frames:
        print("ERROR: no source PNGs found", file=sys.stderr)
        return 1

    frames.sort(key=lambda frame: frame[0])
    header = bytearray(struct.pack("<HHH", 0, 1, len(frames)))
    image_data = bytearray()
    offset = 6 + len(frames) * 16

    for width, height, data in frames:
        directory_width = width if width < 256 else 0
        directory_height = height if height < 256 else 0
        header += struct.pack(
            "<BBBBHHII",
            directory_width,
            directory_height,
            0,
            0,
            1,
            32,
            len(data),
            offset,
        )
        image_data += data
        offset += len(data)

    with open(ICO_PATH, "wb") as file:
        file.write(header + image_data)

    print(f"Rebuilt {ICO_PATH}")
    for width, height, data in frames:
        print(f"  {width}x{height}: {len(data)} bytes")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
