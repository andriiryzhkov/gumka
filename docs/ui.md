# UI

## Design

- A Capture One-style workspace with a flow like IOPaint's (paint, one action, history). Design
  canvas (private): <https://claude.ai/artifact/EUfaeMAVhRFdEcP3J9EZRR>
- Layout: title bar; toolbar with file actions, cursor tools (brush, remove from mask, select
  object), undo/redo, before/after, zoom, and "Save & Return" for a photo another app sent
  ([opening-and-saving.md](opening-and-saving.md#save--return)); left tool rail (Erase,
  Upscale; Extend later); viewer; right inspector; status bar.
- Every tool uses the same inspector, driven by the tool's declaration: name and input (mask,
  mask + prompt, canvas, whole image), collapsible setting sections, a Model section with "Runs
  on: This Mac | API" (API only for models too large to bundle), history, and one primary
  action. Adding a tool should not need new layout code.
- A tool's input decides the canvas: with a mask tool (Erase) the mask shows and the mask tools
  (brush, remove, select) work; with a whole-image tool (Upscale) the mask is hidden, kept as
  it is, and a drag only pans. The mask tools' keys (B, E, S, [ and ]) do nothing there, and
  the hint under the photo offers Space to pan and ⌘1 (100%). Esc clears the mask in a mask
  tool, and stops a running upscale.
- Panning is one gesture everywhere: hold Space and drag, in every tool (every hint says so).
  Two-finger scrolling and dragging with the middle or right button pan too. There is no pan
  tool and no H key.
- Upscale's inspector:
  - **Scale:** a slider from 1.25× to 4× in quarter steps, up to the largest factor whose
    result is within the size limit. The slider sets the scale chosen, which stays as it is;
    for a photo it doesn't fit, the slider, its label, the summary and the action show and use
    the largest that fits instead, and the chosen scale is back for the next photo, or after an
    undo. When not even 1.25× fits, the slider and the action are off and the inspector says
    the photo is already as large as Upscale can make it. A hint says whether the factor is the
    model's own output (2×, 4×) or made with the next model up and scaled down.
  - **The result's size**, and for very large results how much memory the photo can need while
    it is open, at most: its pixels, alpha and mask, the preview's and the mask overlay's
    textures once every part has been seen at 100%, and the photo as it was, kept for undo.
  - **A note when colors beyond P3 may be clipped:** the model sees the photo converted to
    P3, and its primaries are wider (ProPhoto, Rec. 2020) or, for a linear profile that
    converts through a table, can't be told ([color.md](color.md#the-models-color-space)). A
    linear profile that P3 holds (linear sRGB) gets none.
  - **Model:** RealPLKSR at that scale, loaded the first time it is used.
  - **The action**, "Upscale 2×". While it runs: "Upscaling 2×… 37%", with a neutral Cancel
    button. Cancel keeps the photo as it was.
  - The status bar shows the progress from any tool.
- Neutral dark grays and a mid-gray viewer. One amber accent (#F2A541), only for the primary
  action (the inspector's, and Save & Return in the toolbar for a sent photo) and the active
  tool: switches, links, selected buttons and the Models page's rail icon
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
