#!/usr/bin/env python3
from __future__ import annotations

import argparse
from pathlib import Path


def image_to_halfblock(img, width: int, threshold: int) -> str:
    from PIL import Image as _Img

    aspect = img.height / img.width
    height = int(width * aspect) // 2 * 2
    img = img.resize((width, height), _Img.Resampling.LANCZOS)

    lines: list[str] = []
    for y in range(0, height, 2):
        row: list[str] = []
        for x in range(width):
            top = img.getpixel((x, y)) > threshold
            bot = img.getpixel((x, y + 1)) > threshold if y + 1 < height else False
            if top and bot:
                row.append("█")
            elif top:
                row.append("▀")
            elif bot:
                row.append("▄")
            else:
                row.append(" ")
        lines.append("".join(row).rstrip())
    return "\n".join(lines)


def main() -> None:
    parser = argparse.ArgumentParser(description="Generate c3 CLI banner asset.")
    parser.add_argument("--width", "-w", type=int, default=50)
    parser.add_argument("--threshold", "-t", type=int, default=100)
    parser.add_argument("--image", "-i", type=Path, default=Path("docs/images/logo_bw.png"))
    parser.add_argument("--out", "-o", type=Path, default=Path("cli/assets/banner_unicode.txt"))
    args = parser.parse_args()

    from PIL import Image

    img_gray = Image.open(args.image).convert("L")
    art = image_to_halfblock(img_gray, width=args.width, threshold=args.threshold)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(art + "\n", encoding="utf-8")
    print(f"{args.out} ({len(art.splitlines())} lines)")


if __name__ == "__main__":
    main()
