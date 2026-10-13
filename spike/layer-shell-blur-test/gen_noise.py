#!/usr/bin/env python3
"""Generate the deterministic checkerboard the blur experiment lays down.

4px black/white squares: maximum high-frequency content, so a compositor-side
blur is obvious and cannot be confused with ordinary attenuation.

Seed 1234 is fixed so the pattern is byte-identical between runs.

  uv run --with pillow --python 3.12 -- python3 gen_noise.py
"""
import random
from pathlib import Path

from PIL import Image

OUT = Path("/tmp/ci-blur2/noise.png")
W, H = 1600, 520
CELL = 4

img = Image.new("RGB", (W, H))
px = img.load()
for y in range(H):
    for x in range(W):
        v = 255 if ((x // CELL) + (y // CELL)) % 2 == 0 else 0
        px[x, y] = (v, v, v)

OUT.parent.mkdir(parents=True, exist_ok=True)
img.save(OUT)
print(f"wrote {OUT} {img.size} cell={CELL}px")
