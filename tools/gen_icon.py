"""Draws the MCP Manager icon: a hub with three connected nodes on a rounded square.

    python tools/gen_icon.py

Writes assets/mcpm.ico (16-256 px, embedded in mcpm.exe by crates/mcpm/build.rs),
assets/mcpm.png (256 px preview) and assets/window-icon.rgba (64x64 raw RGBA for the window).
Needs Pillow. The outputs are committed, so building never needs Python.
"""
from pathlib import Path

from PIL import Image, ImageDraw

OUT = Path(__file__).resolve().parent.parent / "assets"
BIG = 1024
BG = (79, 70, 229, 255)       # indigo
BG_TOP = (99, 102, 241, 255)
FG = (255, 255, 255, 255)


def draw(size: int) -> Image.Image:
    s = BIG
    img = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    # background: rounded square with a soft vertical gradient
    grad = Image.new("RGBA", (s, s))
    gd = ImageDraw.Draw(grad)
    for y in range(s):
        t = y / (s - 1)
        gd.line([(0, y), (s, y)], fill=tuple(int(BG_TOP[i] + (BG[i] - BG_TOP[i]) * t) for i in range(4)))
    mask = Image.new("L", (s, s), 0)
    ImageDraw.Draw(mask).rounded_rectangle([0, 0, s - 1, s - 1], radius=int(s * 0.22), fill=255)
    img.paste(grad, (0, 0), mask)

    d = ImageDraw.Draw(img)
    cx, cy = s * 0.5, s * 0.54
    hub_r = s * 0.15
    nodes = [(s * 0.27, s * 0.28), (s * 0.73, s * 0.28), (s * 0.5, s * 0.83)]
    node_r = s * 0.095
    w = int(s * 0.07)
    for nx, ny in nodes:
        d.line([(cx, cy), (nx, ny)], fill=FG, width=w)
    for nx, ny in nodes:
        d.ellipse([nx - node_r, ny - node_r, nx + node_r, ny + node_r], fill=FG)
    d.ellipse([cx - hub_r, cy - hub_r, cx + hub_r, cy + hub_r], fill=FG)
    # a hole in the hub so it reads as a ring at small sizes
    hr = hub_r * 0.45
    d.ellipse([cx - hr, cy - hr, cx + hr, cy + hr], fill=BG)
    return img.resize((size, size), Image.LANCZOS)


def main() -> None:
    OUT.mkdir(exist_ok=True)
    sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
    imgs = {n: draw(n) for n in sizes}
    imgs[256].save(OUT / "mcpm.ico", format="ICO", sizes=[(n, n) for n in sizes], append_images=[imgs[n] for n in sizes if n != 256])
    imgs[256].save(OUT / "mcpm.png")
    (OUT / "window-icon.rgba").write_bytes(imgs[64].tobytes())
    print("wrote", OUT)


if __name__ == "__main__":
    main()
