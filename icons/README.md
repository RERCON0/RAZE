# RAZE icon

One shared icon for the CLI, GUI, Explorer menu, and packaging.

- `icon.png`: transparent 256px runtime/README image.
- `icon.ico`: transparent Windows icon with 16, 20, 24, 32, 40, 48, 64, 96, 128 and 256px frames.
- `icon.rc`: shared resource ID 1 for both EXE files.
- `icon-shell.rc`: the same resource ID and ICO for the shell extension.

The selected concept is **03**, the two-piece eraser mark. White fills and charcoal contours are part of the transparent generated source. The square has no backing tile.

Regenerate with `python icons/export_icons.py` (Pillow required). Small frames occupy slightly more of the canvas for menu readability. The exporter preserves the generated artwork and only crops transparent padding, scales it, and writes PNG/ICO files.

Original concept, transparent master and exact prompts are in `source/`. Artwork was created with the built-in image generation tool.

