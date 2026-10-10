#!/usr/bin/env python3
"""Генератор иконок RAZE.

Рисует знак (папка + диагональное лезвие) и экспортирует master-PNG плюс
 ICO-контейнер со всеми нужными размерами. Иконки собираются в PE-ресурс
 через build.rs, поэтому размеры должны включать 16 (панель задач / меню).

    python icons/export_icons.py
"""

from __future__ import annotations

import math
import pathlib

from PIL import Image, ImageDraw

HERE = pathlib.Path(__file__).resolve().parent
SIZES = (16, 20, 24, 32, 40, 48, 64, 128, 256)
MASTER = 1024

# Палитра: тёмная плитка + светлая папка + акцентное лезвие.
BG_TOP = (37, 41, 51)
BG_BOTTOM = (20, 22, 28)
FOLDER_BACK = (84, 96, 118)
FOLDER_FRONT = (150, 163, 186)
BLADE = (255, 138, 60)
BLADE_HI = (255, 190, 130)
CUT = (20, 22, 28)


def _rounded(draw: ImageDraw.ImageDraw, box, radius, fill):
    draw.rounded_rectangle(box, radius=radius, fill=fill)


def _paste_masked(base: Image.Image, layer: Image.Image, mask: Image.Image):
    base.paste(layer, (0, 0), mask)


def _supersample(factor: int = 4):
    return MASTER * factor


def draw_mark(accent: tuple[int, int, int], ss: int = 4) -> Image.Image:
    """Рисует знак с суперсэмплингом: геометрия считается в увеличенном
    размере и уменьшается вниз — так края остаются чистыми в любом размере."""
    s = MASTER * ss
    img = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)

    # --- плитка ---------------------------------------------------------
    pad = int(s * 0.055)
    tile = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    td = ImageDraw.Draw(tile)
    grad = Image.new("RGBA", (1, s))
    gd = ImageDraw.Draw(grad)
    for y in range(s):
        t = y / max(1, s - 1)
        gd.point(
            (0, y),
            fill=(
                int(BG_TOP[0] + (BG_BOTTOM[0] - BG_TOP[0]) * t),
                int(BG_TOP[1] + (BG_BOTTOM[1] - BG_TOP[1]) * t),
                int(BG_TOP[2] + (BG_BOTTOM[2] - BG_TOP[2]) * t),
                255,
            ),
        )
    grad = grad.resize((s, s))
    mask = Image.new("L", (s, s), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        (pad, pad, s - pad, s - pad), radius=int(s * 0.22), fill=255
    )
    _paste_masked(tile, grad, mask)
    img.alpha_composite(tile)
    d = ImageDraw.Draw(img)

    # --- папка ----------------------------------------------------------
    # Крышка папки: скошенная плашка слева сверху.
    back_l, back_t = int(s * 0.20), int(s * 0.30)
    back_r, back_b = int(s * 0.84), int(s * 0.76)
    tab_w = int(s * 0.22)
    _rounded(
        d,
        (back_l, back_t - int(s * 0.10), back_l + tab_w, back_t + int(s * 0.06)),
        int(s * 0.035),
        FOLDER_BACK,
    )
    _rounded(d, (back_l, back_t, back_r, back_b), int(s * 0.055), FOLDER_BACK)

    # Передняя стенка чуть светлее и с наклоном — читается как объём.
    front = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    fd = ImageDraw.Draw(front)
    fd.polygon(
        [
            (int(s * 0.20), int(s * 0.46)),
            (int(s * 0.86), int(s * 0.46)),
            (int(s * 0.78), int(s * 0.79)),
            (int(s * 0.28), int(s * 0.79)),
        ],
        fill=FOLDER_FRONT,
    )
    fd.line(
        [
            (int(s * 0.20), int(s * 0.46)),
            (int(s * 0.86), int(s * 0.46)),
        ],
        fill=(255, 255, 255, 60),
        width=max(1, int(s * 0.012)),
    )
    img.alpha_composite(front)
    d = ImageDraw.Draw(img)

    # --- «разрезы» папки лезвием ---------------------------------------
    # Три тонких линии-вреза поперёк папки: они исчезают там, где проходит
    # лезвие, и дают мгновенное «это про удаление» даже в 16 px.
    for i, (yy, hh) in enumerate(((0.56, 0.020), (0.63, 0.016), (0.70, 0.012))):
        y = int(s * yy)
        w = max(1, int(s * hh))
        fade = 1.0 - i * 0.22
        d.line(
            [(int(s * 0.30), y), (int(s * 0.74), y)],
            fill=(28, 32, 42, int(210 * fade)),
            width=w,
        )

    # --- лезвие ---------------------------------------------------------
    # Всё, что рисуется после плитки, обрезается по её маске: лезвие должно
    # выглядеть прорезанным сквозь плитку, а не наклеенным поверх неё.
    inner = Image.new("RGBA", (s, s), (0, 0, 0, 0))

    def tapered_blade(inset: float, color):
        """Полоса-лезвие: широкая у хвостовика (слева снизу), сходящаяся
        к острому концу (справа сверху)."""
        bd = ImageDraw.Draw(inner)
        x_tail_c, y_tail_c = s * (0.215 + inset), s * (0.785 - inset)
        x_tip_c, y_tip_c = s * (0.865 - inset), s * (0.135 + inset)
        half_tail = s * 0.085
        half_tip = s * 0.012
        dx, dy = x_tip_c - x_tail_c, y_tip_c - y_tail_c
        length = math.hypot(dx, dy)
        nx, ny = -dy / length, dx / length

        def pt(cx, cy, half, t):
            return (cx + nx * half, cy + ny * half) if t < 0.5 else (cx - nx * half, cy - ny * half)

        # Три точки хвоста дают форму «капли», а не обрубка по прямой.
        bd.polygon(
            [
                pt(x_tail_c, y_tail_c, half_tail, 0.0),
                pt(x_tip_c, y_tip_c, half_tip, 0.0),
                pt(x_tip_c, y_tip_c, half_tip, 1.0),
                (x_tail_c - nx * half_tail * 0.15 + nx * half_tail * 0.55,
                 y_tail_c - ny * half_tail * 0.15 + ny * half_tail * 0.55),
                pt(x_tail_c, y_tail_c, half_tail, 1.0),
            ],
            fill=color,
        )

    # Тень под лезвием.
    shadow = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    sd = ImageDraw.Draw(shadow)
    sd.polygon(
        [
            (s * 0.235, s * 0.805),
            (s * 0.855, s * 0.145),
            (s * 0.885, s * 0.175),
            (s * 0.265, s * 0.835),
        ],
        fill=(0, 0, 0, 110),
    )
    inner.alpha_composite(shadow)

    # Хвостовик — тёмная «рукоять», чтобы лезвие читалось как инструмент.
    bd = ImageDraw.Draw(inner)
    bd.polygon(
        [
            (s * 0.075, s * 0.815),
            (s * 0.225, s * 0.935),
            (s * 0.315, s * 0.845),
            (s * 0.165, s * 0.725),
        ],
        fill=(58, 64, 80, 255),
    )
    bd.line(
        [(s * 0.075, s * 0.815), (s * 0.225, s * 0.935)],
        fill=(96, 106, 128, 255),
        width=max(1, int(s * 0.016)),
    )

    tapered_blade(0.0, accent)
    # Скошенная фаска: тонкая светлая полоса вдоль верхней кромки.
    tapered_blade(0.010, BLADE_HI)

    clipped = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    clipped.paste(inner, (0, 0), Image.composite(mask, Image.new("L", (s, s), 0), mask))
    img.alpha_composite(clipped)

    return img.resize((MASTER, MASTER), Image.LANCZOS)


def export(name: str, accent: tuple[int, int, int]) -> None:
    master = draw_mark(accent)
    master.save(HERE / f"{name}.png")

    ico_master = master.copy()
    ico_master.save(
        HERE / f"{name}.ico",
        format="ICO",
        sizes=[(s, s) for s in SIZES],
        append_images=[],
    )
    print(f"{name}.png / {name}.ico  ({', '.join(str(s) for s in SIZES)})")


def main() -> None:
    export("icon-app", (255, 138, 60))
    export("icon-cli", (86, 196, 255))
    export("icon-shell", (255, 138, 60))


if __name__ == "__main__":
    main()