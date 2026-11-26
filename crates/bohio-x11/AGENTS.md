# `bohio-x11` — API header

Header-file-style reference for `bohio-x11`: the desktop trait, the X11 backend and a desktop as a `bohio-api` source. MSRV Rust 1.89.
`#![forbid(unsafe_code)]`. Depends on `bohio-api`, `bohio-proto` (the wire types of `bohio-gui/1`), `x11rb`, `xkeysym`, `thiserror`;
`zbus` only with feature `logind` (Linux). Version `0.0.0-alpha.0`, not published.

## Exports

- `Desktop` (trait): `state() -> DesktopState`, `capture(&DisplayId, Option<Rect>) -> Frame` (RGB, 3 bytes a pixel, rows from the top),
  `input(&[InputEvent]) -> u32`, `cursor()`, `clipboard_text()`, `set_clipboard_text()`, `capabilities()`. One call at a time.
- `GuiError { code: ErrorCode, message }` with `unavailable`, `unsupported`, `not_found`, `io`, `bad_request`; `Frame::new` checks its size.
- `X11Desktop::new(X11Settings)`: connects at the first call and again after a call that failed on the wire. `X11Settings` fields:
  `display` (a name for messages when `connector` is set), `wayland`/`in_wayland`, `session`/`system_bus`/`bus_wait` (logind, only
  with the feature), `key_wait`, `clipboard_wait`, `clipboard_max_bytes`, `selection`, `headless` (never locked, never asks logind),
  `refused`, `hint`, `max_repeat`, `connector`. `Default` is the node's own display settings with no display.
- `Connector` (trait, `Debug + Send + Sync`): `connect() -> io::Result<Wire>`, a new connection per call (the screen holds one and each
  clipboard question makes one). `Wire { stream: WireStream, auth_name, auth_data }` (`WireStream::Tcp(TcpStream)`, or on Unix `WireStream::Unix(UnixStream)`): a stream to the X server, already open, and the
  authorization the server wants (empty for none).
- `source::DesktopOf::new(Arc<dyn Desktop>)`: a `DesktopSource`; `source::{code_of, gui_error, rect_model, display_of, input_model}`: the
  conversions between the kind's models and the desktop's types.

## Gotchas

- Without `logind` a desktop never knows whether the screen is locked (`locked: None`, `lock_detection: false`) unless `headless`.
- A root window `GetImage` that answers `BadMatch` (XWayland) is composed from the viewable windows over black.
- XTest pointer events reach a window on XWayland only after a move onto it: send a `move`, wait, then the click.
