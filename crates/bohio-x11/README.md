# bohio-x11

The desktop a Bohío host serves, and the X11 backend that serves an X display.

- `Desktop`, `DesktopState`, `Frame`, `GuiError`, `Capabilities`: what a desktop backend implements and the GUI engine drives.
- `X11Desktop` over `X11Settings`: the screen of an X display (RandR monitors, `GetImage` capture, XTest input, a clipboard
  selection), in pure safe Rust over `x11rb`. It connects by display name, or over the stream a `Connector` hands it, so the
  display of another machine (an SSH channel) is served by the same code.
- `source::DesktopOf`: any `Desktop` as a `bohio_desktop` source of `bohio-api`.

`bohio` re-exports the types as `bohio::gui::*`. The `logind` feature (Linux) adds lock detection from logind's `LockedHint`.

MPL-2.0. Not published.
