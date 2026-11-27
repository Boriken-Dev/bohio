# `bohio` — API header

Header-file-style reference for the `bohio` crate: every public item with its signature, contract and gotchas, so the
crate can be used without reading its source. It ships inside the package and is self-contained. Development documentation lives
with the source at <https://github.com/Boriken-Dev/bohio>.

| Feature | Adds | Needed for | Default |
| --- | --- | --- | --- |
| `cli` | module `cli`, the `bohio` binary; `clap`, `tracing-subscriber`, `serde_`, `crossterm`, and `notify` | the command | on |
| `gui` | snapshot encoding (`image`: PNG, JPEG; `image-webp`: lossless WebP), and a watch's tiles and rectangle encodings (`bohio-transfer/screen`); without it a snapshot and a watch are `unsupported` | `bohio-gui/1` | on |
| `gui-wayland` | on Linux, `gui::{WaylandDesktop, WaylandSettings, Persist}`: a Wayland session through `xdg-desktop-portal` (`ashpd`, `pollster`) and a PipeWire stream (`pipewire`); it links the system libpipewire-0.3 (pkg-config, libclang), so glibc builds only, and it does nothing off Linux | a Wayland desktop | off |
| `ssh` | `bohio-provider-ssh`: the `ssh` provider (`[source.NAME] provider = "ssh"`): a machine reached over SSH as terminal and exec sources, registered with the built-in providers | the machine's terminal and processes | on (in `default`, not implied by `cli`) |
| `serial` | `bohio-provider-serial`: the `serial` terminal provider (`[source.NAME] provider = "serial"`), registered with the built-in providers (a build without it can load the same provider as a library, `[providers] paths`) | a serial console | on (in `default`, not implied by `cli`) |
| `notify` | `gui::DesktopNotifier` (`notify-rust`) | the desktop notice of the presence indicator | on with `cli` |

MSRV Rust 1.89. A library-only build (`default-features = false`) depends on `bohio-proto`, `bohio-client` (the link and its relay),
`coqui-transport`, `cabuya` (with `sealed`), `coqui-peer`, `coqui-transport-pipe` and `coqui-transport-web` (each a git revision; one copy of each builds), `serde`, `toml`,
`thiserror`, `tracing`, `anycfg` (the layers) and `toml_edit` (in-place edits of a peer table), `portable-pty` (terminals), `cap-std` and `cap-fs-ext` (confined file access), `bohio-transfer` (digests), and `rustix` (Unix) or
`win32job` (Windows). Version `0.0.0-alpha.0`, not published.

Imports: the root holds `Config`, `ConfigError`, `Environment`, `Grant`, `Peers`, `Permission`. Public modules:
`access`, `config`, `control`, `exec`, `forward`, `fs`, `gui`, `info`, `limit`, `link`, `listen`, `local`, `media`, `node`, `pair`, `peer`, `policy`, `power`, `secret`, `session`, `shell`, `cli` (feature `cli`). `unsafe_code` is denied crate-wide, forbidden in every module that needs none, and allowed only in the seven files of the table below, whose reasons `tests/unsafe_files.rs` records. The library prints nothing, reads no environment variable and installs no logger; it starts threads only inside
`exec::serve`, `shell::serve`, the transfer and control engines, `node::Node` (a thread per connection, per stream and per relay) and
`pair::Armed` waits and the pairing a node answers.

| File with `unsafe` | What it does |
| --- | --- |
| `platform/guard.rs` | the Unix guardian (fork) |
| `platform/winproc.rs` | CreateProcessW with handle and job lists, console events |
| `platform/winacl.rs` | the security API: an access list read for the owner check, and `ReplaceFileW` keeping a replaced file's own list |
| `platform/winfs.rs` | NT handle-relative filesystem calls (its buffers are built in `platform/winfs_layout.rs`, which forbids `unsafe`) |
| `platform/gui/windows.rs` | SendInput, session, clipboard |
| `platform/gui/windows_capture.rs` | DXGI and GDI |
| `platform/gui/macos.rs` | ScreenCaptureKit and Core Graphics |

## Module headers

Each module's API header sits beside its code and ships in the package with this one.

| Header | Covers |
| --- | --- |
| `src/config/AGENTS.md` | `bohio.cfg`: every setting and default, the layers, `bohio::profile`, and the peer grants (`bohio::policy`, `[peer.<id>]`) |
| `src/access/AGENTS.md` | sources and the providers behind them (`[source.<name>]`, `[access]`), power, media, secrets and the console |
| `src/exec/AGENTS.md` | `bohio-exec/1`: running a program in a tree of its own |
| `src/shell/AGENTS.md` | `bohio-shell/1`: a program on a terminal |
| `src/session/AGENTS.md` | detached sessions and `bohio-attach/1` |
| `src/fs/AGENTS.md` | files: the roots, transfers, trees and bundles, search and follow |
| `src/forward/AGENTS.md` | `bohio-connect/1`, `bohio-listen/1` and `bohio-accept/1`, the rules, and the forwards a peer's table keeps up |
| `src/gui/AGENTS.md` | the desktop: `bohio-gui/1`, the backends, the per-user helper and the helper on another machine |
| `src/cli/AGENTS.md` | the `bohio` command |
| `src/AGENTS.md` | the single-file modules: `control`, `info`, `limit`, `node`, `link`, `peer`, `pair`, `listen`, `local`, `mcp` |

Exit status: 0 success, 1 the operation failed (one line on standard error), 2 the invocation was wrong; `remote exec` exits with the program's.

## Environment variables

| Name | Effect |
| --- | --- |
| `BOHIO_CONFIG` | the one configuration layer when `--config` is not given; must exist when set |
| `BOHIO_LOG` | log level or filter for standard error, default `warn` (`trace` shows every link event; none holds a secret) |
| `NO_COLOR` | set: no colour in log lines |
| `PATH` | where shells are looked for in `info`, and the programs a grant lists by `exec` and `shell` |
| `APPDATA`, `LOCALAPPDATA`, `ProgramData` (Windows); `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `HOME` (Unix) | where the user's and the system's configuration files and the data directory are looked for |

## Gotchas

- Nothing logs a key, a PIN or a request: link events carry a peer id and a protocol name, and `bohio pair` writes the PIN to standard output
  only. `tests/link.rs` captures every process's log at `trace` and fails when a PIN or any stored key bytes appear in it. `bohio` has no
  invitation flow, so there is no invitation secret.
- A peer id is public; `bohio peer list` and the log print them. A key that proves itself and is not accepted is recorded under the connection's source
  address, so a flood from one source cannot fill the waiting list for the others.
- A revoked peer's link ends at its key check (`[link] key_recheck`, `2 x recheck` unset) at the latest; its streams end at the first re-check or at that check, whichever comes first, and either way hear a revoke.
- `bohio serve` stops only when killed: the exec trees it started are ended when the stream drops, and on Windows by the job object when the
  process dies; on Unix a killed host leaves nothing to end them.
- A loopback connect to a closed port in the ephemeral range can connect a socket to itself; `Connection::connect_tcp` and `bohio join` refuse one.
