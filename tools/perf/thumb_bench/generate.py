"""Generate deterministic 512px thumbnails for the browser decode benchmark."""

from __future__ import annotations

import random
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter

COUNT = 150
SIZE = 512
ROOT = Path(__file__).resolve().parent / "images"


def lerp(a: int, b: int, t: float) -> int:
    return round(a + (b - a) * t)


def make_image(seed: int) -> Image.Image:
    rng = random.Random(seed)
    image = Image.new("RGB", (SIZE, SIZE))
    draw = ImageDraw.Draw(image)

    top = tuple(rng.randrange(20, 235) for _ in range(3))
    bottom = tuple(rng.randrange(20, 235) for _ in range(3))
    for y in range(SIZE):
        t = y / (SIZE - 1)
        draw.line(
            [(0, y), (SIZE, y)],
            fill=tuple(lerp(top[c], bottom[c], t) for c in range(3)),
        )

    for _ in range(26):
        x0 = rng.randrange(-120, SIZE)
        y0 = rng.randrange(-120, SIZE)
        x1 = x0 + rng.randrange(30, 280)
        y1 = y0 + rng.randrange(30, 280)
        color = tuple(rng.randrange(0, 256) for _ in range(3))
        if rng.random() < 0.5:
            draw.ellipse((x0, y0, x1, y1), fill=color)
        else:
            draw.polygon([(x0, y0), (x1, y0), (rng.randrange(SIZE), y1)], fill=color)

    noise = Image.effect_noise((SIZE, SIZE), 22).convert("RGB")
    image = Image.blend(image, noise, 0.045)
    image = image.filter(ImageFilter.GaussianBlur(radius=0.6))
    return image


def main() -> None:
    jpeg_dir = ROOT / "jpg"
    webp_dir = ROOT / "webp"
    jpeg_dir.mkdir(parents=True, exist_ok=True)
    webp_dir.mkdir(parents=True, exist_ok=True)

    jpeg_total = 0
    webp_total = 0
    for index in range(1, COUNT + 1):
        image = make_image(index * 7919)
        jpeg_path = jpeg_dir / f"{index:03}.jpg"
        webp_path = webp_dir / f"{index:03}.webp"
        image.save(jpeg_path, "JPEG", quality=82, optimize=True, progressive=True)
        image.save(webp_path, "WEBP", quality=80, method=4)
        jpeg_total += jpeg_path.stat().st_size
        webp_total += webp_path.stat().st_size

    print(f"JPEG: {jpeg_total / 1048576:.2f} MB, avg {jpeg_total / COUNT / 1024:.1f} KB")
    print(f"WebP: {webp_total / 1048576:.2f} MB, avg {webp_total / COUNT / 1024:.1f} KB")


if __name__ == "__main__":
    main()
