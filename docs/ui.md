# UI

## Design

- A Capture One-style workspace with a flow like IOPaint's (paint, one action, history). Design
  canvas (private): <https://claude.ai/artifact/EUfaeMAVhRFdEcP3J9EZRR>
- Layout: title bar; toolbar with file actions, cursor tools (brush, remove from mask, select
  object, pan), undo/redo, before/after, zoom, and "Save & Return" when opened by a host app;
  left tool rail (Erase, Fill, Extend, Upscale); viewer; right inspector; status bar.
- Every tool uses the same inspector, driven by the tool's declaration: name and input (mask,
  mask + prompt, canvas, whole image), collapsible setting sections, a Model section with "Runs
  on: This Mac | API" (API only for models too large to bundle), history, and one primary
  action. Adding a tool should not need new layout code.
- Neutral dark grays and a mid-gray viewer. One amber accent (#F2A541), only for the primary
  action and the active tool: switches, links, selected buttons and the Models page's rail icon
  use neutral colors. A badge shows whenever the API is in use.
- IBM Plex Sans / Mono (OFL), embedded in the app (`crates/gumka-app/assets/fonts`). Colors and
  styles are in `gumka-app/src/theme.rs`.
- Keyboard: egui's own keyboard UI zoom is off; ⌘+ / ⌘= / ⌘− / ⌘0 / ⌘1 zoom the photo.

## Logo

- `assets/logo/`:
  - `gumka-icon.svg`: the app icon on the macOS grid, an 824 px rounded square with continuous
    corners in a 1024 canvas (radius 185.4, 60 % smoothing). The app renders it with resvg for
    its Dock and window icon (`gumka-app/src/main.rs`).
  - `gumka-icon-small.svg`: for 16-32 px.
  - `gumka-mark.svg`: one color, `currentColor`.
  - `gumka-lockup.svg` / `gumka-lockup-dark.svg`: icon + wordmark, outlined from IBM Plex Sans
    SemiBold.
- Direction C on the design canvas: a dark G on amber whose bar is an eraser.
- The G is the same in every file: an arc of radius 232 around the center, stroke 112, round
  caps; the bar (sleeve and rubber tip) is as tall as the stroke and ends where the arc does.
  The small icon only raises the opening (50° above the bar instead of 40°) and drops the
  crumbs, gradients and shadow.
