# hoard-screen

Native, cross-platform in-game overlay: a **window-capture compositor** (think
OBS, but presenting *over* the game instead of recording). Runs as its own
process; the Tauri desktop app owns the editor and pushes layout over stdin
(newline-JSON, see [`ipc`]).

In **View** mode the surface is click-through — you only see the panels, all
input goes to the game. **Ctrl+O** (global hotkey) opens **Editor** mode, where
the surface captures input so you can place / scale / crop panels. Live content
keeps running in both modes; a captured video keeps playing with its own audio.
"Amplify" fills a panel's box and never the monitor — the content is composited
into the quad we draw, there is no toplevel to fullscreen.

## Layout

| module        | what                                                            | status |
|---------------|-----------------------------------------------------------------|--------|
| `scene`       | layout model + crop/scale geometry (`Fill`/`Fit`)               | done, tested |
| `source`      | `Source` trait, `Frame`, animated `TestPattern`                 | done, tested |
| `compositor`  | backend-free CPU compositing (alpha, z-order)                   | done, tested |
| `engine`      | owns live sources behind a scene; ticks + composites            | done, tested |
| `mode`        | View/Editor + Ctrl+O semantics                                  | done, tested |
| `ipc`         | newline-JSON protocol with the desktop app                      | done, tested |
| `capture`     | per-OS window capture behind one trait                          | see below |
| `runtime`     | on-screen overlay window (`feature = "runtime"`)                | see below |

### Per-OS native layers

| OS / server        | capture                                   | window placement                         | state |
|--------------------|-------------------------------------------|------------------------------------------|-------|
| Linux X11          | `GetImage` (XComposite fast-path TODO)    | ARGB32 override-redirect + SHAPE + XGrabKey | **implemented, compiles** |
| Linux Wayland      | `xdg-desktop-portal` ScreenCast + PipeWire| `wlr-layer-shell` overlay + `wl_shm` / gamescope | **implemented, compiles** (`ashpd`+`pipewire`+`smithay-client-toolkit`) |
| Windows            | `Windows.Graphics.Capture`                | `WS_EX_LAYERED\|WS_EX_TRANSPARENT` topmost | documented stub (`windows` crate) |
| macOS              | ScreenCaptureKit                          | borderless `NSWindow`, `ignoresMouseEvents` | documented stub (`objc2`) |

The Wayland backend (`--features wayland`) — both capture (portal + PipeWire)
and the `wlr-layer-shell` overlay surface — is implemented and compiles against
the real `ashpd`/`pipewire`/`smithay-client-toolkit` crates; it needs a live
Wayland session to exercise the portal dialog and the layer surface. On GNOME
(no layer-shell) run under **gamescope**. The Windows/macOS modules are written
against their real APIs but can't be built on the dev Linux box (no SDK); they
are gated off and each file documents the exact call sequence to finish there.

> **DRM caveat:** capturing protected streaming (Netflix, Prime Video, Disney+)
> yields **black frames** on every OS by design (HDCP / protected media path),
> exactly as OBS hits. YouTube, Twitch, native players, browsers, file managers
> capture fine. Backends flag likely-protected windows via `WindowInfo::protected`.

## Build / run

```sh
# Testable core (no display, no system libs) — runs anywhere incl. CI:
cargo test -p hoard-screen
cargo run  -p hoard-screen -- --snapshot frame.ppm --width 1280 --height 720

# On-screen overlay. X11 needs a compositing X server for alpha; Wayland needs
# wlr-layer-shell (wlroots/KDE/Cosmic) or gamescope on GNOME. Build both backends
# and the right one is chosen at runtime from the session:
cargo run -p hoard-screen --features "runtime wayland"
#   X11: Ctrl+O toggles View <-> Editor (global key grab).
#   Wayland: no global key grab — send `{"type":"set_editor","editor":true}`
#            over stdin (the desktop app owns the real Ctrl+O via the portal);
#            Ctrl+O / Esc drop back to View while the surface has focus.
#   Feed layout on stdin, e.g.:
#   echo '{"type":"set_scene","scene":{"panels":[
#     {"id":"v","source":{"kind":"window","id":"0x..."},
#      "rect":{"x":40,"y":40,"w":640,"h":360},"scale":"fill"}]}}' | \
#     cargo run -p hoard-screen --features "runtime wayland"
```

`--snapshot` flattens transparency onto a checkerboard so crop/scale geometry is
visible in the PPM.
