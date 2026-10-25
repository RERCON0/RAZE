# RAZE 0.1.4

- Center the final native window in the Windows monitor work area, independent of egui zoom.
- Increase the initial window size to 575 x 900 design points (about 678 x 1062 pixels at the pinned UI scale), clamped to the monitor work area.
- Remember the user's normal window size and maximized state across restarts. Minimizing or maximizing does not overwrite the normal size.
- Show the appropriate resize cursor on all window edges and corners.
- Disable Windows 11 corner rounding and the bright native border.
- Center the application window on startup, including after UI scale adjustment.

Windows x64 portable: includes GUI, CLI, shell extension, MSIX, certificate and licenses. Includes a signed manifest.

Dependency updates and remaining upstream notices: [review](https://github.com/RERCON0/RAZE/blob/main/docs/DEPENDENCY-REVIEW-2026-10-25.md).
