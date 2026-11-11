"""Draws assets/icon.png and assets/icon.ico: six camera tiles on a full-bleed square. Needs Pillow.
Each size is drawn on its own pixel grid, so the small ones stay sharp. Run from the repo root."""
from PIL import Image, ImageDraw

LIGHT, DARK = (0xE6, 0xE6, 0xE1, 255), (0x14, 0x14, 0x13, 255)


def draw(n):
    img = Image.new("RGBA", (n, n), LIGHT)
    d = ImageDraw.Draw(img)
    gap = max(1, round(n / 16))
    tile = (n - 2 * max(2, round(n / 8)) - 2 * gap) // 3
    x0, y0 = (n - 3 * tile - 2 * gap) // 2, (n - 2 * tile - gap) // 2
    for row in range(2):
        for col in range(3):
            x, y = x0 + col * (tile + gap), y0 + row * (tile + gap)
            d.rectangle([x, y, x + tile - 1, y + tile - 1], fill=DARK)
    return img


sizes = [16, 20, 24, 32, 40, 48, 64, 256]
draw(256).save("assets/icon.png")
# GitHub's social preview (repo Settings → Social preview): the icon centered on its own background.
social = Image.new("RGBA", (1280, 640), LIGHT)
social.paste(draw(512), (384, 64))
social.save("assets/social-preview.png")
draw(256).save("assets/icon.ico",sizes=[(s, s) for s in sizes], append_images=[draw(s) for s in sizes[:-1]])
