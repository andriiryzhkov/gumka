# Opening and saving

## Standalone and host apps

- Standalone: the user opens a file (open dialog or drag-and-drop) and saves the result (Save /
  Save As). It must work without any host app.
- Host apps: any RAW editor with an "Edit With" / external-editor feature (macOS and Windows),
  which renders a TIFF, opens it in Gumka, and picks up the result when the file is saved in
  place. Capture One is the first one tested; on Linux the targets are the darktable and digiKam
  external editors, or standalone use.
- The app names no host app or other product (they are trademarks, and any editor works the same
  way): the start screen speaks of "your photo editor".
- Recommended export settings for the host: TIFF, 16-bit, Adobe RGB or Display P3. ProPhoto
  works too (converted for the model, see [color.md](color.md)), but is wider than needed.

## How files arrive

- macOS: the file arrives as an Apple "open documents" event, NOT as a command-line argument
  (the same for Finder "Open With", dropping on the Dock icon and a RAW editor's Edit With).
  winit (under eframe) doesn't handle `application:openURLs:`, so `gumka-app/src/open_files.rs`
  adds it to winit's app delegate at runtime. A file that starts Gumka arrives while it is still
  launching, before the app is created, so `main.rs` builds the event loop itself (which makes
  winit's delegate), installs the handler, and only then runs eframe on it
  (`eframe::create_native`). The file waits until the app takes it, which opens it like the
  open panel does: asking about unsaved changes, waiting for a running erase, and waiting while
  a dialog is up. Of several files in one event the first is opened (one photo at a time).
- Info.plist declares TIFF, PNG and JPEG (role Editor, rank Alternate), so Gumka is offered under
  Open With and Edit With without becoming the default app for those files.
- Windows / Linux: the path arrives as a command-line argument.

## Save & Return

- A photo another app sent (the open-documents event on macOS, the command-line path elsewhere)
  gets a "Save & Return" button at the right of the toolbar, the toolbar's primary action.
  Photos opened with the open panel or dropped on the window don't. The app keeps the sent path
  until it opens a file (`GumkaApp::sent`), and marks the document only if that file is the one
  sent (`Document::sent`); a sent file that fails to open doesn't mark the next one.
- It saves in place and quits; macOS then brings back the app that sent the photo, which picks
  up the saved file. Without unsaved changes it just quits. During an erase it waits for the
  pixels, then saves and quits. A failed save says why and Gumka stays.
- Saving as another file ends it: the button goes away, since the sending app waits for the
  original. While a save waits for the pixels the button is off, so a waiting Save As stays the
  save that happens.

## Quitting

- Quitting (⌘Q, the app and Dock menus) asks "Save changes?" like closing the window.
  `gumka-app/src/quit.rs` adds `applicationShouldTerminate:` to winit's app delegate (installed
  when the app is created, which is early enough for quitting). With unsaved changes it cancels
  the quit, brings the window back and asks.
- A logout with unsaved changes is cancelled too; the user logs out again after answering.
- The question appears at once even while an erase runs: Don't Save quits immediately, Save
  saves once the erase is back, then quits. If a save is already waiting, the first quit waits
  for it without asking, and a second one asks.

## Saving

- Every save preserves bit depth (JPEG is 8-bit), gray or color, the ICC profile and the
  metadata, into whichever format it is saved as ([image-io.md](image-io.md)).
- JPEG output is re-encoded (quality 95), so a JPEG's untouched pixels don't stay bit-identical
  on disk; TIFF and PNG output keep them exactly.
- A save writes a temporary file next to the target (created with the original's permissions),
  syncs it to the disk and renames it over the original, keeping its permissions. A save through
  a symlink replaces the file it points to. If anything fails, the temporary file is removed and
  the original is untouched. Not kept yet: extended attributes, Finder tags, creation date.
- As in Preview, a file the user can't write isn't saved over ("<name> is read-only"), and a
  folder the user can't write in says so ("Gumka can't save in <folder>"); Save As to another
  name or folder still works.
- Save As keeps the standard macOS meaning: the document continues under the new name.
- Save is disabled while the document has no unsaved changes.
- Opening, saving and quitting while the worker has the pixels (an erase, or encoding for
  selection) wait for them instead of being refused ([architecture.md](architecture.md#threads)).

## Orientation

EXIF / TIFF orientation (1-8) is applied only when displaying: pixels, tags and file layout stay
as stored, and the canvas maps brush strokes back to stored coordinates
(`gumka_core::picture::viewed_to_stored`). CLI masks and clicks are in stored pixels too.
