# `bohio_client::tools` — API header

Part of the `bohio-client` API header (`../../AGENTS.md` in the crate root).

## Agent tools (`bohio_client::tools`)

What an agent does on a host, as typed JSON calls over any `Open`; `bohio mcp` carries the same tools over the Model Context Protocol (`docs/agents.md`).

```rust
pub struct Tools;                                                  // Debug, Send + Sync; Drop closes the forwards it holds
impl Tools {
    pub fn new(link: Arc<dyn Open>) -> Result<Tools, Error>;       // reads `info` once; a peer without `info` is not an error (`info()` is then None)
    pub fn with_settings(link: Arc<dyn Open>, settings: Settings) -> Result<Tools, Error>;
    pub const fn info(&self) -> Option<&Info>;   pub const fn settings(&self) -> &Settings;
    pub fn availability(&self, spec: &ToolSpec) -> Availability;   // Available | Unavailable(reason): by what `info` says the host has off and what `Settings` allows
    pub fn available(&self) -> Vec<(&'static ToolSpec, Availability)>;
    pub fn call(&self, name: &str, arguments: Value, cancel: &Cancel) -> Result<Output, ToolError>;   // arguments: a JSON object (null is {}); blocks until the tool is done or `cancel` is set
}
pub static REGISTRY: &[ToolSpec];                                  // one entry per tool; a new group of calls is new entries and a function
pub struct ToolSpec { pub name, pub description: &'static str, pub schema: fn() -> Value /* JSON Schema of the arguments */, pub needs: &'static [Need], pub effect: Effect /* ReadOnly | Changes | Runs */, pub run: Runner }
pub struct Output { pub value: Value, pub images: Vec<Image { mime, data }> }
pub struct ToolError { pub code: String, pub message: String, pub detail: Option<Value> }   // Display `code: message`; codes: the host's (`denied`, `not_found`, `exists`, `changed`, `outside_root`, ...) and `bad_request`, `unknown_tool`, `unavailable`, `not_text`, `no_match`, `ambiguous`, `too_large`, `timeout`, `closed`, `link`, `cancelled`
// Desktop calls on Tools (one bohio-gui/1 stream kept per Tools: opened on first use, opened again once after a loss for a read, never again for an input):
impl Tools {
    pub fn screenshot(&self, &ScreenshotOptions) -> Result<Screenshot, ToolError>;   // ScreenshotOptions { display, region, max_width, max_height (Settings when None), format: Option<ShotFormat Auto|Png|Jpeg>, quality, settle: Option<SettleOptions> (Default: Some) }
    //   Screenshot { data, mime, display, width, height, region, scale (picture px per display px), cursor (picture px), settled }; remembers region + scale per display for the calls below
    pub fn gui_displays(&self) -> Result<Displays, ToolError>;
    pub fn click(&self, &Pointer, Button, count: u32) -> Result<Done, ToolError>;    // Pointer { x: f64, y: f64, display: Option<DisplayId>, space: Space::Screenshot (default) | Display }; Done { applied, screenshot, screenshot_error }
    pub fn move_to(&self, &Pointer) -> Result<Done, ToolError>;   pub fn drag(&self, from: &Pointer, to: &Pointer, Button) -> Result<Done, ToolError>;
    pub fn scroll(&self, at: Option<&Pointer>, dx: i32, dy: i32) -> Result<Done, ToolError>;   pub fn type_text(&self, &str) -> Result<Done, ToolError>;   pub fn key(&self, combo: &str) -> Result<Done, ToolError>;
    pub fn wait_settled(&self, Option<&DisplayId>, SettleOptions) -> Result<Waited, ToolError>;   pub fn cursor(&self) -> Result<CursorPosition, ToolError>;   // a 1x1 snapshot
    pub fn clipboard_read(&self) -> Result<String, ToolError>;   pub fn clipboard_write(&self, &str) -> Result<(), ToolError>;
}   // errors besides the host's: `out_of_bounds` (message and detail name the bounds), `no_screenshot`, `stale_screenshot` (the display changed size), `too_large`
pub struct Cancel;   impl Cancel { pub fn new() -> Self; pub fn cancel(&self); pub fn is_cancelled(&self) -> bool }   // Clone: a running exec is killed, a read or watch stops
pub struct Settings { reply_timeout /*30 s*/, transfer_timeout /*5 min*/, output: Bound /*64 KiB, ceiling 4 MiB*/, lines /*2000, 100 000*/, list /*200, 10 000*/, walk /*1000, 100 000*/, follow /*5000 ms, 300 000*/, bundle /*1 MiB, 16 MiB*/,
    max_text /*8 MiB: the largest edit and write*/, exec_deadline_ms /*120 000; 0 none*/, exec_grace /*15 s*/, poll /*50 ms*/, session_quiet_ms /*300*/, local_roots: Vec<PathBuf> /*none: put and get unavailable*/,
    forward_local_targets: Vec<String> /*none: forward_remote unavailable*/, forward_allow_remote_bind /*false*/, max_forwards /*16*/, forward: ForwardSettings, gui_timeout /*120 s*/, gui_max_image /*4 MiB: the most a screenshot returns*/, gui_auto_limit /*1 MiB: where `auto` turns to JPEG*/, gui_jpeg_quality /*80*/,
    gui_max_width /*1280*/, gui_max_height /*800*/, gui_after_action /*false: an input answers with a settled screenshot*/ }   // Default, non_exhaustive; Bound { default, ceiling }
pub fn base64_encode(&[u8]) -> String;   pub fn base64_decode(&str) -> Option<Vec<u8>>;
```

Tools and what each needs on the host (`needs` is also checked here: `info` says a host has files, programs, sessions, forwards or the desktop off, and `Settings` allows the local-machine tools): `info` (info); `exec` (exec; `pty` needs shell); `read`, `list`, `stat`, `hash`, `walk`, `glob`, `search`, `follow`, `read_many` (fs.read); `stats`, `which` (info); `write`, `write_many`, `move`, `copy`, `truncate`, `mkdir`, `remove` (fs.write; `write` with `if_sha256` also fs.read); `edit` (fs.read and fs.write); `put`, `get` (fs.write / fs.read, and `local_roots`); `session_start` (session and exec), `session_read`, `session_write`, `session_signal`, `session_list`, `session_end` (session); `forward_local` (forward.connect), `forward_remote` (forward.listen and `forward_local_targets`), `forward_list`, `forward_close` (none); the desktop: `screenshot`, `wait`, `displays`, `cursor_position`, `gui_watch` (gui.view), `click`, `move_pointer`, `drag`, `scroll`, `type`, `key` (gui.input), `clipboard_read`, `clipboard_write` (gui.clipboard; a stream with it also holds `view` or `input`); `ToolSpec::is_gui()` says which tools these are, and `bohio mcp` lists them only with `--gui`. A refusal by the host is a `denied` error that names what the tool needs.

Bounds: `exec` keeps the first and last halves of each output stream within `max_output` (`Capture`: `{ encoding: utf8|base64, data, tail?, total_bytes, omitted_bytes, truncated }`, cut between characters of text); `read` pages by `offset_line`, `limit_lines` and `max_bytes` (`more`, `next_offset_line`, `total_lines` once the end was reached, `sha256` only when the whole file was read, `line_cut` for a line longer than the bound); `list`, `walk`, `glob`, `search` return a page and a cursor (`search`: `skip`, `limit`, `more`, `next_skip`, and the host's closing counts in `done`); `follow` watches for `duration_ms` (`Settings::follow`, default 5 s, ceiling 5 minutes) and keeps its output as `exec` does, with each cut or rotation in `truncations`; `read_many` has a byte budget and lists a file with no room with its size; `session_read` returns up to `max_output` bytes from an offset and `next`. A request above a ceiling is lowered to it. `edit` reads the file (at most `max_text`), counts `old` (`no_match`, `ambiguous` with the count) and writes with a `base` of the size, time and SHA-256 it read, so the host answers `changed` when a second writer moved the file. `session_start` starts detached (`detach: true`); `session_read` attaches from `from` and drops the stream, so the session keeps running. Local forwards stop accepting on close; connections they carry run to their end.
