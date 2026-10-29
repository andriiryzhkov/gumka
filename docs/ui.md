# UI

## Design

- A Capture One-style workspace with a flow like IOPaint's (paint, one action, history). Design
  canvas (private): <https://claude.ai/artifact/EUfaeMAVhRFdEcP3J9EZRR>
- Layout: title bar; toolbar with file actions, cursor tools (brush, remove from mask, select
  object), undo/redo, before/after, zoom, and "Save & Return" for a photo another app sent
  ([opening-and-saving.md](opening-and-saving.md#save--return)); left tool rail (Erase,
  Upscale; Extend later), with Config at its bottom; viewer; right inspector; status bar.
- The Config page, in place of the viewer and inspector: Gumka's version at the top
  (selectable, for bug reports), then the models and where they run. Esc or its rail button
  goes back to the editor.
- The title bar is the system's, except on Wayland desktops that leave it to apps (GNOME):
  there winit draws one in GNOME's Adwaita style (its `wayland-csd-adwaita` feature), close to
  GTK's but not following the theme or the button layout exactly.
- Dialogs ("Save changes?", and messages such as a file that won't open) are windows of their
  own, with the system's title bar and the app's style inside (`panels` in `app.rs`):
  - egui deferred viewports, which repaint on their own rather than with the main window, one
    per text (another message is another window), filled like a card and sized to their
    content (laid out once unseen, with a point to spare for pixel rounding). Where a window's
    scale lays the text out wider and it wraps onto more lines, the window grows to fit, so
    nothing is cut off.
  - A dialog's content area is centered on the main window's. The offset for its title bar is
    the dialog window's own once one has told it (kept for later dialogs), the main window's
    until then. On X11 and Windows a dialog that opens on a monitor of another scale moves into
    place once its window exists. From then on it stays where it is: dragging it, or moving
    the main window, doesn't pull it back.
  - Until one is answered the main window is dimmed and takes no clicks, and clicking or
    focusing it brings the dialog back to the front. Enter and Escape answer from either
    window: both are OK in a message; in "Save changes?" Escape is Cancel and Enter does
    nothing. Closing a dialog's window is Cancel, or OK for a message.
  - On macOS the main window's zoom button (the green one) is off meanwhile, so the main
    window can't go to a fullscreen Space without its dialog, or leave one with the dialog
    behind it. A dialog opened while the main window is fullscreen shows in that Space.
    Elsewhere maximizing leaves the dialog alone, and the button stays on.
  - A window that can't be created logs an error; Enter and Escape in the main window still
    answer its dialog.
- On Wayland too a dialog is a window of its own, with the title bar winit draws, but there the
  app can't bring it back to the front: winit's `focus_window` does nothing on Wayland, and
  winit 0.30 sets no parent window, so the compositor doesn't keep the dialog above the main
  window. If the main window is clicked and comes to the front, the dialog stays behind it
  until it is picked with Alt+Tab or in the overview (decided October 2026: a real window's
  title bar over drawing dialogs in the main window). Parent windows in winit and egui would
  fix it.
- A dialog's window asks for no minimize or maximize button and a fixed size. macOS and
  Windows show only close. On X11 it is a dialog (`_NET_WM_WINDOW_TYPE_DIALOG`): the window
  manager allows only moving and closing it, though GNOME's still draws a minimize button for
  a dialog with no parent window. On Wayland winit draws the title bar with the buttons the
  desktop's layout names (GNOME's setting `button-layout`) and takes no others: winit 0.30
  doesn't implement enabled buttons there, and a fixed-size window's maximize button is drawn
  but does nothing.
- A model found complete reads "Included" on macOS, where models come with the app, and
  "Downloaded" elsewhere (`worker::FOUND`).
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
  - **The action**, "Upscale 2×". While it runs, the same button, in the same place and size,
    says "Upscaling 2×… 37%" with a spinner, dimmed; under the pointer it lights up as "Stop
    upscaling · Esc", and a click (or Esc) stops it, keeping the photo as it was. The worker
    stops after the tile it is on, which takes up to about 10 s at 2× on a CPU, so the button
    says "Stopping…" at once (and the status bar "Stopping upscale…") and takes no more clicks
    until then. No Cancel button appears, so nothing in the inspector moves.
  - The status bar shows the progress from any tool.
- Neutral dark grays and a mid-gray viewer. One amber accent (#F2A541), only for the primary
  action (the inspector's, and Save & Return in the toolbar for a sent photo) and the selected
  rail button (the active tool, or the open Config page): switches, links and selected
  buttons elsewhere use neutral colors. A badge shows whenever the API is in use.
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
