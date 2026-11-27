# `bohio::gui` — API header

Part of the `bohio` API header (`../../AGENTS.md` in the crate root): its features, imports and environment variables are there.

## Desktop (`bohio::gui`, `[gui]`)

```rust
pub trait Desktop: Send + Sync {                                   // what a backend, or an application, implements; the engine calls it one call at a time
    fn state(&self) -> Result<DesktopState, GuiError>;             // DesktopState { user, active, locked: Option<bool>, displays }
    fn capture(&self, display: &DisplayId, region: Option<Rect>) -> Result<Frame, GuiError>;   // Frame: RGB, 3 bytes a pixel, rows from the top (`Frame::new(w, h, rgb)`)
    fn input(&self, events: &[InputEvent]) -> Result<u32, GuiError>;   // how many it took; a `wait` never arrives (the engine pauses)
    fn cursor(&self) -> Option<(DisplayId, i32, i32)>;
    fn clipboard_text(&self) -> Result<String, GuiError>;   fn set_clipboard_text(&self, text: &str) -> Result<(), GuiError>;
    fn capabilities(&self) -> Capabilities { .. }                  // provided: Capabilities { backend, input, clipboard, lock_detection }
}
pub struct GuiError { pub code: ErrorCode, pub message: String }   // unavailable(..), unsupported(..), not_found(..), io(..), bad_request(..); the message reaches the peer: no typed text, no clipboard
impl Node { pub fn with_sources(self, Sources) -> Self; pub fn sources(&self) -> Arc<Sources>; pub fn with_secrets(self, Arc<dyn SecretResolver>) -> Result<Self, NodeError>;   // sources (see `bohio::access`): call with_sources first
            pub fn with_desktop(self, Arc<dyn Desktop>) -> Self; pub fn with_presence(self, Arc<dyn Presence>) -> Self; pub fn with_notifier(self, Arc<dyn Notifier>) -> Self; pub fn gui_approvals(&self) -> Arc<Approvals>; }
pub trait Presence: Send + Sync { fn engaged(&self, &Engagement); fn released(&self, &Engagement); fn approve(&self, &ApprovalRequest<'_>) -> Approval; }   // default: LocalPresence over a Notifier and Approvals
pub struct Engagement { pub peer: PeerId, pub session: String, pub controlling: bool }
pub trait Notifier: Send + Sync { fn notify(&self, notice: &Notice, decide: Option<Decide>); }   // LogNotifier (default), DesktopNotifier (feature `notify`); Notice { title, body }
pub fn serve(channel: Arc<dyn Channel>, metadata: &[u8], context: &Context<'_>) -> Result<Ended, ErrorBody>;   // `bohio-gui/1`; Ended: counts only
pub fn serve_chosen(channel: &Arc<dyn Channel>, metadata: &[u8], context: &Context<'_>);   // a node's open: its own desktop, or a helper's (`Context.session` names the session served; `DEFAULT_SESSION` is the node's own)
pub fn covers(grant: &Grant, metadata: &[u8]) -> bool;
```

`[gui]` (`GuiConfig`; every key in `examples/bohio.cfg`): `enabled` `false`, `max_streams` `4`, `max_streams_per_peer` `2`, `snapshots_per_minute` `120`, `max_snapshot_pixels` `33554432`,
`formats` `["png","jpeg","webp"]`, `jpeg_quality` `80`, `max_input_rate` `1000`, `max_batch_events` `256`, `max_text` `4096`, `max_batch_wait` `5s`, `input_when_locked` `false`, `input_when_lock_unknown` `false` (input is refused while `locked` is unknown), `settle_interval` `100ms`,
`settle_quiet` `300ms`, `settle_timeout` `5s`, `max_settle_wait` `30s`, `settle_ignore_tiles` `1`, `clipboard_max_bytes` `1048576`, `chunk_size` `65536`, `poll_interval` `50ms`, `state_interval` `500ms`, `idle_timeout` `30m`,
`indicator` `"notify"`, `indicator_linger` `30s`, `approval` `"none"`, `approval_timeout` `60s`, `approval_ttl` `15m`, `approval_asks_per_minute` `3`, `helper_accounts` empty, `helper_endpoint` and `node_account` unset (these three: lowest layer only), `max_helpers` `8`; the watch's `default_fps` `5`, `max_fps` `15`, `min_fps` `1`, `tile` `64`, `frames_in_flight` `2`,
`lossless_max_bpp` `0.75` (a float: `GuiConfig` is not `Eq`), `refine_after` `1s`, `zstd_level` `3`, `watch_quality` `90`, `adapt` `true`, `adapt_ladder` `["85","70","50","50@0.75","40@0.5"]` (`LadderStep::parse`: `QUALITY[@SCALE]`),
`adapt_window` `32`, `adapt_down_after` `3`, `adapt_up_after` `5`, `adapt_late` `1.5`, `adapt_early` `0.5`, `max_rate` unset (bytes a second), `max_rect_bytes` `33554432`; a zero count or time is an error naming the key.
A `watch` (`gui/stream.rs`, `gui/adapt.rs`): the stream thread captures only while fewer than `frames_in_flight` updates are unacknowledged and at most `fps` times a second, compares the frame in `tile` tiles
(`bohio_transfer::screen::TileGrid`), sends the changed rectangles (`zstd_rgb` where it costs at most `lossless_max_bpp` bytes a pixel, else JPEG; a JPEG rectangle that stays unchanged for `refine_after` is sent
again lossless), and moves along the ladder by how late each `ack` was, judged by its excess over the least delay of the last `adapt_window` acks (`adapt_down_after` late in a row step down, `adapt_up_after` early ones step up; a lossless watch lowers only the rate). The grant is read again
every `state_interval`. Without feature `gui` a watch is `unsupported`. `examples/gui_host.rs` serves a synthetic desktop (`examples/support/synthetic.rs`: `text` or `motion`) over TCP, for measuring a watch with the built command. A node serves a stream only with `enabled`, a desktop
(`with_desktop` adds it as the source `app` and makes it the default desktop; the `native` source serves the platform desktop through `[access] desktop`: Windows, Linux over X11 (or, with feature `gui-wayland`, Wayland) and macOS) and a peer whose grant holds what the request wants (`gui.clipboard` only beside `gui.view` or `gui.input`). Each request is decided by the grant
as it is then; the stream ends `revoked` at the next `[link] recheck`. An engagement (one peer, one session) starts at its first stream and ends `indicator_linger` after its last, so many short streams make one notice.
`bohio-gui-approve/1` is served on the local endpoint only (`LocalLink::gui_pending`, `gui_decide`; `bohio gui pending|allow|deny`).

A helper on another machine (`gui::helper_exec`, `bohio gui-helper --stdio`, `[gui] source` as `NAME` or `NAME=COMMAND` and `source_retry` `10s`, both lowest layer only; `ExecHelper::list` makes the helpers, the default command being `bohio gui-helper --stdio`, or `{helper} gui-helper --stdio` when `helper_upload` names the source): `Node::serve_exec_helpers(&[ExecHelper], retry, stop)` starts the command with the exec source, wraps the process's standard output and input as a length-prefixed channel (`bohio_client::link::stream` over `Duplex`) and calls `serve_helper_channel` with the account `exec:SOURCE`; the helper's standard error is drained to the debug log (an unread queue would stop its output). The helper side is `helper::serve_node` over `bohio_client::link::stdio`: nothing else may write to its standard output, and no account is checked, the login being what authenticates it. Tests: `tests/gui_helper_exec.rs` (the painted helper through an in-process ssh server; the real helper on the machine's X display, Linux).

Putting the helper on the machine (`gui::helper_upload`; `[gui] helper_upload`, `helper_binary`, `helper_upload_overwrite`, `helper_upload_shared_dir`, `helper_upload_check_platform`, `helper_upload_answer_max` (`4096`), all lowest layer only; `ExecHelper::list(&GuiConfig)` builds the helpers from them and validates them): not an ssh feature. The copy goes through the files kind (`SOURCE=DIR`, or `SOURCE@FILES=DIR` for a `bohio_files` source FILES other than SOURCE) and the command runs through the exec source; `{helper}` (`HELPER_WORD`) in a `[gui] source` command is where the file went (the default command of a source with an upload names it), and a command that names it needs an upload for its source. `helper_upload::upload(&Sources, source, &Upload, &stop) -> Result<path, String>` runs at every start of the helper: the file's header (ELF, Mach-O or PE; `built_for`) is compared with `uname -sm` run through the exec source (`check_platform`, default on; a machine with no `uname` turns it off), the directory must not be writable by group or other (`shared_dir`), the name is `bohio-helper-<16 hex of the SHA-256>` (plus `.exe` when the source file has it), a file of that name with the same digest is reused and any other is refused unless `overwrite`, and a new file is written as a mode 0700 `.part` and renamed. A weakened check is logged `non-default:`. The binary defaults to the node's own executable. The directory must exist. Tests: `tests/gui_helper_exec.rs` (the node's own files and exec, the in-process ssh server, a private `sshd` on Linux; the platform and directory tests on Unix). A login through `sshd` has no `XDG_RUNTIME_DIR`: `gui-helper --stdio` needs none.

The per-user GUI helper (`bohio::gui::helper`, `bohio gui-helper`, `[gui] helper_accounts`, `helper_endpoint`, `refused_log_interval` (`1m`), `node_account`, `max_helpers`, `helper_socket_dir_check` (`true`: on Unix a socket directory, or one above it, that another account can replace is refused; `false` is logged `non-default:`); the first three and `helper_socket_dir_check` are read from the lowest layer only): a node with no desktop of its own serves a
desktop user's, which that user's helper lends over `bohio-gui-helper/1` on the helper endpoint (a mux with no proof: the account is the boundary).
```rust
pub struct HelperListener;   // Debug; bind(&GuiConfig, &PeerId) -> Result<Option<Self>, NodeError> (None when helper_accounts is empty; resolves each account; Unix: refuses a socket directory another account owns or may write; Windows: DACL SY, the node's SID, IU)
impl HelperListener { pub const fn endpoint(&self) -> &Endpoint; }
impl Node { pub fn serve_helpers(&self, &HelperListener, stop: &AtomicBool) -> io::Result<()>;   // closes unread a connection whose OsIdentity user is not a listed account (a missing answer refuses): GuiChange::Refused on the node's own id
            pub fn serve_helper_channel<C: Channel>(&self, &C, account: &str, stop: Option<&AtomicBool>); }   // the account is the caller's check
pub struct HelperSettings { pub endpoint: Endpoint, pub node_account: String, pub session: String, pub user: Option<String>, pub gui: GuiConfig /*the user's: narrowed by the node's, never widened*/, pub link: Settings }
pub fn run(&HelperSettings, Arc<Sources>, stop: &AtomicBool) -> Result<(), HelperError>;   // checks who serves the endpoint BEFORE it sends (HelperError::NotTheNode names both), then serve_node
pub fn serve_node<C: Channel>(&C, &HelperSettings, Arc<Sources>, stop: &AtomicBool);   // the caller has checked the node
pub fn resolve_account(&str) -> Result<String, String>;   // a SID (Windows; names through Windows PowerShell) or a uid (Unix; names through /etc/passwd)
pub fn admits(user: Option<&str>, accounts: &[String]) -> bool;   pub fn narrowed(&GuiConfig, &HelperPolicy) -> GuiConfig;
```
A request that names a helper's session (`ACCOUNT:NAME`), or none while the node's own desktop does not answer, is relayed by `gui::serve_chosen`; the node checks the grant, the slots, its approval and each input and clipboard frame, and the helper's engine
(`Context.session` is the listed id) serves exactly what the node granted. `bohio gui-helper [--endpoint E | --node ID] [--node-account A] [--session NAME]` exits 1, naming both accounts, when another account serves the endpoint.
`tests/gui_helper.rs` runs a node and a helper over a real pipe against the painted desktop; `tests/accounts.rs` has the real second-account cases (`BOHIO_TEST_OTHER_ACCOUNT`).

```rust
// Windows only (`cfg(windows)`), `bohio::gui`: the screen of the interactive session this process runs in.
pub struct WindowsDesktop;   // impl Desktop; `new(WindowsSettings)` sets the process to per-monitor DPI awareness v2 (once, process-wide), so every coordinate is a physical pixel
pub struct WindowsSettings { pub capture: WindowsCapture, pub frame_wait: Duration, pub capture_idle: Duration, pub clipboard_wait: Duration, pub clipboard_max_bytes: usize, pub max_repeat: u32 }   // From<&GuiConfig>, Default; read once at start, not live
```

`WindowsDesktop`: displays from `EnumDisplayMonitors` (id `display1`, `display2`, ... from the device name, `scale` is the effective DPI over 96, `rotation` from the current display settings); the session is in
`state` (`user`, `active` from the terminal service's connect state, `locked` when the input desktop is not `Default` or cannot be opened); session 0 is `unavailable` ("run the node in the user's session, or use the
GUI helper"). `capture` takes DXGI duplication (one per display, on a thread of its own, kept `windows_capture_idle` after the last capture; a still display is read from the copy of the last frame) and falls back to GDI when
`windows_capture = "auto"` and DXGI cannot serve; no capture or input while the session is not active or the secure desktop has the screen (`unavailable`, naming it). `input` plans every event before it sends one (a point
outside its display is `bad_request` and nothing is sent), sends one event's operations in one `SendInput`, and returns how many events were taken (none taken: `unavailable`, "input was blocked"). The pure rules live in
`platform/gui/input.rs` (virtual-screen normalisation, the virtual-key table, text as strokes) and `session.rs` (what the session's answers mean, the clipboard text's CR LF encoding), tested on every platform.
`tests/gui_windows.rs` makes its own window (`tests/support/gui_probe.ps1`) and drives only it, and only with `BOHIO_TEST_GUI_LIVE=1`.

Linux only (`cfg(target_os = "linux")`), `bohio::gui`: the screen of an X display.

```rust
pub struct X11Desktop;   // impl Desktop; `new(X11Settings)` connects at the first call (and again after a call that failed on the wire)
pub struct X11Settings { pub display: Option<String>, pub wayland: bool, pub in_wayland: bool, pub session: Option<String>, pub system_bus: Option<String>,
    pub bus_wait: Duration, pub key_wait: Duration, pub clipboard_wait: Duration, pub clipboard_max_bytes: usize, pub selection: String, pub max_repeat: u32,
    pub headless: bool, pub lock_unknown: bool, pub refused: Option<String>, pub hint: Option<String>, pub connector: Option<Arc<dyn Connector>> }   // Default; read once at start; headless: never locked, no logind; lock_unknown: another machine's display, `locked` is `None`; connector: how to reach an X server that is not a display of this machine; refused: every call is `unavailable` with it; hint: appended to a display that does not answer
```

`X11Desktop` (the `bohio-x11` crate: `x11.rs`, `x11_clipboard.rs`, `logind.rs` behind its `logind` feature, which `bohio` turns on; all safe code, re-exported as `bohio::gui::X11Desktop`): displays are RandR 1.5 monitors (id from the monitor's name, lowercased: `rdp-0`; `display1` for a root with none; one is primary); `capture` is `GetImage`
of the root at the monitor's offset converted from the visual's masks (16, 24 and 32 bits a pixel), and where the root answers `BadMatch` (XWayland's) the viewable top-level windows composed bottom to top over black; the
pointer is reported by `cursor` and never drawn. `input` plans every event before it sends one (a point off its display or a repeat past `max_repeat` is `bad_request` and nothing is sent) and sends XTest `FakeInput` on the
root: buttons 1-3, wheel 4-7, back 8, forward 9; a key is looked up in the keyboard mapping (its first symbol, or its second with shift pressed around it) and a symbol with no key is bound to the highest keycode that has none for
`key_wait`, typed and unbound. `state.locked` is logind's `LockedHint` of `session` (else of this process's session): `None` and `lock_detection: false` when logind is not reachable. `wayland` without `in_wayland` is
`unavailable` before any connection is tried. The clipboard is a selection this desktop owns from a thread with a connection of its own (TARGETS, UTF8_STRING, TEXT, STRING); a read asks the owner over a connection made for it, and
a transfer that needs INCR is `unavailable`. The pure rules (layout arithmetic, pixel formats, the keyboard plan, composing windows) are unit-tested without a server. `tests/gui_x11.rs` starts its own `Xvfb` (`-displayfd`), or uses
`BOHIO_TEST_X11_DISPLAY`, and drives only the window `examples/gui_probe_x11.rs` makes; both need `BOHIO_TEST_GUI_LIVE=1`, and the probe is built first (`cargo build -p bohio --example gui_probe_x11`).

A service account's own desktop (Linux; `platform/gui/own.rs`, safe code; `bohio::gui::{OwnSession, OwnServer, own_uid, own_runtime_dir, own_display, check_runtime_dir}`): `[source.native] own_session = "none" | "x11" | "wayland"` (default `none`) and `own_runtime_dir` (default `/run/user/UID`)
make the node serve the display its account's user units run (`packaging/systemd/own-desktop/`: headless sway and a rootful Xwayland on `:UID`), whatever session the command was started in (the command's `DISPLAY` is not
read). The uid is the process's own (`/proc/self`); the X server is reached on its abstract socket by `OwnServer`, which refuses a server whose `SO_PEERCRED` uid is not the node's (`own_session_check_server = false` serves it, logged `non-default:`); `check_runtime_dir(dir, uid, check_owner)` refuses, naming the path, a directory that is a link and, with `check_owner` (`own_runtime_dir_check`, default `true`; `false` is logged `non-default:`), one that is not the uid's or not mode 0700 (every call of the desktop is then `unavailable` with that
text), an explicit `x11_display` wins over `:UID`, and `locked` is `false` with `lock_detection: false` (no lock screen). `wayland` serves the compositor through the portals (feature `gui-wayland`; the process needs
`XDG_RUNTIME_DIR` and `DBUS_SESSION_BUS_ADDRESS` of the account, which the system unit sets), and without the feature it is `unavailable` naming it. A display that does not answer is `unavailable` naming the display and the
units to look at. `tests/gui_own_desktop.rs`: the refusals need no desktop; the live test needs `tools/own_desktop_units.sh run cargo test -p bohio --test gui_own_desktop` (the shipped user units installed under a name of their own in the running
account's `~/.config/systemd/user`, removed afterwards; `verify` runs `systemd-analyze verify` on every unit).

Linux with feature `gui-wayland`, `bohio::gui`: the screen of a Wayland session, through the desktop portals.

```rust
pub struct WaylandDesktop;   // impl Desktop; `new(WaylandSettings)` asks the portal at the first call (and again after the session ends)
pub struct WaylandSettings { pub persist: Persist, pub restore_file: Option<PathBuf>, pub capture_wait: Duration, pub start_wait: Duration, pub retry: Duration,
    pub idle: Duration, pub session: Option<String>, pub system_bus: Option<String>, pub bus_wait: Duration, pub headless: bool, pub max_repeat: u32 }   // Default: Node, none, 5s, 10s, 30s, 60s, none, none, 1s, false (headless: never locked, no logind), 1000
pub enum Persist { None, Node /* default */, Always }   // `parse("none" | "node" | "always")`
```

`WaylandDesktop` (`platform/gui/wayland.rs`, safe code; the `native` source builds it instead of `X11Desktop` when the session is Wayland and `x11_in_wayland` is not set): one RemoteDesktop session with ScreenCast monitor sources, or a
ScreenCast session alone where the portal has no RemoteDesktop (`xdg-desktop-portal-wlr`), which is a view: `capabilities().input` is `false` and `input` is `unsupported`. The first call of any kind (`info` included) asks the portal, whose own
dialog is the system's approval; a call waits `start_wait` for it and then reports `unavailable` while the session goes on starting, and a refused or failed start is reported for `retry` before the portal is asked again. One PipeWire
stream for each monitor is read on a thread of its own and keeps the latest picture; a display is a monitor, its size the pixels the compositor negotiated and its scale that over the portal's logical size, and `capture` waits up to
`capture_wait` for a monitor's first picture (a compositor may send one only when the screen changes: `xdg-desktop-portal-wlr` does, measured). Input is `notify_pointer_motion_absolute` (pixels divided by the scale, on the monitor's
stream node), `notify_pointer_button` with evdev codes (`BTN_LEFT` 0x110 to `BTN_EXTRA` 0x114), `notify_pointer_axis_discrete` (positive scrolls down or right) and `notify_keyboard_keysym`; every event is planned before one is sent. The cursor
is hidden in the pictures and `cursor` is the last point this desktop moved to; there is no clipboard (`unsupported`). With `persist = Always` the portal's restore token is kept in `restore_file` (`0600`). `tests/gui_wayland.rs` runs against a
headless sway through `tools/wayland_headless.sh` (needs `sway`, `pipewire`, `wireplumber`, `xdg-desktop-portal`, `xdg-desktop-portal-wlr`; prints `SKIPPED` without `BOHIO_TEST_WAYLAND=1`):
`tools/wayland_headless.sh cargo test -p bohio --features gui-wayland --test gui_wayland -- --test-threads=1`. The harness's `BOHIO_TEST_WAYLAND_POKE` repaints the background so the first picture exists.

```rust
pub struct MacosDesktop;   // impl Desktop; `new(MacosSettings)` asks the system nothing until the first call
pub struct MacosSettings { pub capture_wait: Duration, pub clipboard_max_bytes: usize, pub max_repeat: u32 }   // Default (10 s, 1 MiB, 1000)
```

`MacosDesktop` (`platform/gui/macos.rs`, the unsafe file, and `mac_rules.rs`, the rules, pure and tested on every platform; macOS only): displays come from Core Graphics (id `display` and the `CGDirectDisplayID`; `x`, `y` are the
display's place in the layout in points, `width`, `height` backing pixels, `scale` pixels a point; `CGDisplayIsMain` is the primary one). The session state is `CGSessionCopyCurrentDictionary` (`kCGSSessionOnConsoleKey`,
`CGSSessionScreenIsLocked`); no dictionary (an ssh login, a LaunchDaemon, no one logged in at the screen) is `unavailable` with how to get a session. `capture` asks `CGPreflightScreenCaptureAccess` first (no prompt) and then
ScreenCaptureKit's `SCScreenshotManager` for the whole display at its backing size, cut to the region in Rust; it needs macOS 14 (earlier is `unavailable`, not an `SCStream` fallback). `input` asks `enigo` (CGEvents at the
HID tap) after enigo's Accessibility check, plans every event before posting one, and puts a pixel on the whole point that holds it (`pixel / scale` rounded down, plus the display's place); the event source is kept, so a held key
stays held between requests. Both refusals say `System Settings, Privacy & Security`, the pane and the program's path, because a grant belongs to the program at its path. The clipboard is the general pasteboard through
`arboard`. Keys of the closed list that a Mac lacks (`print_screen`, `menu`, `f21` to `f24`) are `unsupported`; `insert` is the Help key. The pointer is reported by `cursor`, never drawn. `tests/gui_macos.rs` runs its
refusal tests wherever the suite runs (a missing grant or session is what they prove) and its live test, which builds `tests/support/gui_probe.swift` with `xcrun swiftc` and drives only that window, with
`BOHIO_TEST_GUI_LIVE=1`; the live test prints `SKIPPED` with the system's reason when there is no session or no grant.
