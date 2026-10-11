#!/usr/bin/env python3
"""Export the shared RAZE icon. Run: python icons/export_icons.py"""
from __future__ import annotations
import io
import pathlib
import struct
from PIL import Image

HERE = pathlib.Path(__file__).resolve().parent
SOURCE = HERE / "source" / "mark-erase.png"
SIZES = (16, 20, 24, 32, 40, 48, 64, 96, 128, 256)

def load_mark(source: pathlib.Path = SOURCE) -> Image.Image:
    image = Image.open(source).convert("RGBA")
    # Ignore almost invisible alpha when measuring the generated transparent padding.
    bounds = image.getchannel("A").point(lambda a: 255 if a > 32 else 0).getbbox()
    if bounds is None:
        raise ValueError("The icon source is empty")
    left, top, right, bottom = bounds
    pad = 4
    return image.crop((max(0, left-pad), max(0, top-pad),
                       min(image.width, right+pad), min(image.height, bottom+pad)))

def render(mark: Image.Image, size: int) -> Image.Image:
    # Let tiny menu icons occupy more of their square without altering the mark.
    fill = 0.90 if size <= 16 else 0.88 if size <= 20 else 0.84 if size <= 32 else 0.82
    ss = 4
    side = size * ss
    extent = max(1, round(side * fill))
    scale = extent / max(mark.size)
    resized = mark.resize((max(1, round(mark.width * scale)),
                           max(1, round(mark.height * scale))), Image.Resampling.LANCZOS)
    canvas = Image.new("RGBA", (side, side), (0, 0, 0, 0))
    canvas.alpha_composite(resized, ((side-resized.width)//2, (side-resized.height)//2))
    return canvas.resize((size, size), Image.Resampling.LANCZOS)

def write_ico(path: pathlib.Path, frames: list[Image.Image]) -> None:
    # Store each separately sized PNG so small-frame optical sizing is retained.
    payloads = []
    for frame in frames:
        out = io.BytesIO()
        frame.save(out, format="PNG", optimize=True)
        payloads.append(out.getvalue())
    offset = 6 + 16 * len(frames)
    directory = []
    for frame, payload in zip(frames, payloads):
        dimension = 0 if frame.width == 256 else frame.width
        directory.append(struct.pack("<BBBBHHII", dimension, dimension, 0, 0,
                                     1, 32, len(payload), offset))
        offset += len(payload)
    path.write_bytes(struct.pack("<HHH", 0, 1, len(frames)) +
                     b"".join(directory) + b"".join(payloads))

def main() -> None:
    mark = load_mark()
    frames = [render(mark, size) for size in SIZES]
    frames[-1].save(HERE / "icon.png", optimize=True)
    write_ico(HERE / "icon.ico", frames)
    print("Shared icon.png / icon.ico: " + ", ".join(map(str, SIZES)))

if __name__ == "__main__":
    main()
