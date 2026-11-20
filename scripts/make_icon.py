"""Regenerate icon.ico from res/icons/icon.png.

Requires Pillow: python -m pip install pillow
Run from anywhere: python scripts/make_icon.py
"""

from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
source = ROOT / "res" / "icons" / "icon.png"
destination = ROOT / "icon.ico"

image = Image.open(source).convert("RGBA")
image.save(
    destination,
    format="ICO",
    sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
)
print(f"Wrote {destination} ({destination.stat().st_size} bytes)")
