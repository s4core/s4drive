#!/usr/bin/env python3
"""Rebuild icon.ico with modern PNG-format frames for Windows RC.EXE compatibility."""

from PIL import Image
import sys, os

ICONS_DIR = os.path.join(os.path.dirname(__file__), "..", "src-tauri", "icons")
ICO_PATH = os.path.join(ICONS_DIR, "icon.ico")

# Source PNGs we have
png_sources = {
    32:  os.path.join(ICONS_DIR, "32x32.png"),
    128: os.path.join(ICONS_DIR, "128x128.png"),
    256: os.path.join(ICONS_DIR, "128x128@2x.png"),
}

# Standard Windows icon sizes — we want at least these
target_sizes = [16, 24, 32, 48, 64, 128, 256]

images = []
# Find the smallest available source to use as base for sizes smaller than it
available_sizes = sorted([s for s in png_sources if os.path.exists(png_sources[s])])
base_size = available_sizes[0] if available_sizes else 32

for size in sorted(target_sizes):
    if size in png_sources and os.path.exists(png_sources[size]):
        img = Image.open(png_sources[size]).convert("RGBA")
        if img.size != (size, size):
            img = img.resize((size, size), Image.LANCZOS)
        images.append(img)
    else:
        # Downscale from the closest available size
        src_size = min(available_sizes, key=lambda s: abs(s - size))
        img = Image.open(png_sources[src_size]).convert("RGBA")
        img = img.resize((size, size), Image.LANCZOS)
        images.append(img)

if not images:
    print("ERROR: no source PNGs found", file=sys.stderr)
    sys.exit(1)

# Save as ICO — write all images manually
import struct

ico_data = bytearray()
# ICO header: reserved(2) + type(1=ico, 2=cur) + count
count = len(images)
ico_data += struct.pack('<HHH', 0, 1, count)

# Directory entries + image data
offset = 6 + count * 16  # header + dir entries
image_data = []

for img in images:
    # Convert to PNG bytes
    from io import BytesIO
    buf = BytesIO()
    img.save(buf, format='PNG')
    png_bytes = buf.getvalue()
    
    w = img.width if img.width < 256 else 0
    h = img.height if img.height < 256 else 0
    bpp = 32
    colors = 0
    reserved = 0
    planes = 1
    
    ico_data += struct.pack('<BBBBHHII',
        w, h, colors, reserved, planes, bpp,
        len(png_bytes), offset)
    
    image_data.append(png_bytes)
    offset += len(png_bytes)

ico_data += b''.join(image_data)

with open(ICO_PATH, 'wb') as f:
    f.write(ico_data)

# Verify
with open(ICO_PATH, 'rb') as f:
    data = f.read()
count = struct.unpack_from('<H', data, 4)[0]
print(f"✓ Rewrote {ICO_PATH}")
print(f"  Entries: {count}")
print(f"  Size: {len(data)} bytes")
for i in range(count):
    off = 6 + i * 16
    w, h = struct.unpack_from('<BB', data, off)
    dw, dh = (w if w != 0 else 256), (h if h != 0 else 256)
    bpp = struct.unpack_from('<H', data, off + 6)[0]
    sz = struct.unpack_from('<I', data, off + 8)[0]
    img_start = struct.unpack_from('<I', data, off + 12)[0]
    is_png = data[img_start:img_start+4] == b'\x89PNG'
    print(f"  [{i}] {dw}x{dh}x{bpp}, {sz} bytes, PNG={is_png}")
